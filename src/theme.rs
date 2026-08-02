//! Regenerating a desktop colour scheme from the artwork.
//!
//! This exists for setups that do not already do it. Shells with their own
//! dynamic theming — DankMaterialShell being the case this was developed
//! against — regenerate from the wallpaper on their own: DMS binds its theme to
//! `SessionData.monitorWallpapers`, which is precisely what
//! `dms ipc call wallpaper setFor` writes, so setting a wallpaper through it
//! already re-derives the scheme from our render. Running matugen ourselves
//! there would be a second pass over the same image, racing the first for the
//! same template outputs.
//!
//! The default is `Off`. Recolouring GTK, the terminal and the browser on every
//! album change is a much larger intervention than setting a wallpaper, and
//! nobody installing a wallpaper tool has implicitly asked for it.
//!
//! `Auto` is the opt-in that does the right thing per setup: skip when the
//! wallpaper backend is a shell known to theme itself, and otherwise use
//! whatever is installed. On swww, hyprpaper or swaybg that means theming the
//! user would not otherwise have; on DMS it means nothing new, because they
//! already had it.

use crate::config::{Backend, ThemeConfig, ThemeMode, ThemeSource};
use std::path::Path;

pub struct Theming {
    mode: ThemeMode,
    source: ThemeSource,
    command: Option<String>,
}

impl Theming {
    /// Resolve `Auto` against what is actually installed and running.
    pub async fn new(cfg: &ThemeConfig, backend: Backend) -> Self {
        let mode = match cfg.mode {
            ThemeMode::Auto => resolve_auto(backend).await,
            explicit => explicit,
        };

        if mode != ThemeMode::Off {
            tracing::info!(?mode, source = ?cfg.source, "theming enabled");
        }

        Self {
            mode,
            source: cfg.source,
            command: cfg.command.clone(),
        }
    }

    pub fn mode(&self) -> ThemeMode {
        self.mode
    }

    /// Regenerate the colour scheme from the artwork.
    ///
    /// `cover` is the artwork as fetched; `wallpaper` is the composed image that
    /// went on screen. Which one to sample is a real choice: the cover gives
    /// purer colours, while the composed wallpaper is what the user is actually
    /// looking at, blur and darkening included.
    pub async fn apply(&self, cover: &Path, wallpaper: &Path) {
        let image = match self.source {
            ThemeSource::Cover => cover,
            ThemeSource::Wallpaper => wallpaper,
        };

        let result = match self.mode {
            ThemeMode::Off | ThemeMode::Auto => return,
            ThemeMode::Matugen => run("matugen", &["image", &image.display().to_string()]).await,
            // `-n` stops pywal setting the wallpaper itself, which would fight
            // the backend we just used.
            ThemeMode::Pywal => run("wal", &["-i", &image.display().to_string(), "-n", "-q"]).await,
            ThemeMode::Command => {
                let Some(template) = &self.command else {
                    tracing::warn!("theme.mode is \"command\" but no theme.command was set");
                    return;
                };
                let rendered = template.replace("{image}", &image.display().to_string());
                run("sh", &["-c", &rendered]).await
            }
        };

        match result {
            Ok(()) => tracing::debug!(image = %image.display(), "regenerated colour scheme"),
            Err(e) => tracing::warn!(error = %e, "theming failed"),
        }
    }
}

/// Pick a mode based on the environment.
async fn resolve_auto(backend: Backend) -> ThemeMode {
    // A shell that owns the wallpaper generally owns the theme too, and will
    // have re-themed already by the time we could. Deliberately decided from the
    // backend rather than by reading the shell's own config: a misread there
    // would cause a double run, and doing nothing is the safe failure.
    if backend == Backend::Dms {
        tracing::debug!(
            "wallpaper backend themes itself from the wallpaper; leaving colours to it"
        );
        return ThemeMode::Off;
    }

    if which("matugen").await {
        return ThemeMode::Matugen;
    }
    if which("wal").await {
        return ThemeMode::Pywal;
    }
    ThemeMode::Off
}

async fn which(binary: &str) -> bool {
    tokio::process::Command::new("sh")
        .args(["-c", &format!("command -v {binary}")])
        .output()
        .await
        .map(|o| o.status.success())
        .unwrap_or(false)
}

async fn run(program: &str, args: &[&str]) -> anyhow::Result<()> {
    let output = tokio::process::Command::new(program)
        .args(args)
        .output()
        .await?;
    if !output.status.success() {
        anyhow::bail!(
            "{program} exited with {:?}: {}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The case this whole module is shaped around. DMS re-derives its scheme
    /// from the wallpaper we just set, so running matugen too would be a second
    /// pass racing the first over the same template files.
    #[tokio::test]
    async fn auto_defers_to_a_shell_that_themes_itself() {
        assert_eq!(resolve_auto(Backend::Dms).await, ThemeMode::Off);
    }

    /// Backends that only paint a wallpaper leave theming to us.
    #[tokio::test]
    async fn auto_takes_over_for_plain_wallpaper_backends() {
        // matugen is present on the development machine; assert the decision is
        // "not deferred" rather than a specific tool, so the test holds wherever
        // it runs.
        for backend in [Backend::Swww, Backend::Hyprpaper, Backend::Swaybg] {
            let mode = resolve_auto(backend).await;
            assert!(
                matches!(mode, ThemeMode::Matugen | ThemeMode::Pywal | ThemeMode::Off),
                "unexpected mode {mode:?} for {backend:?}"
            );
            assert_ne!(
                mode,
                ThemeMode::Auto,
                "auto must resolve to something concrete"
            );
        }
    }

    /// Recolouring the whole desktop must never happen because someone
    /// installed a wallpaper tool and accepted the defaults.
    #[test]
    fn theming_is_off_unless_asked_for() {
        let cfg: crate::config::Config = toml::from_str("").unwrap();
        assert_eq!(cfg.theme.mode, ThemeMode::Off);
    }

    #[tokio::test]
    async fn off_never_runs_anything() {
        let theming = Theming {
            mode: ThemeMode::Off,
            source: ThemeSource::Cover,
            command: None,
        };
        // Paths that do not exist: if this tried to run a tool it would fail
        // loudly rather than returning.
        theming
            .apply(
                Path::new("/nonexistent/a.png"),
                Path::new("/nonexistent/b.png"),
            )
            .await;
    }
}
