//! hypr-music-bg — set the currently playing track's album art as your wallpaper.

mod art;
mod backend;
mod cache;
mod compose;
mod config;
mod control;
mod matching;
mod monitors;
mod mpris;
mod track;
mod tray;
mod util;

use anyhow::{Context, Result};
use art::Resolver;
use backend::Wallpaper;
use cache::Cache;
use clap::{Parser, Subcommand};
use config::{Config, Layout, RenderConfig, RenderStyle};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::RwLock;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use track::{PlaybackStatus, TrackInfo};

type LogFilterHandle =
    tracing_subscriber::reload::Handle<tracing_subscriber::EnvFilter, tracing_subscriber::Registry>;

static LOG_FILTER: OnceLock<LogFilterHandle> = OnceLock::new();

/// Swap the log filter on a running daemon.
///
/// Validated by hand before parsing, because `EnvFilter` is deliberately
/// permissive: it treats an unrecognised word as a *target* name rather than
/// rejecting it, so `parse()` alone accepts nonsense like "not a level" and
/// silently installs a filter that matches nothing.
fn set_log_level(level: &str) -> Result<()> {
    const LEVELS: [&str; 6] = ["trace", "debug", "info", "warn", "error", "off"];

    let level = level.trim();
    if level.is_empty() {
        anyhow::bail!("no log level given");
    }
    if level.split_whitespace().count() > 1 {
        anyhow::bail!(
            "{level:?} is not a valid log filter (no spaces; use commas to separate directives)"
        );
    }
    // A bare word has to be a level. Anything with `=` is a target directive,
    // which we let EnvFilter judge.
    if !level.contains('=') && !LEVELS.contains(&level.to_ascii_lowercase().as_str()) {
        anyhow::bail!(
            "{level:?} is not a log level (expected one of {})",
            LEVELS.join(", ")
        );
    }

    let handle = LOG_FILTER.get().context("log filter not initialised")?;
    let filter: tracing_subscriber::EnvFilter = level
        .parse()
        .with_context(|| format!("{level:?} is not a valid log filter"))?;
    handle.reload(filter).context("reloading log filter")?;
    Ok(())
}

#[derive(Parser)]
#[command(name = "hypr-music-bg", version, about)]
struct Cli {
    /// Config file. Defaults to $XDG_CONFIG_HOME/hypr-music-bg/config.toml.
    #[arg(short, long)]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Follow the active player and update the wallpaper. The default.
    Run,
    /// Resolve and apply art once for what is playing now, then exit.
    Once {
        /// Render the wallpapers and print their paths without applying them.
        #[arg(long)]
        dry_run: bool,
    },
    /// Show what the source chain returns for the current track without
    /// touching the wallpaper.
    Probe,
    /// Print detected monitors, players, sources and the wallpaper backend.
    Doctor,

    // --- these talk to a running daemon over the control socket ---
    /// Show what the running daemon is doing.
    Status,
    /// Enable or disable art updates without stopping the daemon.
    Toggle,
    Enable,
    Disable,
    /// Re-resolve and re-apply art for the current track.
    Refresh {
        /// Ignore the remembered winner and walk the chain again.
        #[arg(long)]
        bypass_cache: bool,
    },
    /// Put back the pre-daemon wallpaper.
    Restore,
    /// Re-read the config file without restarting.
    Reload,
    /// Delete cached art and renders, keeping the saved original wallpaper.
    ClearCache,
    /// Change the log filter on a running daemon, e.g. `debug`.
    LogLevel {
        level: String,
    },
    /// Ask the daemon to restore and exit.
    Quit,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // HTTP/2 and TLS internals log a frame per packet at debug, which buries
    // everything this daemon actually has to say. Pin them at warn so
    // `HMB_LOG=debug` stays readable.
    let mut filter = tracing_subscriber::EnvFilter::try_from_env("HMB_LOG")
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    for noisy in [
        "h2=warn",
        "hyper=warn",
        "hyper_util=warn",
        "rustls=warn",
        "reqwest=warn",
    ] {
        if let Ok(directive) = noisy.parse() {
            filter = filter.add_directive(directive);
        }
    }

    // Wrapped in a reload layer so `hypr-music-bg log-level debug` can change
    // verbosity on a running daemon instead of requiring a restart.
    let (filter_layer, reload_handle) = tracing_subscriber::reload::Layer::new(filter);
    tracing_subscriber::registry()
        .with(filter_layer)
        .with(tracing_subscriber::fmt::layer().with_target(false))
        .init();
    LOG_FILTER.set(reload_handle).ok();

    let command = cli.command.unwrap_or(Command::Run);

    // Client subcommands must not load config or touch the cache: they only
    // forward a message to whatever daemon is already running.
    if let Some(forwarded) = as_control_command(&command) {
        return control_client(forwarded).await;
    }

    let cfg = Config::load(cli.config.as_deref())?;
    let config_path = cli.config.clone();

    match command {
        Command::Run => run(cfg, config_path).await,
        Command::Once { dry_run } => once(cfg, config_path, dry_run).await,
        Command::Probe => probe(cfg).await,
        Command::Doctor => doctor(cfg).await,
        _ => unreachable!("client subcommands are forwarded above"),
    }
}

/// Map a CLI subcommand onto a control-socket command, if it is one.
fn as_control_command(command: &Command) -> Option<control::Command> {
    Some(match command {
        Command::Status => control::Command::Status,
        Command::Toggle => control::Command::Toggle,
        Command::Enable => control::Command::Enable,
        Command::Disable => control::Command::Disable,
        Command::Refresh { bypass_cache } => control::Command::Refresh {
            bypass_cache: *bypass_cache,
        },
        Command::Restore => control::Command::Restore,
        Command::Reload => control::Command::Reload,
        Command::ClearCache => control::Command::ClearCache,
        Command::LogLevel { level } => control::Command::LogLevel {
            level: level.clone(),
        },
        Command::Quit => control::Command::Quit,
        Command::Run | Command::Once { .. } | Command::Probe | Command::Doctor => return None,
    })
}

/// The parts of the daemon derived from the config file.
///
/// Grouped behind one lock so `reload` can rebuild them together: the source
/// chain and the wallpaper backend both depend on config, and swapping them
/// independently could leave the two briefly disagreeing.
struct Live {
    cfg: Config,
    resolver: Resolver,
    wallpaper: Wallpaper,
}

/// Everything the event loop needs.
struct App {
    live: RwLock<Live>,
    cache: Cache,
    /// What each monitor was showing before we touched anything, so stopping
    /// playback can put back exactly that.
    original: HashMap<String, PathBuf>,
    /// Published for the CLI, the tray and the GUI.
    ///
    /// A std lock, not a tokio one: ksni's `Tray` methods are synchronous and
    /// cannot await. Nothing holds this guard across an await point.
    status: std::sync::RwLock<control::Status>,
    /// False means stay running but stop reacting to the player.
    enabled: AtomicBool,
    /// Remembered so `reload` reads the same file the daemon started with.
    config_path: Option<PathBuf>,
    /// The last track seen, so `refresh` can re-resolve without the player
    /// having to emit anything.
    last_track: RwLock<Option<TrackInfo>>,
    /// Raised by `quit`, awaited by the run loop.
    shutdown: tokio::sync::Notify,
    /// Bumped on any state change, so the tray can refresh its icon and menu.
    ///
    /// Not just artwork: the menu shows the enabled checkmark, the render radio
    /// groups and the log level too, and dbusmenu clients cache the layout until
    /// its revision changes. Anything altering Status must bump this.
    state_generation: tokio::sync::watch::Sender<u64>,
}

impl App {
    async fn new(cfg: Config, config_path: Option<PathBuf>) -> Result<Self> {
        let cache = Cache::new(cfg.cache_dir())?;
        let resolver = Resolver::new(&cfg.art, cache.clone())?;
        let wallpaper = Wallpaper::new(&cfg.wallpaper).await?;

        // Snapshot the wallpaper that was up before we ever touched anything.
        //
        // Persisted, and never overwritten with one of our own renders: after a
        // previous run the backend reports *our* cover as the current wallpaper,
        // so re-capturing blindly would lose the real one permanently and make
        // restore put back a stale album cover.
        let mut original = cache.load_originals();
        if let Ok(monitors) = monitors::detect().await {
            for m in monitors {
                let Some(path) = wallpaper.current(&m.name).await else {
                    continue;
                };
                if cache.is_ours(&path) {
                    tracing::debug!(
                        monitor = %m.name,
                        "current wallpaper is one of ours; keeping the stored original"
                    );
                    continue;
                }
                tracing::debug!(monitor = %m.name, path = %path.display(), "captured original wallpaper");
                original.insert(m.name, path);
            }
        }
        cache.save_originals(&original);

        if original.is_empty() {
            tracing::warn!(
                "no original wallpaper known, so restore has nothing to put back. \
                 Set art.fallback_wallpaper, or set a wallpaper before starting the daemon"
            );
        }

        let status = control::Status {
            enabled: true,
            version: env!("CARGO_PKG_VERSION").to_string(),
            playback: "Unknown".into(),
            min_resolution: cfg.art.min_resolution,
            render_style: render_style_name(cfg.render.style),
            render_layout: layout_name(cfg.render.layout),
            backend: format!("{:?}", wallpaper.backend()),
            source_chain: resolver
                .source_names()
                .iter()
                .map(|s| s.to_string())
                .collect(),
            log_level: std::env::var("HMB_LOG").unwrap_or_else(|_| "info".into()),
            ..Default::default()
        };

        Ok(Self {
            live: RwLock::new(Live {
                cfg,
                resolver,
                wallpaper,
            }),
            cache,
            original,
            status: std::sync::RwLock::new(status),
            enabled: AtomicBool::new(true),
            config_path,
            last_track: RwLock::new(None),
            shutdown: tokio::sync::Notify::new(),
            state_generation: tokio::sync::watch::channel(0).0,
        })
    }

    fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }

    /// Tell anything mirroring `Status` that it changed.
    fn notify_changed(&self) {
        let next = *self.state_generation.borrow() + 1;
        self.state_generation.send_replace(next);
    }

    /// Resolve art for a track, render it per monitor, and apply it.
    async fn apply_for_track(&self, track: &TrackInfo, dry_run: bool) -> Result<()> {
        tracing::info!(
            player = %track.player,
            artist = %track.search_artist(),
            album = %track.album,
            "album changed"
        );

        *self.last_track.write().await = Some(track.clone());
        {
            let mut status = self.status.write().unwrap();
            status.playback = "Playing".into();
            status.player = Some(track.player.clone());
            status.artist = Some(track.search_artist().to_string());
            status.album = Some(track.album.clone());
            status.title = Some(track.title.clone());
        }

        let live = self.live.read().await;
        let resolution = live.resolver.resolve(track).await;
        let min_resolution = live.cfg.art.min_resolution;
        let render_cfg = live.cfg.render.clone();
        drop(live);

        {
            let mut status = self.status.write().unwrap();
            status.chain = resolution.outcomes.clone();
            status.art = resolution.art.as_ref().map(|art| control::ArtStatus {
                source: art.source.clone(),
                width: art.width,
                height: art.height,
                bytes: art.bytes.len() as u64,
                degraded: art.degraded,
                cache_path: Some(self.cache.art_path(&art.locator_key).display().to_string()),
            });
        }

        let (bytes, label) = match resolution.art {
            Some(art) => {
                if art.degraded {
                    tracing::warn!(
                        source = %art.source,
                        width = art.width,
                        height = art.height,
                        min = min_resolution,
                        "using degraded art: no source met min_resolution"
                    );
                }
                (art.bytes, art.source.clone())
            }
            None => match self.fallback_bytes().await {
                Some(bytes) => {
                    // A fallback is an ordinary wallpaper, not a cover: render it
                    // plainly rather than blurring it and pasting a shrunken copy
                    // of itself in the middle.
                    return self
                        .render_and_apply_with(
                            &bytes,
                            "fallback",
                            &plain_render_config(&bytes, &render_cfg),
                            dry_run,
                        )
                        .await;
                }
                None => {
                    tracing::warn!("no art and no fallback; leaving wallpaper alone");
                    return Ok(());
                }
            },
        };

        self.notify_changed();
        self.render_and_apply(&bytes, &format!("{}|{label}", track.album_key()), dry_run)
            .await
    }

    async fn render_and_apply(
        &self,
        art_bytes: &[u8],
        cache_key: &str,
        dry_run: bool,
    ) -> Result<()> {
        let cfg = self.live.read().await.cfg.render.clone();
        self.render_and_apply_with(art_bytes, cache_key, &cfg, dry_run)
            .await
    }

    async fn render_and_apply_with(
        &self,
        art_bytes: &[u8],
        cache_key: &str,
        render_cfg: &RenderConfig,
        dry_run: bool,
    ) -> Result<()> {
        let monitors = monitors::detect().await?;

        // Rendering is CPU-bound and would otherwise stall the D-Bus loop.
        let art = art_bytes.to_vec();
        let render_cfg = render_cfg.clone();
        let targets = monitors.clone();
        let rendered =
            tokio::task::spawn_blocking(move || compose::render(&art, &targets, &render_cfg))
                .await
                .context("render task panicked")??;

        for item in rendered {
            let path = self
                .cache
                .render_path(&format!("{cache_key}|{}", item.monitor));
            cache::write_atomic(&path, &item.png)
                .with_context(|| format!("writing {}", path.display()))?;

            if dry_run {
                println!("{}  {}", item.monitor, path.display());
                continue;
            }

            match self
                .live
                .read()
                .await
                .wallpaper
                .apply(&item.monitor, &path)
                .await
            {
                Ok(()) => {
                    tracing::info!(monitor = %item.monitor, "wallpaper set");
                    self.status
                        .write()
                        .unwrap()
                        .rendered
                        .insert(item.monitor.clone(), path.display().to_string());
                }
                Err(e) => {
                    tracing::error!(monitor = %item.monitor, error = %e, "failed to set wallpaper");
                    self.status.write().unwrap().last_error = Some(e.to_string());
                }
            }
        }

        Ok(())
    }

    /// Read the configured fallback image, picking one if a directory was given.
    async fn fallback_bytes(&self) -> Option<Vec<u8>> {
        let configured = {
            let live = self.live.read().await;
            live.cfg.art.fallback_wallpaper.clone()?
        };
        let path = config::expand_tilde(&configured);

        let file = if path.is_dir() {
            let mut entries: Vec<_> = std::fs::read_dir(&path)
                .ok()?
                .flatten()
                .map(|e| e.path())
                .filter(|p| {
                    p.extension().and_then(|e| e.to_str()).is_some_and(|e| {
                        matches!(
                            e.to_ascii_lowercase().as_str(),
                            "jpg" | "jpeg" | "png" | "webp"
                        )
                    })
                })
                .collect();
            if entries.is_empty() {
                return None;
            }
            entries.sort();
            // No RNG dependency: the clock gives enough variety here.
            let index = (util::now_secs() as usize) % entries.len();
            entries.swap_remove(index)
        } else {
            path
        };

        match tokio::fs::read(&file).await {
            Ok(bytes) => Some(bytes),
            Err(e) => {
                tracing::warn!(path = %file.display(), error = %e, "cannot read fallback wallpaper");
                None
            }
        }
    }

    /// Put back what was on screen before the daemon started.
    ///
    /// Restoring the captured original is better than re-rendering a configured
    /// fallback: it returns the desktop to exactly the state the user's shell
    /// had set, with no surprise.
    async fn restore(&self) -> Result<()> {
        if !self.original.is_empty() {
            tracing::info!("restoring the wallpaper that was set at startup");
            for (monitor, path) in &self.original {
                if let Err(e) = self.live.read().await.wallpaper.apply(monitor, path).await {
                    tracing::error!(monitor = %monitor, error = %e, "failed to restore");
                }
            }
            return Ok(());
        }

        let Some(bytes) = self.fallback_bytes().await else {
            tracing::debug!("nothing to restore to; leaving wallpaper alone");
            return Ok(());
        };
        tracing::info!("restoring fallback wallpaper");
        let render_cfg = self.live.read().await.cfg.render.clone();
        self.render_and_apply_with(
            &bytes,
            "fallback",
            &plain_render_config(&bytes, &render_cfg),
            false,
        )
        .await
    }
}

/// Render settings for an ordinary wallpaper rather than a cover.
///
/// Album art is square and gets the blur-backdrop treatment. A wallpaper is
/// already screen-shaped, so it just needs cropping to fit — blurring it and
/// centring a shrunken copy of itself would be nonsense.
///
/// Layout is inferred from the image: a file already sized for the whole desktop
/// (the user's 5120x1440 spanning wallpapers, say) is sliced across monitors,
/// while a single-screen image is applied to each monitor independently.
fn plain_render_config(bytes: &[u8], base: &RenderConfig) -> RenderConfig {
    let mut cfg = base.clone();
    cfg.style = RenderStyle::Fill;
    cfg.darken = 0.0;

    if let Ok((w, h)) = art::probe_dimensions(bytes) {
        // Wider than 2:1 means it was almost certainly authored to span a
        // multi-monitor layout rather than to fill one screen.
        cfg.layout = if h > 0 && (w as f32 / h as f32) > 2.5 {
            Layout::Span
        } else {
            Layout::PerMonitor
        };
    }

    cfg
}

/// Implements the control socket's command set.
#[async_trait::async_trait]
impl control::Controller for App {
    async fn handle(&self, command: control::Command) -> control::Response {
        use control::{Command as C, Response};

        match command {
            C::Status => {
                let mut status = self.status.read().unwrap().clone();
                status.enabled = self.is_enabled();
                status.cache_bytes = directory_size(self.cache.root());
                Response::with_status(status)
            }

            C::Toggle => {
                let now = !self.is_enabled();
                self.set_enabled(now).await;
                Response::ok(if now { "enabled" } else { "disabled" })
            }
            C::Enable => {
                self.set_enabled(true).await;
                Response::ok("enabled")
            }
            C::Disable => {
                self.set_enabled(false).await;
                Response::ok("disabled")
            }

            C::Refresh { bypass_cache } => {
                let Some(track) = self.last_track.read().await.clone() else {
                    return Response::error("no track seen yet");
                };
                if bypass_cache {
                    // Drop the remembered winner so the chain is walked again;
                    // useful when a source has since gained better art.
                    self.cache.forget_lookup(&track.album_key());
                    self.cache.clear_negative(&track.album_key());
                }
                match self.apply_for_track(&track, false).await {
                    Ok(()) => Response::ok("refreshed"),
                    Err(e) => Response::error(e.to_string()),
                }
            }

            C::Restore => match self.restore().await {
                Ok(()) => Response::ok("restored"),
                Err(e) => Response::error(e.to_string()),
            },

            C::Reload => match self.reload().await {
                Ok(()) => Response::ok("config reloaded"),
                Err(e) => Response::error(e.to_string()),
            },

            C::ClearCache => {
                // The originals must survive: they are the only record of the
                // pre-daemon wallpaper, and losing them would make restore
                // permanently impossible.
                let originals = self.cache.load_originals();
                let freed = directory_size(self.cache.root());
                for sub in ["art", "render", "lookup", "negative"] {
                    let dir = self.cache.root().join(sub);
                    if let Ok(entries) = std::fs::read_dir(&dir) {
                        for entry in entries.flatten() {
                            std::fs::remove_file(entry.path()).ok();
                        }
                    }
                }
                self.cache.save_originals(&originals);
                self.status.write().unwrap().cache_bytes = directory_size(self.cache.root());
                self.notify_changed();
                Response::ok(format!("cleared {} KiB", freed / 1024))
            }

            C::LogLevel { level } => match set_log_level(&level) {
                Ok(()) => {
                    self.status.write().unwrap().log_level = level.clone();
                    self.notify_changed();
                    Response::ok(format!("log level set to {level}"))
                }
                Err(e) => Response::error(e.to_string()),
            },

            C::Render { style, layout } => {
                let mut live = self.live.write().await;
                let mut changed = Vec::new();
                if let Some(style) = &style {
                    match style.to_ascii_lowercase().as_str() {
                        "blur" => live.cfg.render.style = RenderStyle::Blur,
                        "fill" => live.cfg.render.style = RenderStyle::Fill,
                        "fit" => live.cfg.render.style = RenderStyle::Fit,
                        other => return Response::error(format!("unknown render style {other:?}")),
                    }
                    changed.push(format!("style={style}"));
                }
                if let Some(layout) = &layout {
                    match layout.to_ascii_lowercase().as_str() {
                        "per_monitor" | "permonitor" => live.cfg.render.layout = Layout::PerMonitor,
                        "span" => live.cfg.render.layout = Layout::Span,
                        other => return Response::error(format!("unknown layout {other:?}")),
                    }
                    changed.push(format!("layout={layout}"));
                }
                let (style_name, layout_name_now) = (
                    render_style_name(live.cfg.render.style),
                    layout_name(live.cfg.render.layout),
                );
                drop(live);
                {
                    let mut status = self.status.write().unwrap();
                    status.render_style = style_name;
                    status.render_layout = layout_name_now;
                }
                self.notify_changed();

                // Re-render immediately so the change is visible rather than
                // waiting for the next album.
                if let Some(track) = self.last_track.read().await.clone() {
                    self.apply_for_track(&track, false).await.ok();
                }
                Response::ok(format!("{} (this session only)", changed.join(", ")))
            }

            C::Restart => {
                // Under systemd, let the supervisor do it: re-execing would
                // orphan the unit's accounting and lose Restart= semantics.
                if std::env::var_os("INVOCATION_ID").is_some() {
                    tracing::info!("restarting via systemd");
                    self.restore().await.ok();
                    match tokio::process::Command::new("systemctl")
                        .args(["--user", "restart", "hypr-music-bg.service"])
                        .spawn()
                    {
                        Ok(_) => Response::ok("restarting via systemd"),
                        Err(e) => Response::error(format!("systemctl failed: {e}")),
                    }
                } else {
                    Response::error(
                        "not running under systemd; stop and start it again, \
                         or install contrib/hypr-music-bg.service",
                    )
                }
            }

            C::Quit => {
                tracing::info!("quit requested over control socket");
                // Signal the run loop rather than calling `std::process::exit`.
                // Exiting from inside a runtime worker deadlocks in the exit
                // handlers, and it skips the socket cleanup on the way out,
                // leaving a stale socket that blocks the next start.
                self.shutdown.notify_waiters();
                Response::ok("shutting down")
            }
        }
    }
}

impl App {
    async fn set_enabled(&self, enabled: bool) {
        self.enabled.store(enabled, Ordering::Relaxed);
        self.status.write().unwrap().enabled = enabled;
        self.notify_changed();
        tracing::info!(enabled, "daemon toggled");
        if !enabled {
            // Disabled means the desktop should not keep showing our art.
            self.restore().await.ok();
        }
    }

    /// Re-read the config file and rebuild everything derived from it.
    async fn reload(&self) -> Result<()> {
        let cfg = Config::load(self.config_path.as_deref())?;
        let resolver = Resolver::new(&cfg.art, self.cache.clone())?;
        let wallpaper = Wallpaper::new(&cfg.wallpaper).await?;

        {
            let mut status = self.status.write().unwrap();
            status.min_resolution = cfg.art.min_resolution;
            status.render_style = render_style_name(cfg.render.style);
            status.render_layout = layout_name(cfg.render.layout);
            status.backend = format!("{:?}", wallpaper.backend());
            status.source_chain = resolver
                .source_names()
                .iter()
                .map(|s| s.to_string())
                .collect();
        }

        // Swapped together, so the chain and the backend never disagree.
        {
            let mut live = self.live.write().await;
            *live = Live {
                cfg,
                resolver,
                wallpaper,
            };
        }
        self.notify_changed();
        tracing::info!("config reloaded");
        Ok(())
    }
}

fn render_style_name(style: RenderStyle) -> String {
    match style {
        RenderStyle::Blur => "blur",
        RenderStyle::Fill => "fill",
        RenderStyle::Fit => "fit",
    }
    .into()
}

fn layout_name(layout: Layout) -> String {
    match layout {
        Layout::PerMonitor => "per_monitor",
        Layout::Span => "span",
    }
    .into()
}

/// Total bytes under a directory, one level deep per subdirectory.
fn directory_size(root: &std::path::Path) -> u64 {
    fn walk(dir: &std::path::Path) -> u64 {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return 0;
        };
        entries
            .flatten()
            .map(|entry| match entry.metadata() {
                Ok(meta) if meta.is_dir() => walk(&entry.path()),
                Ok(meta) => meta.len(),
                Err(_) => 0,
            })
            .sum()
    }
    walk(root)
}

async fn run(cfg: Config, config_path: Option<PathBuf>) -> Result<()> {
    let app = Arc::new(App::new(cfg, config_path).await?);
    {
        let live = app.live.read().await;
        tracing::info!(sources = ?live.resolver.source_names(), "source chain");
    }

    let player_cfg = app.live.read().await.cfg.player.clone();
    let watcher = mpris::MprisWatcher::new(player_cfg).await?;

    // Control socket runs alongside the watcher.
    let control_app = app.clone();
    let control = tokio::spawn(async move {
        if let Err(e) = control::serve(control_app).await {
            tracing::error!(error = %e, "control socket unavailable");
        }
    });

    tray::spawn(app.clone()).await;

    let loop_app = app.clone();
    let watch = async move {
        let app = loop_app;
        watcher
            .watch(move |event| {
                let app = app.clone();
                async move {
                    if !app.is_enabled() {
                        tracing::debug!("disabled; ignoring event");
                        return;
                    }

                    let result = match event {
                        mpris::Event::Album(track) => app.apply_for_track(&track, false).await,
                        mpris::Event::Idle(status) => {
                            app.status.write().unwrap().playback = format!("{status:?}");
                            let behavior = app.live.read().await.cfg.behavior.clone();
                            let should_restore = match status {
                                PlaybackStatus::Paused => behavior.restore_on_pause,
                                PlaybackStatus::Stopped => behavior.restore_on_stop,
                                PlaybackStatus::Playing => false,
                            };
                            if should_restore {
                                app.restore().await
                            } else {
                                Ok(())
                            }
                        }
                    };

                    if let Err(e) = result {
                        tracing::error!(error = %e, "failed to handle event");
                        app.status.write().unwrap().last_error = Some(e.to_string());
                    }
                }
            })
            .await
    };

    let shutdown_app = app.clone();
    tokio::select! {
        result = watch => result?,
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("interrupted, restoring wallpaper");
            app.restore().await.ok();
        }
        _ = async move { shutdown_app.shutdown.notified().await } => {
            // Restore here rather than in the command handler, so the wallpaper
            // is put back exactly once on the way out of the loop.
            tracing::info!("shutting down, restoring wallpaper");
            app.restore().await.ok();
        }
    }

    control.abort();
    std::fs::remove_file(control::socket_path()).ok();
    Ok(())
}

async fn once(cfg: Config, config_path: Option<PathBuf>, dry_run: bool) -> Result<()> {
    let app = App::new(cfg, config_path).await?;
    let player_cfg = app.live.read().await.cfg.player.clone();
    let watcher = mpris::MprisWatcher::new(player_cfg).await?;

    let Some(player) = watcher.active_player().await? else {
        anyhow::bail!("no MPRIS player is running");
    };
    let (track, _status) = watcher.snapshot(&player).await?;
    if track.is_empty() {
        anyhow::bail!("{player} is not reporting any track metadata");
    }

    app.apply_for_track(&track, dry_run).await
}

async fn probe(cfg: Config) -> Result<()> {
    let watcher = mpris::MprisWatcher::new(cfg.player.clone()).await?;
    let Some(player) = watcher.active_player().await? else {
        anyhow::bail!("no MPRIS player is running");
    };
    let (track, status) = watcher.snapshot(&player).await?;

    println!("player:  {player}");
    println!("status:  {status:?}");
    println!("artist:  {}", track.search_artist());
    println!("album:   {}", track.album);
    println!("title:   {}", track.title);
    println!(
        "usable:  {}",
        if track.is_searchable() {
            "yes"
        } else {
            "no - too thin to search a catalogue with"
        }
    );
    println!();

    let cache = Cache::new(cfg.cache_dir())?;
    // Report what the chain does right now, so bypass the caches that would
    // otherwise short-circuit it.
    cache.clear_negative(&track.album_key());
    cache.forget_lookup(&track.album_key());

    let resolver = Resolver::new(&cfg.art, cache)?;
    println!("chain:   {:?}", resolver.source_names());
    println!("min_res: {}px", cfg.art.min_resolution);
    println!();

    let resolution = resolver.resolve(&track).await;
    for outcome in &resolution.outcomes {
        println!("  {:<18} {}", outcome.source, outcome.outcome);
    }
    println!();

    match resolution.art {
        Some(art) => println!(
            "result:  {} {}x{}{}",
            art.source,
            art.width,
            art.height,
            if art.degraded { "   <-- DEGRADED" } else { "" }
        ),
        None => println!("result:  no source produced usable art"),
    }

    Ok(())
}

async fn doctor(cfg: Config) -> Result<()> {
    println!("config:   {}", config::default_config_path().display());
    println!("cache:    {}", cfg.cache_dir().display());
    println!("socket:   {}", control::socket_path().display());
    println!();

    match monitors::detect().await {
        Ok(list) => {
            println!("monitors:");
            for m in &list {
                let (w, h) = m.logical_size();
                println!(
                    "  {:<8} {}x{} at +{},{}  scale {}",
                    m.name, w, h, m.x, m.y, m.scale
                );
            }
            let (_, _, tw, th) = monitors::bounding_box(&list);
            println!("  bounding box: {tw}x{th}");
        }
        Err(e) => println!("monitors: ERROR {e}"),
    }
    println!();

    match mpris::MprisWatcher::new(cfg.player.clone()).await {
        Ok(watcher) => match watcher.active_player().await {
            Ok(Some(player)) => {
                println!("player:   {player}");
                if let Ok((t, status)) = watcher.snapshot(&player).await {
                    println!("  status: {status:?}");
                    println!("  {} - {}", t.search_artist(), t.album);
                    println!("  searchable: {}", t.is_searchable());
                    match t.art_url.as_deref() {
                        // Never print a remote artUrl: on Subsonic servers it
                        // carries the auth token in its query string.
                        Some(u) if u.starts_with("http") => {
                            println!("  artUrl: <remote url, redacted>")
                        }
                        Some(u) => println!("  artUrl: {u}"),
                        None => println!("  artUrl: none"),
                    }
                }
            }
            Ok(None) => println!("player:   none running"),
            Err(e) => println!("player:   ERROR {e}"),
        },
        Err(e) => println!("dbus:     ERROR {e}"),
    }
    println!();

    match Wallpaper::new(&cfg.wallpaper).await {
        Ok(w) => println!("backend:  {:?}", w.backend()),
        Err(e) => println!("backend:  ERROR {e}"),
    }

    let names: Vec<_> = art::build_sources(&cfg.art)
        .iter()
        .map(|s| s.name())
        .collect();
    println!("sources:  {names:?}");
    println!("min_res:  {}px", cfg.art.min_resolution);
    println!(
        "fallback: {}",
        cfg.art
            .fallback_wallpaper
            .as_deref()
            .unwrap_or("(none set)")
    );

    println!();
    match control::send(&control::Command::Status).await {
        Ok(_) => println!("daemon:   running"),
        Err(_) => println!("daemon:   not running"),
    }

    Ok(())
}

/// Send a command to a running daemon and print the reply.
async fn control_client(command: control::Command) -> Result<()> {
    let response = control::send(&command).await?;

    if let Some(status) = response.status {
        print_status(&status);
        return Ok(());
    }

    let message = response.message.unwrap_or_default();
    if response.ok {
        println!("{message}");
        Ok(())
    } else {
        anyhow::bail!(message)
    }
}

fn print_status(s: &control::Status) {
    println!("enabled:  {}", s.enabled);
    println!("version:  {}", s.version);
    println!("playback: {}", s.playback);
    if let Some(player) = &s.player {
        println!("player:   {player}");
    }
    println!(
        "track:    {} - {} - {}",
        s.artist.as_deref().unwrap_or("?"),
        s.album.as_deref().unwrap_or("?"),
        s.title.as_deref().unwrap_or("?")
    );
    println!();

    match &s.art {
        Some(art) => {
            println!("art:");
            println!("  source:     {}", art.source);
            println!("  resolution: {}x{}", art.width, art.height);
            println!("  size:       {} KiB", art.bytes / 1024);
            println!("  degraded:   {}", art.degraded);
            if let Some(path) = &art.cache_path {
                println!("  cached:     {path}");
            }
        }
        None => println!("art:      none resolved"),
    }
    println!();

    if !s.chain.is_empty() {
        println!("chain (min {}px):", s.min_resolution);
        for outcome in &s.chain {
            println!("  {:<18} {}", outcome.source, outcome.outcome);
        }
        println!();
    }

    if !s.rendered.is_empty() {
        println!("rendered:");
        for (monitor, path) in &s.rendered {
            println!("  {monitor:<8} {path}");
        }
        println!();
    }

    println!("backend:  {}", s.backend);
    println!("sources:  {}", s.source_chain.join(", "));
    println!("cache:    {} KiB", s.cache_bytes / 1024);
    println!("log:      {}", s.log_level);
    if let Some(error) = &s.last_error {
        println!("last err: {error}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `EnvFilter::parse` accepts an unrecognised word as a *target* directive,
    /// so validation cannot be left to it: `log-level "not a level"` used to
    /// report success while installing a filter that matched nothing.
    #[test]
    fn rejects_things_that_are_not_log_levels() {
        for bad in ["not a level", "verbose", "", "  ", "loud"] {
            assert!(
                set_log_level(bad).is_err(),
                "{bad:?} should be rejected as a log level"
            );
        }
    }

    #[test]
    fn accepts_levels_and_target_directives() {
        // set_log_level also reloads, which needs an initialised subscriber, so
        // assert on the validation rules rather than the side effect.
        const LEVELS: [&str; 6] = ["trace", "debug", "info", "warn", "error", "off"];
        for good in LEVELS {
            assert!(good.parse::<tracing_subscriber::EnvFilter>().is_ok());
        }
        assert!(
            "hypr_music_bg=debug"
                .parse::<tracing_subscriber::EnvFilter>()
                .is_ok()
        );
    }

    #[test]
    fn client_subcommands_are_forwarded_and_local_ones_are_not() {
        // A client subcommand must not load config or build a cache, so the
        // mapping decides whether the process talks to a daemon at all.
        assert!(as_control_command(&Command::Status).is_some());
        assert!(as_control_command(&Command::Toggle).is_some());
        assert!(as_control_command(&Command::Quit).is_some());
        assert!(as_control_command(&Command::Run).is_none());
        assert!(as_control_command(&Command::Doctor).is_none());
        assert!(as_control_command(&Command::Once { dry_run: true }).is_none());
    }

    #[test]
    fn spanning_wallpapers_are_sliced_and_single_screen_ones_are_not() {
        let base = RenderConfig::default();

        let wide = image::RgbaImage::new(5120, 1440);
        let mut bytes = Vec::new();
        image::DynamicImage::ImageRgba8(wide)
            .write_to(
                &mut std::io::Cursor::new(&mut bytes),
                image::ImageFormat::Png,
            )
            .unwrap();
        let cfg = plain_render_config(&bytes, &base);
        assert_eq!(cfg.layout, Layout::Span);
        // A wallpaper must never get the blur-and-centre-a-copy treatment.
        assert_eq!(cfg.style, RenderStyle::Fill);
        assert_eq!(cfg.darken, 0.0);

        let single = image::RgbaImage::new(2560, 1440);
        let mut bytes = Vec::new();
        image::DynamicImage::ImageRgba8(single)
            .write_to(
                &mut std::io::Cursor::new(&mut bytes),
                image::ImageFormat::Png,
            )
            .unwrap();
        assert_eq!(
            plain_render_config(&bytes, &base).layout,
            Layout::PerMonitor
        );
    }
}
