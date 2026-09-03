//! Applying a rendered image as the wallpaper.
//!
//! Several tools own the background on Wayland and they do not agree on an
//! interface, so each gets a small adapter and `Backend::Auto` probes for
//! whichever is actually running.

use anyhow::{Context, Result, anyhow};
use hypr_music_bg::config::{Backend, WallpaperConfig};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

/// How long a freshly spawned `swaybg` is given to fall over before it is
/// believed. A bad output name makes it exit within a few milliseconds.
const SWAYBG_LIVENESS_GRACE: Duration = Duration::from_millis(100);

pub struct Wallpaper {
    backend: Backend,
    command: Option<String>,
    /// Live `swaybg` processes, keyed by monitor. Empty for every other backend.
    ///
    /// swaybg has no IPC, so changing a wallpaper means replacing the process.
    /// The obvious way to do that — `pkill -x swaybg` before each spawn — is
    /// wrong on more than one monitor: `apply` is called once per monitor, so
    /// the second call kills the first monitor's process and only the last
    /// monitor keeps a wallpaper. Verified against a nested compositor with two
    /// outputs, where the daemon logged "wallpaper set" for both and one output
    /// was black. Tracking a process per monitor is what makes the kill precise.
    swaybg: Mutex<HashMap<String, tokio::process::Child>>,
}

impl Wallpaper {
    pub async fn new(cfg: &WallpaperConfig) -> Result<Self> {
        let backend = match cfg.backend {
            Backend::Auto => detect().await?,
            explicit => explicit,
        };

        if backend == Backend::Command && cfg.command.is_none() {
            return Err(anyhow!(
                "wallpaper.backend is \"command\" but no wallpaper.command was set"
            ));
        }

        // Processes from a previous run are not in our map and would never be
        // replaced, so they would stack under every future spawn. This is the
        // one place a blanket kill is correct: once, before we own anything.
        if backend == Backend::Swaybg {
            let _ = run("pkill", &["-x", "swaybg"]).await;
        }

        tracing::info!(?backend, "wallpaper backend");
        Ok(Self {
            backend,
            command: cfg.command.clone(),
            swaybg: Mutex::new(HashMap::new()),
        })
    }

    pub fn backend(&self) -> Backend {
        self.backend
    }

    /// The wallpaper currently set, if the backend can be asked.
    ///
    /// Captured at startup so playback stopping can put back exactly what was
    /// there, rather than whatever `fallback_wallpaper` happens to name.
    pub async fn current(&self, monitor: &str) -> Option<PathBuf> {
        let raw = match self.backend {
            Backend::Dms => {
                let out = tokio::process::Command::new("dms")
                    .args(["ipc", "call", "wallpaper", "getFor", monitor])
                    .output()
                    .await
                    .ok()?;
                // DMS exits 0 even when it refuses a call — `wallpaper get`
                // answers "ERROR: Per-monitor mode enabled" with status 0 — so
                // the exit code cannot be trusted on its own.
                let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
                if !out.status.success() || text.starts_with("ERROR") || text.is_empty() {
                    tracing::debug!(monitor, response = %text, "backend would not report a wallpaper");
                    return None;
                }
                text
            }
            Backend::Swww => {
                let out = tokio::process::Command::new("swww")
                    .arg("query")
                    .output()
                    .await
                    .ok()?;
                if !out.status.success() {
                    return None;
                }
                // `swww query` prints "<output>: WxH, scale, image: <path>".
                String::from_utf8_lossy(&out.stdout)
                    .lines()
                    .find(|l| l.starts_with(monitor))
                    .and_then(|l| l.rsplit_once("image: "))
                    .map(|(_, path)| path.trim().to_string())?
            }
            _ => return None,
        };

        let path = PathBuf::from(raw);
        path.is_file().then_some(path)
    }

    pub async fn apply(&self, monitor: &str, image: &Path) -> Result<()> {
        let path = image.display().to_string();

        match self.backend {
            Backend::Dms => {
                run(
                    "dms",
                    &["ipc", "call", "wallpaper", "setFor", monitor, &path],
                )
                .await
            }
            Backend::Swww => {
                run(
                    "swww",
                    &[
                        "img",
                        "--outputs",
                        monitor,
                        "--transition-type",
                        "fade",
                        &path,
                    ],
                )
                .await
            }
            Backend::Hyprpaper => {
                // `reload` does preload, set and unload-old in one step, which
                // avoids leaking every cover into hyprpaper's memory over a
                // long listening session.
                run(
                    "hyprctl",
                    &["hyprpaper", "reload", &format!("{monitor},{path}")],
                )
                .await
            }
            Backend::Swaybg => self.apply_swaybg(monitor, &path).await,
            Backend::Command => {
                let template = self
                    .command
                    .as_deref()
                    .ok_or_else(|| anyhow!("no wallpaper.command configured"))?;
                let rendered = template
                    .replace("{image}", &path)
                    .replace("{monitor}", monitor);
                run("sh", &["-c", &rendered]).await
            }
            Backend::Auto => Err(anyhow!("backend should have been resolved at startup")),
        }
    }

    /// Replace only this monitor's `swaybg`, and only once the replacement is
    /// known to be alive.
    ///
    /// The new process is started *before* the old one is killed, so a spawn
    /// that fails leaves the previous wallpaper up instead of opening a gap.
    /// Both briefly draw on the same output; the newer layer surface wins, and
    /// the old one is gone a moment later.
    async fn apply_swaybg(&self, monitor: &str, path: &str) -> Result<()> {
        self.spawn_tracked(monitor, "swaybg", &["-o", monitor, "-i", path, "-m", "fill"])
            .await
    }

    /// Spawn a long-lived renderer for `monitor`, replacing whatever this
    /// backend previously started for that same monitor and leaving every other
    /// monitor's process alone.
    async fn spawn_tracked(&self, monitor: &str, program: &str, args: &[&str]) -> Result<()> {
        let mut child = tokio::process::Command::new(program)
            .args(args)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .with_context(|| format!("spawning {program}"))?;

        // Without this an unusable output name is reported as a wallpaper that
        // was set: `spawn` succeeds, swaybg exits on its own, and the monitor
        // is recorded in `status.rendered` while it is actually black.
        tokio::time::sleep(SWAYBG_LIVENESS_GRACE).await;
        if let Some(status) = child.try_wait().context("checking on the renderer")? {
            return Err(anyhow!(
                "{program} exited immediately with {:?}; is {monitor} a real output?",
                status.code()
            ));
        }

        let previous = self
            .swaybg
            .lock()
            .expect("swaybg process map poisoned")
            .insert(monitor.to_string(), child);

        if let Some(mut old) = previous {
            // Already gone if the user killed it by hand; that is not an error.
            let _ = old.kill().await;
        }

        Ok(())
    }
}

/// Probe for a backend that is actually usable right now.
///
/// Order matters: a shell that owns the background (DMS) will happily coexist
/// with an installed-but-idle `swww` binary, and driving both means they fight
/// over the surface.
async fn detect() -> Result<Backend> {
    if running("dms").await && ipc_ready().await {
        return Ok(Backend::Dms);
    }
    if running("swww-daemon").await {
        return Ok(Backend::Swww);
    }
    if running("hyprpaper").await {
        return Ok(Backend::Hyprpaper);
    }
    if which("swww").await {
        return Ok(Backend::Swww);
    }
    if which("hyprpaper").await {
        return Ok(Backend::Hyprpaper);
    }
    if which("swaybg").await {
        return Ok(Backend::Swaybg);
    }

    Err(anyhow!(
        "no wallpaper backend found. Install swww, hyprpaper or swaybg, \
         or set wallpaper.backend and wallpaper.command explicitly"
    ))
}

/// Confirm the DMS wallpaper target actually answers before committing to it.
///
/// Exit status is useless here — DMS returns 0 for refusals too — so this
/// checks that *something* came back on stdout instead. Note that an `ERROR:`
/// reply still counts as ready: `wallpaper get` legitimately refuses when
/// per-monitor mode is on, which means DMS is running and owns the wallpaper,
/// which is exactly what we are testing for.
async fn ipc_ready() -> bool {
    let Ok(out) = tokio::process::Command::new("dms")
        .args(["ipc", "call", "wallpaper", "get"])
        .output()
        .await
    else {
        return false;
    };
    !String::from_utf8_lossy(&out.stdout).trim().is_empty()
}

async fn running(process: &str) -> bool {
    tokio::process::Command::new("pgrep")
        .args(["-x", process])
        .output()
        .await
        .map(|o| o.status.success())
        .unwrap_or(false)
}

async fn which(binary: &str) -> bool {
    tokio::process::Command::new("sh")
        .args(["-c", &format!("command -v {binary}")])
        .output()
        .await
        .map(|o| o.status.success())
        .unwrap_or(false)
}


async fn run(program: &str, args: &[&str]) -> Result<()> {
    let output = tokio::process::Command::new(program)
        .args(args)
        .output()
        .await
        .with_context(|| format!("running {program}"))?;

    if !output.status.success() {
        return Err(anyhow!(
            "{program} exited with {:?}: {}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn swaybg_backend() -> Wallpaper {
        Wallpaper {
            backend: Backend::Swaybg,
            command: None,
            swaybg: Mutex::new(HashMap::new()),
        }
    }

    /// Tracked *and* still running. `Child::id` keeps answering for a process
    /// that has already died, so a map entry on its own would let a killed
    /// renderer pass for a live one — exactly the failure being guarded here.
    fn live_pids(w: &Wallpaper) -> Vec<(String, u32)> {
        let mut map = w.swaybg.lock().unwrap();
        let mut out = Vec::new();
        for (monitor, child) in map.iter_mut() {
            let pid = child.id().expect("tracked child was already reaped");
            assert!(
                child.try_wait().unwrap().is_none(),
                "{monitor}'s renderer (pid {pid}) is tracked but no longer running"
            );
            out.push((monitor.clone(), pid));
        }
        out.sort();
        out
    }

    /// The bug this fixes: a blanket `pkill -x swaybg` before each spawn meant
    /// applying to a second monitor killed the first monitor's process, so only
    /// the last monitor kept a wallpaper.
    #[tokio::test]
    async fn a_second_monitor_does_not_evict_the_first() {
        let w = swaybg_backend();

        w.spawn_tracked("DP-1", "sleep", &["30"]).await.unwrap();
        w.spawn_tracked("DP-2", "sleep", &["30"]).await.unwrap();

        let live = live_pids(&w);
        assert_eq!(live.len(), 2, "both monitors should still have a renderer");
        assert_eq!(live[0].0, "DP-1");
        assert_eq!(live[1].0, "DP-2");
    }

    /// Re-applying to the same monitor must replace that monitor's process
    /// rather than accumulate one per album change.
    #[tokio::test]
    async fn reapplying_replaces_that_monitors_process() {
        let w = swaybg_backend();

        w.spawn_tracked("DP-1", "sleep", &["30"]).await.unwrap();
        let first = live_pids(&w)[0].1;

        w.spawn_tracked("DP-1", "sleep", &["30"]).await.unwrap();
        let live = live_pids(&w);

        assert_eq!(live.len(), 1, "one monitor, one process");
        assert_ne!(live[0].1, first, "the old process should have been replaced");
    }

    /// The silent half of the bug: `spawn` succeeding is not the same as the
    /// wallpaper being up, and a monitor that is actually black must not be
    /// reported as rendered.
    #[tokio::test]
    async fn a_renderer_that_exits_immediately_is_an_error() {
        let w = swaybg_backend();

        let err = w
            .spawn_tracked("NOPE-1", "false", &[])
            .await
            .expect_err("an immediate exit must not report success");

        assert!(
            err.to_string().contains("exited immediately"),
            "unexpected error: {err}"
        );
        assert!(
            w.swaybg.lock().unwrap().is_empty(),
            "a failed spawn must not be tracked as live"
        );
    }

    /// A failed replacement leaves the previous wallpaper up rather than
    /// blanking the monitor.
    #[tokio::test]
    async fn a_failed_replacement_keeps_the_previous_process() {
        let w = swaybg_backend();

        w.spawn_tracked("DP-1", "sleep", &["30"]).await.unwrap();
        let original = live_pids(&w)[0].1;

        w.spawn_tracked("DP-1", "false", &[]).await.unwrap_err();

        let live = live_pids(&w);
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].1, original, "the working renderer should survive");
    }
}
