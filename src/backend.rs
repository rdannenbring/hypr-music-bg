//! Applying a rendered image as the wallpaper.
//!
//! Several tools own the background on Wayland and they do not agree on an
//! interface, so each gets a small adapter and `Backend::Auto` probes for
//! whichever is actually running.

use crate::config::{Backend, WallpaperConfig};
use anyhow::{Context, Result, anyhow};
use std::path::{Path, PathBuf};

pub struct Wallpaper {
    backend: Backend,
    command: Option<String>,
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

        tracing::info!(?backend, "wallpaper backend");
        Ok(Self {
            backend,
            command: cfg.command.clone(),
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
            Backend::Swaybg => {
                // swaybg has no IPC: it renders for as long as it runs, so the
                // only way to change wallpaper is to replace the process.
                let _ = run("pkill", &["-x", "swaybg"]).await;
                tokio::process::Command::new("swaybg")
                    .args(["-o", monitor, "-i", &path, "-m", "fill"])
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .spawn()
                    .context("spawning swaybg")?;
                Ok(())
            }
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
