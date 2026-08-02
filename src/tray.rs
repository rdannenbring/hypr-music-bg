//! System tray icon and menu, via StatusNotifierItem.
//!
//! There is no XEmbed tray on Wayland, so this speaks the freedesktop
//! `StatusNotifierItem` spec (plus `com.canonical.dbusmenu` for the menu) over
//! D-Bus. `ksni` implements both. Any SNI host picks it up — DankMaterialShell,
//! Waybar's tray module, KDE's, and so on.
//!
//! Two constraints from the protocol shape everything here:
//!
//! 1. `Tray`'s methods are synchronous, so the menu is built from a snapshot of
//!    `Status` behind a std lock. Nothing here may await.
//! 2. `activate` callbacks must not block, or the menu freezes for the user.
//!    Every action therefore hands work to the tokio runtime and returns
//!    immediately, going through the same `Controller` the socket uses.
//!
//! dbusmenu also has no text entry and no sliders, which is why numeric and
//! list settings (`min_resolution`, source order, paths) are not editable here.
//! The menu offers what a menu can express — toggles, fixed choices, readouts —
//! and defers the rest to the config file or the settings GUI.

use crate::App;
use crate::control::{Command, Controller, Status};
use ksni::menu::{CheckmarkItem, RadioGroup, RadioItem, StandardItem, SubMenu};
use ksni::{Icon, MenuItem, ToolTip, Tray, TrayMethods};
use std::sync::Arc;

/// Icon edge in pixels. SNI hosts scale as needed; 64 is a reasonable middle
/// that still looks sharp when a bar renders at 24 or 32.
const ICON_SIZE: u32 = 64;

const RENDER_STYLES: [&str; 3] = ["blur", "fill", "fit"];
const LAYOUTS: [&str; 2] = ["per_monitor", "span"];
const LOG_LEVELS: [&str; 5] = ["error", "warn", "info", "debug", "trace"];

pub struct MusicTray {
    app: Arc<App>,
    runtime: tokio::runtime::Handle,
    /// Rendered from the current cover. Rebuilt when the artwork changes rather
    /// than on every repaint, since `icon_pixmap` takes `&self` and the host may
    /// ask for it often.
    icon: Option<Icon>,
    /// The art path the cached icon was built from, to detect staleness.
    icon_source: Option<String>,
}

impl MusicTray {
    pub fn new(app: Arc<App>, runtime: tokio::runtime::Handle) -> Self {
        Self {
            app,
            runtime,
            icon: None,
            icon_source: None,
        }
    }

    fn status(&self) -> Status {
        self.app.status.read().unwrap().clone()
    }

    /// Fire a control command without blocking the menu.
    fn dispatch(&self, command: Command) {
        let app = self.app.clone();
        self.runtime.spawn(async move {
            let response = app.handle(command).await;
            if !response.ok {
                tracing::warn!(
                    message = response.message.as_deref().unwrap_or("?"),
                    "tray command failed"
                );
            }
        });
    }

    /// Rebuild the icon from the current artwork, if it has changed.
    pub fn refresh_icon(&mut self) {
        let path = self.status().art.and_then(|art| art.cache_path);

        if path == self.icon_source {
            return;
        }
        self.icon_source = path.clone();

        self.icon = path.and_then(|path| match load_icon(&path) {
            Ok(icon) => Some(icon),
            Err(e) => {
                tracing::debug!(path = %path, error = %e, "could not build tray icon from artwork");
                None
            }
        });
    }
}

/// Decode the cover and convert it to the ARGB32 network-byte-order buffer that
/// StatusNotifierItem expects.
fn load_icon(path: &str) -> anyhow::Result<Icon> {
    let image = image::ImageReader::open(path)?
        .with_guessed_format()?
        .decode()?
        .resize_exact(ICON_SIZE, ICON_SIZE, image::imageops::FilterType::Lanczos3)
        .to_rgba8();

    // RGBA -> ARGB, big-endian, which is what the spec means by network order.
    let mut data = Vec::with_capacity((ICON_SIZE * ICON_SIZE * 4) as usize);
    for pixel in image.pixels() {
        let [r, g, b, a] = pixel.0;
        data.extend_from_slice(&[a, r, g, b]);
    }

    Ok(Icon {
        width: ICON_SIZE as i32,
        height: ICON_SIZE as i32,
        data,
    })
}

fn label(text: impl Into<String>) -> MenuItem<MusicTray> {
    StandardItem {
        label: text.into(),
        // Readouts, not actions: shown but not clickable.
        enabled: false,
        ..Default::default()
    }
    .into()
}

fn action(text: impl Into<String>, command: Command) -> MenuItem<MusicTray> {
    StandardItem {
        label: text.into(),
        activate: Box::new(move |tray: &mut MusicTray| tray.dispatch(command.clone())),
        ..Default::default()
    }
    .into()
}

impl Tray for MusicTray {
    fn id(&self) -> String {
        "hypr-music-bg".into()
    }

    fn title(&self) -> String {
        "hypr-music-bg".into()
    }

    fn category(&self) -> ksni::Category {
        ksni::Category::ApplicationStatus
    }

    /// Fall back to a themed icon when there is no artwork, or when the daemon
    /// is disabled — a stale cover would suggest it is still working.
    fn icon_name(&self) -> String {
        if self.icon.is_some() && self.app.is_enabled() {
            String::new()
        } else {
            "audio-x-generic".into()
        }
    }

    fn icon_pixmap(&self) -> Vec<Icon> {
        if !self.app.is_enabled() {
            return Vec::new();
        }
        self.icon.clone().into_iter().collect()
    }

    fn tool_tip(&self) -> ToolTip {
        let status = self.status();
        let title = match (&status.artist, &status.album) {
            (Some(artist), Some(album)) => format!("{artist} — {album}"),
            _ => "Nothing playing".into(),
        };

        let mut lines = Vec::new();
        if let Some(track) = &status.title {
            lines.push(track.clone());
        }
        match &status.art {
            Some(art) => lines.push(format!(
                "{} · {}x{}{}",
                art.source,
                art.width,
                art.height,
                if art.degraded { " · degraded" } else { "" }
            )),
            None => lines.push("no artwork resolved".into()),
        }
        if !status.enabled {
            lines.push("(disabled)".into());
        }

        ToolTip {
            title,
            description: lines.join("\n"),
            icon_name: String::new(),
            icon_pixmap: Vec::new(),
        }
    }

    /// Rebuilt each time the menu opens, so readouts are current.
    fn menu_about_to_show(&mut self) {
        self.refresh_icon();
    }

    fn menu(&self) -> Vec<MenuItem<MusicTray>> {
        let status = self.status();
        let mut items: Vec<MenuItem<MusicTray>> = Vec::new();

        // --- now playing -------------------------------------------------
        match (&status.artist, &status.album) {
            (Some(artist), Some(album)) => {
                items.push(label(format!("{artist} — {album}")));
                if let Some(title) = &status.title {
                    items.push(label(format!("   {title}")));
                }
            }
            _ => items.push(label("Nothing playing")),
        }
        // "Unknown" is the pre-first-event placeholder and reads as a fault
        // when the real situation is simply that no player is running.
        items.push(label(match &status.player {
            Some(_) => format!("   {}", status.playback),
            None => "   No player running".to_string(),
        }));

        if status.art.as_ref().is_some_and(|art| art.degraded) {
            items.push(label(format!(
                "   ⚠ below {}px minimum",
                status.min_resolution
            )));
        }

        items.push(MenuItem::Separator);

        // --- actions -----------------------------------------------------
        items.push(
            CheckmarkItem {
                label: "Enabled".into(),
                checked: status.enabled,
                activate: Box::new(|tray: &mut MusicTray| tray.dispatch(Command::Toggle)),
                ..Default::default()
            }
            .into(),
        );
        items.push(action(
            "Refresh",
            Command::Refresh {
                bypass_cache: false,
            },
        ));
        items.push(action(
            "Re-resolve (ignore cache)",
            Command::Refresh { bypass_cache: true },
        ));
        items.push(action("Restore wallpaper", Command::Restore));
        items.push(MenuItem::Separator);

        // --- artwork detail ----------------------------------------------
        let mut artwork: Vec<MenuItem<MusicTray>> = Vec::new();
        match &status.art {
            Some(art) => {
                artwork.push(label(format!("Source: {}", art.source)));
                artwork.push(label(format!("Resolution: {}x{}", art.width, art.height)));
                artwork.push(label(format!("Size: {} KiB", art.bytes / 1024)));
                artwork.push(label(format!(
                    "Meets {}px floor: {}",
                    status.min_resolution,
                    if art.degraded { "no" } else { "yes" }
                )));
                if let Some(path) = &art.cache_path {
                    artwork.push(label(shorten(path)));
                    let open = path.clone();
                    artwork.push(
                        StandardItem {
                            label: "Open image".into(),
                            activate: Box::new(move |_| open_path(&open)),
                            ..Default::default()
                        }
                        .into(),
                    );
                }
            }
            None => artwork.push(label("No artwork resolved")),
        }
        items.push(
            SubMenu {
                label: "Artwork".into(),
                submenu: artwork,
                ..Default::default()
            }
            .into(),
        );

        // --- the chain walk ---------------------------------------------
        let mut chain: Vec<MenuItem<MusicTray>> = Vec::new();
        if status.chain.is_empty() {
            chain.push(label("Nothing resolved yet"));
        } else {
            for outcome in &status.chain {
                chain.push(label(format!("{}: {}", outcome.source, outcome.outcome)));
            }
        }
        chain.push(MenuItem::Separator);
        chain.push(label(format!(
            "Configured: {}",
            status.source_chain.join(" → ")
        )));
        items.push(
            SubMenu {
                label: "Source chain".into(),
                submenu: chain,
                ..Default::default()
            }
            .into(),
        );

        // --- render settings the menu can actually express ----------------
        let mut render: Vec<MenuItem<MusicTray>> = Vec::new();
        render.push(
            SubMenu {
                label: "Style".into(),
                submenu: vec![
                    RadioGroup {
                        selected: RENDER_STYLES
                            .iter()
                            .position(|s| *s == status.render_style)
                            .unwrap_or(0),
                        select: Box::new(|tray: &mut MusicTray, index| {
                            tray.dispatch(Command::Render {
                                style: RENDER_STYLES.get(index).map(|s| s.to_string()),
                                layout: None,
                            });
                        }),
                        options: RENDER_STYLES
                            .iter()
                            .map(|s| RadioItem {
                                label: (*s).into(),
                                ..Default::default()
                            })
                            .collect(),
                    }
                    .into(),
                ],
                ..Default::default()
            }
            .into(),
        );
        render.push(
            SubMenu {
                label: "Layout".into(),
                submenu: vec![
                    RadioGroup {
                        selected: LAYOUTS
                            .iter()
                            .position(|s| *s == status.render_layout)
                            .unwrap_or(0),
                        select: Box::new(|tray: &mut MusicTray, index| {
                            tray.dispatch(Command::Render {
                                style: None,
                                layout: LAYOUTS.get(index).map(|s| s.to_string()),
                            });
                        }),
                        options: LAYOUTS
                            .iter()
                            .map(|s| RadioItem {
                                label: (*s).into(),
                                ..Default::default()
                            })
                            .collect(),
                    }
                    .into(),
                ],
                ..Default::default()
            }
            .into(),
        );
        render.push(MenuItem::Separator);
        render.push(label("Changes apply to this session only"));
        items.push(
            SubMenu {
                label: "Render".into(),
                submenu: render,
                ..Default::default()
            }
            .into(),
        );

        // --- diagnostics -------------------------------------------------
        let mut diagnostics: Vec<MenuItem<MusicTray>> = Vec::new();
        diagnostics.push(label(format!("Backend: {}", status.backend)));
        diagnostics.push(label(format!("Theming: {}", status.theme_mode)));
        diagnostics.push(label(format!(
            "Player: {}",
            status.player.as_deref().unwrap_or("none")
        )));
        for (monitor, path) in &status.rendered {
            diagnostics.push(label(format!("{monitor}: {}", shorten(path))));
        }
        diagnostics.push(label(format!(
            "Cache: {} MiB",
            status.cache_bytes / 1_048_576
        )));
        match &status.last_error {
            Some(error) => diagnostics.push(label(format!("Last error: {}", shorten(error)))),
            None => diagnostics.push(label("No errors")),
        }
        diagnostics.push(MenuItem::Separator);
        diagnostics.push(
            SubMenu {
                label: "Log level".into(),
                submenu: vec![
                    RadioGroup {
                        selected: LOG_LEVELS
                            .iter()
                            .position(|level| status.log_level.starts_with(level))
                            .unwrap_or(2),
                        select: Box::new(|tray: &mut MusicTray, index| {
                            if let Some(level) = LOG_LEVELS.get(index) {
                                tray.dispatch(Command::LogLevel {
                                    level: (*level).into(),
                                });
                            }
                        }),
                        options: LOG_LEVELS
                            .iter()
                            .map(|level| RadioItem {
                                label: (*level).into(),
                                ..Default::default()
                            })
                            .collect(),
                    }
                    .into(),
                ],
                ..Default::default()
            }
            .into(),
        );
        items.push(
            SubMenu {
                label: "Diagnostics".into(),
                submenu: diagnostics,
                ..Default::default()
            }
            .into(),
        );

        items.push(MenuItem::Separator);

        // --- maintenance -------------------------------------------------
        items.push(action("Reload config", Command::Reload));
        items.push(action(
            format!("Clear cache ({} MiB)", status.cache_bytes / 1_048_576),
            Command::ClearCache,
        ));

        let config_path = crate::config::default_config_path();
        items.push(
            StandardItem {
                label: "Edit config…".into(),
                activate: Box::new(move |_| open_path(&config_path.display().to_string())),
                ..Default::default()
            }
            .into(),
        );

        // --- about --------------------------------------------------------
        //
        // No build comparison here, unlike `about` on the command line: the tray
        // runs *inside* the daemon, so what it reports is by definition the
        // build that is running. There is nothing it could disagree with.
        let mut about: Vec<MenuItem<MusicTray>> = vec![
            label(format!("hypr-music-bg {}", status.build.version)),
            label(format!("commit {}", status.build.commit)),
            label(format!("branch {}", status.build.branch)),
            label(format!("built  {}", status.build.built)),
        ];
        if status.build.commit.ends_with("-dirty") {
            about.push(label("⚠ built from an uncommitted tree"));
        }
        if let Some(exe) = &status.build.exe {
            about.push(MenuItem::Separator);
            about.push(label(shorten(exe)));
            let open = exe.clone();
            about.push(
                StandardItem {
                    label: "Show binary in file manager".into(),
                    activate: Box::new(move |_| {
                        // The parent directory: opening the executable itself
                        // would try to run it.
                        let dir = std::path::Path::new(&open)
                            .parent()
                            .map(|p| p.display().to_string())
                            .unwrap_or_else(|| open.clone());
                        open_path(&dir)
                    }),
                    ..Default::default()
                }
                .into(),
            );
        }
        items.push(
            SubMenu {
                label: "About".into(),
                submenu: about,
                ..Default::default()
            }
            .into(),
        );

        items.push(MenuItem::Separator);
        items.push(action("Restart", Command::Restart));
        items.push(action("Quit", Command::Quit));

        items
    }
}

/// Hand a path or URL to the desktop's default handler.
fn open_path(target: &str) {
    if let Err(e) = std::process::Command::new("xdg-open")
        .arg(target)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        tracing::warn!(target, error = %e, "could not open");
    }
}

/// Trim long paths from the left, since the distinctive part is the end.
fn shorten(text: &str) -> String {
    const MAX: usize = 48;
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= MAX {
        return text.to_string();
    }
    format!(
        "…{}",
        chars[chars.len() - MAX + 1..].iter().collect::<String>()
    )
}

/// Register the tray and keep its icon in step with the artwork.
///
/// Failure is not fatal: a missing SNI host means no tray, not no wallpaper.
pub async fn spawn(app: Arc<App>) {
    let runtime = tokio::runtime::Handle::current();
    let mut tray = MusicTray::new(app.clone(), runtime);
    tray.refresh_icon();

    let handle = match tray.spawn().await {
        Ok(handle) => {
            tracing::info!("tray registered");
            handle
        }
        Err(e) => {
            tracing::warn!(error = %e, "no status notifier host available; continuing without a tray");
            return;
        }
    };

    // Follow the artwork rather than polling for it.
    let mut generations = app.state_generation.subscribe();
    tokio::spawn(async move {
        while generations.changed().await.is_ok() {
            handle
                .update(|tray: &mut MusicTray| tray.refresh_icon())
                .await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shortens_from_the_left_keeping_the_filename() {
        let long = "/home/someone/.cache/hypr-music-bg/render/803acfff598c768a.png";
        let short = shorten(long);
        assert!(short.starts_with('…'));
        assert!(short.ends_with("803acfff598c768a.png"));
        assert!(short.chars().count() <= 48);

        assert_eq!(shorten("short"), "short");
    }

    #[test]
    fn icon_conversion_is_argb_not_rgba() {
        // Getting this backwards is the classic SNI bug: the icon renders with
        // swapped channels and a broken alpha, which looks like a corrupt image
        // rather than a coding error.
        let dir = std::env::temp_dir().join(format!("hmb-icon-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("art.png");

        // Fully opaque pure red.
        let art = image::RgbaImage::from_pixel(8, 8, image::Rgba([255, 0, 0, 255]));
        art.save(&path).unwrap();

        let icon = load_icon(&path.display().to_string()).unwrap();
        std::fs::remove_dir_all(&dir).ok();

        assert_eq!(icon.width, ICON_SIZE as i32);
        assert_eq!(icon.data.len(), (ICON_SIZE * ICON_SIZE * 4) as usize);
        // A=255, R=255, G=0, B=0.
        assert_eq!(&icon.data[0..4], &[255, 255, 0, 0]);
    }
}
