//! Control surface: a Unix socket, the CLI that talks to it, and the status
//! snapshot they exchange.
//!
//! The daemon needs shared state and a command channel for the tray anyway, so
//! exposing them over a socket costs almost nothing extra and makes the same
//! actions reachable from a Hyprland keybind, a waybar module, or a shell
//! script. The wire format is line-delimited JSON specifically so it can be
//! driven by hand:
//!
//! ```text
//! echo '{"command":"status"}' | socat - UNIX-CONNECT:$XDG_RUNTIME_DIR/hypr-music-bg.sock
//! ```

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

/// One instruction for the running daemon.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "kebab-case")]
pub enum Command {
    /// Everything the daemon currently knows, for display.
    Status,
    Toggle,
    Enable,
    Disable,
    /// Re-resolve and re-apply art for whatever is playing.
    Refresh {
        /// Ignore the lookup cache, in case a source now has better art.
        #[serde(default)]
        bypass_cache: bool,
    },
    /// Put back the pre-daemon wallpaper.
    Restore,
    /// Re-read the config file and rebuild the source chain.
    Reload,
    ClearCache,
    /// Change the log filter without restarting.
    LogLevel {
        level: String,
    },
    /// Change how art is composited, for the current session only.
    ///
    /// Session-only is deliberate: the tray can offer a fixed set of choices
    /// but has nowhere to type a value, so persisting these belongs to the
    /// settings GUI, which can rewrite the config file properly.
    Render {
        #[serde(default)]
        style: Option<String>,
        #[serde(default)]
        layout: Option<String>,
    },
    /// Restart the process, via systemd when supervised.
    Restart,
    Quit,
}

/// Also tolerant of unknown and missing fields, for the same version-skew
/// reason as `Status`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Response {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<Status>,
}

impl Response {
    pub fn ok(message: impl Into<String>) -> Self {
        Self {
            ok: true,
            message: Some(message.into()),
            status: None,
        }
    }

    pub fn error(message: impl Into<String>) -> Self {
        Self {
            ok: false,
            message: Some(message.into()),
            status: None,
        }
    }

    pub fn with_status(status: Status) -> Self {
        Self {
            ok: true,
            message: None,
            status: Some(status),
        }
    }
}

/// What the art currently on screen is, and where it came from.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ArtStatus {
    pub source: String,
    pub width: u32,
    pub height: u32,
    pub bytes: u64,
    /// Accepted despite being under `min_resolution`.
    pub degraded: bool,
    /// Cached original, before compositing.
    pub cache_path: Option<String>,
}

/// One source's outcome during the last chain walk.
///
/// The whole design is a fallback chain, so "which source won, and why did the
/// earlier ones not" is the single most useful thing to be able to see.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SourceOutcome {
    pub source: String,
    pub outcome: String,
}

/// A snapshot of the daemon, for the CLI, the tray, and the GUI.
///
/// `serde(default)` on the container is load-bearing, not decoration. This is a
/// wire format between two processes that are updated independently: the daemon
/// keeps running while `cargo build` replaces the binary the CLI invokes, so a
/// newer client routinely talks to an older daemon. Without it, adding a single
/// field makes every client command fail with a parse error against a daemon
/// that predates it — which is exactly what adding `theme_mode` did.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Status {
    pub enabled: bool,
    pub version: String,

    pub player: Option<String>,
    pub playback: String,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub title: Option<String>,

    pub art: Option<ArtStatus>,
    pub chain: Vec<SourceOutcome>,
    pub source_chain: Vec<String>,
    pub min_resolution: u32,
    /// Lowercase names matching the config values, for the tray's radio groups.
    pub render_style: String,
    pub render_layout: String,

    pub backend: String,
    /// Monitor name to the rendered PNG currently applied to it.
    pub rendered: BTreeMap<String, String>,

    pub cache_bytes: u64,
    /// Resolved theming mode: what `auto` actually decided.
    pub theme_mode: String,
    pub log_level: String,
    pub last_error: Option<String>,
}

/// Where the socket lives.
///
/// `$XDG_RUNTIME_DIR` is the correct home: it is user-private, on tmpfs, and
/// cleaned up automatically at logout, so a stale socket cannot outlive the
/// session.
pub fn socket_path() -> PathBuf {
    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| {
            PathBuf::from(format!(
                "/tmp/hypr-music-bg-{}",
                std::env::var("UID").unwrap_or_else(|_| "0".into())
            ))
        });
    dir.join("hypr-music-bg.sock")
}

/// Implemented by the daemon.
#[async_trait]
pub trait Controller: Send + Sync {
    async fn handle(&self, command: Command) -> Response;
}

/// Serve the control socket until the process ends.
pub async fn serve(controller: Arc<dyn Controller>) -> Result<()> {
    let path = socket_path();

    // A socket left behind by a crashed daemon would block binding, so clear it
    // — but only after checking nobody is actually listening, otherwise this
    // would silently steal the socket from a running instance.
    if path.exists() {
        if UnixStream::connect(&path).await.is_ok() {
            anyhow::bail!(
                "another hypr-music-bg is already listening on {}",
                path.display()
            );
        }
        tracing::debug!(path = %path.display(), "removing stale socket");
        std::fs::remove_file(&path).ok();
    }

    let listener = UnixListener::bind(&path)
        .with_context(|| format!("binding control socket {}", path.display()))?;
    tracing::info!(path = %path.display(), "control socket listening");

    loop {
        let (stream, _addr) = match listener.accept().await {
            Ok(pair) => pair,
            Err(e) => {
                tracing::warn!(error = %e, "control socket accept failed");
                continue;
            }
        };

        let controller = controller.clone();
        // One task per connection so a slow client cannot stall the daemon.
        tokio::spawn(async move {
            if let Err(e) = handle_connection(stream, controller).await {
                tracing::debug!(error = %e, "control connection ended");
            }
        });
    }
}

async fn handle_connection(stream: UnixStream, controller: Arc<dyn Controller>) -> Result<()> {
    let (read_half, mut write_half) = stream.into_split();
    let mut lines = BufReader::new(read_half).lines();

    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }

        let response = match serde_json::from_str::<Command>(&line) {
            Ok(command) => {
                tracing::debug!(?command, "control command");
                controller.handle(command).await
            }
            Err(e) => Response::error(format!("could not parse command: {e}")),
        };

        let mut encoded = serde_json::to_vec(&response)?;
        encoded.push(b'\n');
        write_half.write_all(&encoded).await?;
        write_half.flush().await?;
    }

    Ok(())
}

/// Send one command to a running daemon and return its reply.
pub async fn send(command: &Command) -> Result<Response> {
    let path = socket_path();
    let stream = UnixStream::connect(&path).await.with_context(|| {
        format!(
            "no daemon listening on {} (is `hypr-music-bg run` started?)",
            path.display()
        )
    })?;

    let (read_half, mut write_half) = stream.into_split();
    let mut encoded = serde_json::to_vec(command)?;
    encoded.push(b'\n');
    write_half.write_all(&encoded).await?;
    write_half.flush().await?;

    let mut lines = BufReader::new(read_half).lines();
    let reply = lines
        .next_line()
        .await?
        .context("daemon closed the connection without replying")?;

    Ok(serde_json::from_str(&reply)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_round_trip_as_json() {
        let cases = [
            (r#"{"command":"status"}"#, "Status"),
            (r#"{"command":"toggle"}"#, "Toggle"),
            (r#"{"command":"clear-cache"}"#, "ClearCache"),
            (r#"{"command":"log-level","level":"debug"}"#, "LogLevel"),
        ];
        for (json, label) in cases {
            let parsed: Command = serde_json::from_str(json).unwrap_or_else(|e| {
                panic!("{label} should parse from {json}: {e}");
            });
            // Re-encoding must produce something the daemon would accept again.
            let reencoded = serde_json::to_string(&parsed).unwrap();
            serde_json::from_str::<Command>(&reencoded).unwrap();
        }
    }

    #[test]
    fn refresh_defaults_to_using_the_cache() {
        let parsed: Command = serde_json::from_str(r#"{"command":"refresh"}"#).unwrap();
        match parsed {
            Command::Refresh { bypass_cache } => assert!(!bypass_cache),
            other => panic!("expected Refresh, got {other:?}"),
        }
    }

    /// Version skew is the normal case, not an edge case: the daemon keeps
    /// running while a rebuild replaces the binary the CLI invokes.
    #[test]
    fn a_newer_client_can_read_an_older_daemons_status() {
        // A payload from a daemon that predates theme_mode, render_style and
        // render_layout entirely.
        let old = r#"{"enabled":true,"version":"0.1.0","playback":"Playing",
                      "artist":"D12","album":"Devil's Night","min_resolution":600,
                      "backend":"Dms","cache_bytes":1024,"log_level":"info"}"#;

        let status: Status = serde_json::from_str(old).expect("must not fail on missing fields");
        assert!(status.enabled);
        assert_eq!(status.album.as_deref(), Some("Devil's Night"));
        assert_eq!(status.theme_mode, "", "absent fields fall back to defaults");
    }

    /// And the reverse: an older client must survive fields it has never heard
    /// of rather than refusing the whole response.
    #[test]
    fn an_older_client_ignores_fields_it_does_not_know() {
        let future = r#"{"ok":true,"message":"done","some_future_field":42}"#;
        let response: Response = serde_json::from_str(future).expect("must ignore unknown fields");
        assert!(response.ok);
        assert_eq!(response.message.as_deref(), Some("done"));
    }

    #[test]
    fn socket_lives_under_the_runtime_dir() {
        // Correctness matters here: a socket outside XDG_RUNTIME_DIR would not be
        // user-private and would survive logout.
        unsafe { std::env::set_var("XDG_RUNTIME_DIR", "/run/user/1000") };
        assert_eq!(
            socket_path(),
            PathBuf::from("/run/user/1000/hypr-music-bg.sock")
        );
    }
}
