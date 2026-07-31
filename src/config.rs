//! Configuration: TOML on disk, with defaults that work with no config at all.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub player: PlayerConfig,
    #[serde(default)]
    pub art: ArtConfig,
    #[serde(default)]
    pub render: RenderConfig,
    #[serde(default)]
    pub wallpaper: WallpaperConfig,
    #[serde(default)]
    pub behavior: BehaviorConfig,
    #[serde(default)]
    pub theme: ThemeConfig,
}

/// Regenerating a desktop colour scheme from the artwork.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ThemeConfig {
    #[serde(default)]
    pub mode: ThemeMode,
    #[serde(default)]
    pub source: ThemeSource,
    /// For `mode = "command"`. `{image}` is substituted.
    #[serde(default)]
    pub command: Option<String>,
}

#[derive(Debug, Clone, Copy, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ThemeMode {
    /// Do nothing. The default, deliberately.
    ///
    /// Recolouring GTK, the terminal, the browser and everything else on every
    /// album change is a far larger intervention than setting a wallpaper, and
    /// nobody installing a wallpaper tool has asked for it. Opt in.
    #[default]
    Off,
    /// Skip when the wallpaper backend already themes itself, otherwise use
    /// whatever is installed.
    Auto,
    Matugen,
    Pywal,
    Command,
}

#[derive(Debug, Clone, Copy, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ThemeSource {
    /// The artwork as fetched: purer colours.
    #[default]
    Cover,
    /// The composed wallpaper: what is actually on screen, blur included.
    Wallpaper,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlayerConfig {
    /// Bus-name fragments to prefer, best first. Matched case-insensitively.
    #[serde(default)]
    pub prefer: Vec<String>,
    /// Bus-name fragments to never follow. Browsers register on MPRIS and will
    /// otherwise hijack the wallpaper with whatever video tab is open.
    #[serde(default = "default_ignore")]
    pub ignore: Vec<String>,
}

fn default_ignore() -> Vec<String> {
    // `playerctld` is playerctl's proxy daemon: it mirrors whatever *it*
    // considers the active player, using its own notion of "active". Following
    // it means reading metadata second-hand and seeing the same track twice on
    // the bus, with which entry wins depending on ListNames ordering.
    [
        "chromium",
        "chrome",
        "firefox",
        "brave",
        "vlc",
        "mpv",
        "playerctld",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

impl Default for PlayerConfig {
    fn default() -> Self {
        Self {
            prefer: Vec::new(),
            ignore: default_ignore(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtConfig {
    /// Shortest acceptable edge, in pixels. A source returning art below this
    /// is passed over in favour of the next source.
    #[serde(default = "default_min_resolution")]
    pub min_resolution: u32,
    /// Check candidates from search-based sources against the MPRIS metadata.
    #[serde(default = "default_true")]
    pub verify_match: bool,
    /// Similarity in `0.0..=1.0` below which a candidate is rejected.
    #[serde(default = "default_match_threshold")]
    pub match_threshold: f64,
    /// When no source clears `min_resolution`, use the largest art that was
    /// found anyway rather than giving up. Disable to go straight to
    /// `fallback_wallpaper`.
    #[serde(default = "default_true")]
    pub allow_degraded: bool,
    /// Image or directory used when nothing at all validates.
    #[serde(default)]
    pub fallback_wallpaper: Option<String>,
    #[serde(default)]
    pub cache_dir: Option<String>,
    /// Seconds to remember that an album has no art anywhere, so replaying it
    /// does not re-walk the whole chain and burn rate-limited lookups. 0 disables.
    #[serde(default = "default_negative_ttl")]
    pub negative_cache_ttl: u64,
    /// Size budget for cached art and rendered wallpapers, in MiB. 0 disables.
    ///
    /// Rendered wallpapers dominate: a two-monitor 1440p setup writes roughly
    /// 3 MB per album, so an unbounded cache reaches gigabytes over a few
    /// months of listening.
    #[serde(default = "default_cache_max_mb")]
    pub cache_max_mb: u64,
    /// Drop entries older than this many days regardless of the size budget.
    /// 0 disables.
    #[serde(default = "default_cache_max_age_days")]
    pub cache_max_age_days: u64,
    /// Sources in priority order.
    #[serde(default = "default_sources", rename = "source")]
    pub sources: Vec<SourceConfig>,
}

fn default_min_resolution() -> u32 {
    600
}
fn default_match_threshold() -> f64 {
    0.6
}
fn default_negative_ttl() -> u64 {
    60 * 60 * 24
}
fn default_cache_max_mb() -> u64 {
    512
}
fn default_cache_max_age_days() -> u64 {
    30
}
fn default_true() -> bool {
    true
}

fn default_sources() -> Vec<SourceConfig> {
    vec![
        // First: instant, no network, and structurally incapable of returning
        // the wrong album. Falls through on its own if it is too small.
        SourceConfig::Mpris,
        SourceConfig::Local { paths: Vec::new() },
        SourceConfig::CoverArtArchive { size: 1200 },
        SourceConfig::Deezer { size: 1200 },
    ]
}

impl Default for ArtConfig {
    fn default() -> Self {
        Self {
            min_resolution: default_min_resolution(),
            verify_match: true,
            match_threshold: default_match_threshold(),
            allow_degraded: true,
            fallback_wallpaper: None,
            cache_dir: None,
            negative_cache_ttl: default_negative_ttl(),
            cache_max_mb: default_cache_max_mb(),
            cache_max_age_days: default_cache_max_age_days(),
            sources: default_sources(),
        }
    }
}

/// One entry in the source chain.
///
/// Credentials are always taken by *reference* — an env var or a command to
/// run — never inline. A config file you might paste into an issue should
/// never contain a token.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "name", rename_all = "lowercase", deny_unknown_fields)]
pub enum SourceConfig {
    /// The `mpris:artUrl` the player already handed us.
    Mpris,

    /// Cover files on disk, next to the audio or under `paths`.
    Local {
        #[serde(default)]
        paths: Vec<String>,
    },

    /// MusicBrainz release-group lookup, then Cover Art Archive. Keyless.
    ///
    /// `size` picks a pre-rendered thumbnail (250, 500 or 1200). Set it to 0
    /// for the original upload, which is frequently 1000-3000px but is a
    /// larger download and is not size-guaranteed.
    #[serde(rename = "coverartarchive")]
    CoverArtArchive {
        #[serde(default = "default_caa_size")]
        size: u32,
    },

    /// Deezer public catalog. Keyless, caps at 1200px.
    Deezer {
        #[serde(default = "default_deezer_size")]
        size: u32,
    },

    /// Spotify Web API. Needs client credentials; tops out at 640px.
    Spotify {
        client_id_env: String,
        client_secret_env: String,
    },

    /// iTunes Search API. Keyless and resizable, but returns confidently wrong
    /// albums for anything it does not carry, so it is only safe with
    /// `verify_match` on.
    Itunes {
        #[serde(default = "default_itunes_size")]
        size: u32,
    },

    /// Discogs release database. Needs a personal token; around 600px.
    Discogs { token_env: String },

    /// fanart.tv artist backgrounds.
    ///
    /// Note this is *not* album art — it returns artist imagery, already in
    /// 16:9 and produced specifically to be used as a background. That makes it
    /// a better backdrop than a zoomed, blurred square cover, but it will not
    /// change between albums by the same artist. Best used with
    /// `style = "backdrop_only"` alongside a real cover source.
    #[serde(rename = "fanarttv")]
    FanartTv {
        api_key_env: String,
        #[serde(default)]
        require_mbid: bool,
    },

    /// A Subsonic-compatible server (Navidrome, Airsonic, Gonic).
    Subsonic {
        url: String,
        username: String,
        /// Env var holding the password.
        password_env: String,
        #[serde(default)]
        size: Option<u32>,
    },

    /// Escape hatch: run any command that prints an image path on stdout.
    /// Gets `HMB_ARTIST`, `HMB_ALBUM`, `HMB_TITLE` in its environment.
    /// This is how you plug in sacad, beets fetchart, or anything you write.
    Exec {
        command: String,
        #[serde(default)]
        args: Vec<String>,
    },
}

impl SourceConfig {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Mpris => "mpris",
            Self::Local { .. } => "local",
            Self::CoverArtArchive { .. } => "coverartarchive",
            Self::Deezer { .. } => "deezer",
            Self::Spotify { .. } => "spotify",
            Self::Itunes { .. } => "itunes",
            Self::Discogs { .. } => "discogs",
            Self::FanartTv { .. } => "fanarttv",
            Self::Subsonic { .. } => "subsonic",
            Self::Exec { .. } => "exec",
        }
    }
}

fn default_caa_size() -> u32 {
    1200
}
fn default_deezer_size() -> u32 {
    1200
}
fn default_itunes_size() -> u32 {
    1200
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RenderConfig {
    #[serde(default)]
    pub style: RenderStyle,
    /// `per_monitor` centres the cover on each screen independently.
    /// `span` builds one canvas across the whole layout and slices it.
    #[serde(default)]
    pub layout: Layout,
    /// Cover size as a fraction of the screen's shorter edge.
    #[serde(default = "default_cover_scale")]
    pub cover_scale: f32,
    #[serde(default = "default_blur_strength")]
    pub blur_strength: f32,
    /// `0.0..=1.0` darkening applied to the backdrop, for desktop-icon legibility.
    #[serde(default = "default_darken")]
    pub darken: f32,
}

#[derive(Debug, Clone, Copy, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum RenderStyle {
    /// Blurred zoomed cover as backdrop, sharp cover centred on top.
    #[default]
    Blur,
    /// Cover scaled to fill the screen, cropped.
    Fill,
    /// Cover fitted inside the screen, letterboxed against a flat colour.
    Fit,
}

#[derive(Debug, Clone, Copy, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Layout {
    #[default]
    PerMonitor,
    Span,
}

fn default_cover_scale() -> f32 {
    0.55
}
fn default_blur_strength() -> f32 {
    8.0
}
fn default_darken() -> f32 {
    0.25
}

impl Default for RenderConfig {
    fn default() -> Self {
        Self {
            style: RenderStyle::default(),
            layout: Layout::default(),
            cover_scale: default_cover_scale(),
            blur_strength: default_blur_strength(),
            darken: default_darken(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WallpaperConfig {
    #[serde(default)]
    pub backend: Backend,
    /// For `backend = "command"`. `{image}` and `{monitor}` are substituted.
    #[serde(default)]
    pub command: Option<String>,
}

#[derive(Debug, Clone, Copy, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Backend {
    /// Probe for whatever is actually running.
    #[default]
    Auto,
    Dms,
    Swww,
    Hyprpaper,
    Swaybg,
    Command,
}

impl Default for WallpaperConfig {
    fn default() -> Self {
        Self {
            backend: Backend::Auto,
            command: None,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BehaviorConfig {
    #[serde(default)]
    pub restore_on_pause: bool,
    #[serde(default = "default_true")]
    pub restore_on_stop: bool,
}

impl Default for BehaviorConfig {
    fn default() -> Self {
        Self {
            restore_on_pause: false,
            restore_on_stop: true,
        }
    }
}

impl Config {
    pub fn load(path: Option<&Path>) -> Result<Self> {
        let path = match path {
            Some(p) => p.to_path_buf(),
            None => default_config_path(),
        };

        if !path.exists() {
            tracing::info!(path = %path.display(), "no config file, using defaults");
            return Ok(Self::default());
        }

        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        let cfg: Self =
            toml::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?;
        tracing::info!(path = %path.display(), "loaded config");
        Ok(cfg)
    }

    pub fn cache_dir(&self) -> PathBuf {
        self.art
            .cache_dir
            .as_deref()
            .map(expand_tilde)
            .unwrap_or_else(|| base_dir("XDG_CACHE_HOME", ".cache").join("hypr-music-bg"))
    }
}

pub fn default_config_path() -> PathBuf {
    base_dir("XDG_CONFIG_HOME", ".config")
        .join("hypr-music-bg")
        .join("config.toml")
}

fn base_dir(env_var: &str, fallback: &str) -> PathBuf {
    std::env::var_os(env_var)
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| home().join(fallback))
}

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"))
}

pub fn expand_tilde(s: &str) -> PathBuf {
    match s.strip_prefix("~/") {
        Some(rest) => home().join(rest),
        None if s == "~" => home(),
        None => PathBuf::from(s),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_parse_from_empty_document() {
        let cfg: Config = toml::from_str("").unwrap();
        assert_eq!(cfg.art.min_resolution, 600);
        assert_eq!(cfg.art.sources.len(), 4);
        // MPRIS leads: instant and never the wrong album. The resolution gate,
        // not the ordering, is what promotes a higher-quality source.
        assert_eq!(cfg.art.sources[0].label(), "mpris");
    }

    #[test]
    fn source_chain_round_trips() {
        let cfg: Config = toml::from_str(
            r#"
            [art]
            min_resolution = 900

            [[art.source]]
            name = "mpris"

            [[art.source]]
            name = "deezer"
            size = 1000

            [[art.source]]
            name = "spotify"
            client_id_env = "SPOTIFY_ID"
            client_secret_env = "SPOTIFY_SECRET"
            "#,
        )
        .unwrap();

        assert_eq!(cfg.art.min_resolution, 900);
        let labels: Vec<_> = cfg.art.sources.iter().map(|s| s.label()).collect();
        assert_eq!(labels, ["mpris", "deezer", "spotify"]);
    }

    #[test]
    fn typos_are_rejected_rather_than_silently_ignored() {
        let err = toml::from_str::<Config>("[art]\nmin_resolutoin = 900\n");
        assert!(err.is_err());
    }
}
