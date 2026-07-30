//! Art sources and the policy that chooses between them.
//!
//! The chain is deliberately *not* "first source with a response wins". Search
//! based catalogs answer queries for records they do not carry with confident,
//! unrelated results, so every candidate has to clear two gates before it is
//! accepted: it must look like the album that is playing, and it must be large
//! enough to be worth putting on a screen.

pub mod coverartarchive;
pub mod deezer;
pub mod discogs;
pub mod exec;
pub mod fanarttv;
pub mod itunes;
pub mod local;
pub mod mpris_art;
pub mod musicbrainz;
pub mod spotify;
pub mod subsonic;

use crate::cache::Cache;
use crate::config::{ArtConfig, SourceConfig};
use crate::control::SourceOutcome;
use crate::matching;
use crate::track::TrackInfo;
use anyhow::{Result, anyhow};
use async_trait::async_trait;
use std::io::Cursor;
use std::path::PathBuf;
use std::sync::Arc;

/// Where a candidate image can be read from.
#[derive(Debug, Clone)]
pub enum Locator {
    Url(String),
    File(PathBuf),
}

impl Locator {
    /// Stable cache key.
    pub fn key(&self) -> String {
        match self {
            Self::Url(u) => u.clone(),
            Self::File(p) => p.display().to_string(),
        }
    }
}

/// What a source believes this image depicts. `None` means the source is
/// structurally incapable of being wrong — the player handed us the art for the
/// track it is playing, or we read it off the audio file itself — so there is
/// nothing to verify against.
#[derive(Debug, Clone)]
pub struct Claim {
    pub artist: String,
    pub album: String,
}

/// A candidate cover, before it has been downloaded or checked.
#[derive(Debug, Clone)]
pub struct ArtRef {
    pub locator: Locator,
    /// Dimensions the source advertises, when it does. Lets the engine skip
    /// downloading art it already knows is too small.
    pub declared: Option<(u32, u32)>,
    pub source: &'static str,
    pub claim: Option<Claim>,
}

impl ArtRef {
    pub fn new(locator: Locator, source: &'static str) -> Self {
        Self {
            locator,
            declared: None,
            source,
            claim: None,
        }
    }

    pub fn with_claim(mut self, artist: impl Into<String>, album: impl Into<String>) -> Self {
        self.claim = Some(Claim {
            artist: artist.into(),
            album: album.into(),
        });
        self
    }

    pub fn with_declared(mut self, w: u32, h: u32) -> Self {
        self.declared = Some((w, h));
        self
    }
}

/// A candidate that has been fetched, decoded far enough to know its size, and
/// checked against the playing track.
#[derive(Debug, Clone)]
pub struct ResolvedArt {
    pub bytes: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub source: String,
    /// Cache key of the image these bytes came from, so the winning lookup can
    /// be recorded and replayed.
    pub locator_key: String,
    /// True when this was accepted only because nothing cleared
    /// `min_resolution`.
    pub degraded: bool,
}

impl ResolvedArt {
    pub fn short_edge(&self) -> u32 {
        self.width.min(self.height)
    }
}

/// The outcome of one chain walk: the winner, plus what every source did.
///
/// The per-source record exists because this whole design is a fallback chain.
/// "Which source won" is far less useful for diagnosis than "mpris was 300x300
/// so it was passed over, local had nothing, coverartarchive gave 1200x1190".
#[derive(Debug, Default)]
pub struct Resolution {
    pub art: Option<ResolvedArt>,
    pub outcomes: Vec<SourceOutcome>,
}

/// Shared services handed to every source.
pub struct Ctx {
    pub http: reqwest::Client,
    pub cache: Cache,
}

impl Ctx {
    /// Fetch bytes for a locator, going through the cache.
    pub async fn materialize(&self, locator: &Locator) -> Result<Vec<u8>> {
        let key = locator.key();
        if let Some(bytes) = self.cache.get(&key) {
            tracing::debug!(key = %key, "cache hit");
            return Ok(bytes);
        }

        let bytes = match locator {
            Locator::File(path) => tokio::fs::read(path).await?,
            Locator::Url(url) => {
                let resp = self.http.get(url).send().await?;
                let status = resp.status();
                if !status.is_success() {
                    return Err(anyhow!("HTTP {status}"));
                }
                resp.bytes().await?.to_vec()
            }
        };

        if bytes.is_empty() {
            return Err(anyhow!("empty response"));
        }
        self.cache.put(&key, &bytes);
        Ok(bytes)
    }
}

/// One place art can come from.
#[async_trait]
pub trait ArtSource: Send + Sync {
    fn name(&self) -> &'static str;

    /// Candidates for this track, best first. An empty vec means "I have
    /// nothing", which is not an error.
    async fn find(&self, track: &TrackInfo, ctx: &Ctx) -> Result<Vec<ArtRef>>;

    /// Whether this source queries a remote catalogue by artist and album, and
    /// so should be skipped when the metadata is too thin to search with.
    ///
    /// Sources reading art the player or filesystem already has do not need it.
    fn needs_search_metadata(&self) -> bool {
        true
    }
}

/// Build the configured chain. Sources that cannot be constructed (a missing
/// credential env var, say) are logged and dropped rather than being fatal, so
/// one misconfigured entry does not take the daemon down.
pub fn build_sources(cfg: &ArtConfig) -> Vec<Arc<dyn ArtSource>> {
    let mut out: Vec<Arc<dyn ArtSource>> = Vec::new();

    for source in &cfg.sources {
        let built: Result<Arc<dyn ArtSource>> = match source {
            SourceConfig::Mpris => Ok(Arc::new(mpris_art::MprisArt)),
            SourceConfig::Local { paths } => Ok(Arc::new(local::LocalArt::new(paths))),
            SourceConfig::CoverArtArchive { size } => {
                Ok(Arc::new(coverartarchive::CoverArtArchive::new(*size)))
            }
            SourceConfig::Deezer { size } => Ok(Arc::new(deezer::Deezer::new(*size))),
            SourceConfig::Itunes { size } => Ok(Arc::new(itunes::Itunes::new(*size))),
            SourceConfig::Discogs { token_env } => {
                discogs::Discogs::new(token_env).map(|s| Arc::new(s) as Arc<dyn ArtSource>)
            }
            SourceConfig::FanartTv {
                api_key_env,
                require_mbid,
            } => fanarttv::FanartTv::new(api_key_env, *require_mbid)
                .map(|s| Arc::new(s) as Arc<dyn ArtSource>),
            SourceConfig::Spotify {
                client_id_env,
                client_secret_env,
            } => spotify::Spotify::new(client_id_env, client_secret_env)
                .map(|s| Arc::new(s) as Arc<dyn ArtSource>),
            SourceConfig::Subsonic {
                url,
                username,
                password_env,
                size,
            } => subsonic::Subsonic::new(url, username, password_env, *size)
                .map(|s| Arc::new(s) as Arc<dyn ArtSource>),
            SourceConfig::Exec { command, args } => {
                Ok(Arc::new(exec::ExecSource::new(command, args)))
            }
        };

        match built {
            Ok(s) => out.push(s),
            Err(e) => tracing::warn!(
                source = source.label(),
                error = %e,
                "skipping source it could not be configured"
            ),
        }
    }

    out
}

/// Walks the chain and applies the acceptance policy.
pub struct Resolver {
    sources: Vec<Arc<dyn ArtSource>>,
    ctx: Ctx,
    min_resolution: u32,
    verify_match: bool,
    match_threshold: f64,
    allow_degraded: bool,
    negative_cache_ttl: u64,
}

impl Resolver {
    pub fn new(cfg: &ArtConfig, cache: Cache) -> Result<Self> {
        // MusicBrainz asks that clients identify themselves, and rejects
        // requests from generic agents.
        let http = reqwest::Client::builder()
            .user_agent(concat!(
                "hypr-music-bg/",
                env!("CARGO_PKG_VERSION"),
                " ( https://github.com/rdannenbring/hypr-music-bg )"
            ))
            .timeout(std::time::Duration::from_secs(15))
            .build()?;

        Ok(Self {
            sources: build_sources(cfg),
            ctx: Ctx { http, cache },
            min_resolution: cfg.min_resolution,
            verify_match: cfg.verify_match,
            match_threshold: cfg.match_threshold,
            allow_degraded: cfg.allow_degraded,
            negative_cache_ttl: cfg.negative_cache_ttl,
        })
    }

    pub fn source_names(&self) -> Vec<&'static str> {
        self.sources.iter().map(|s| s.name()).collect()
    }

    /// Resolve art for a track.
    ///
    /// Tier 1: the first candidate, in source order, that both verifies and
    ///         clears `min_resolution`.
    /// Tier 2: failing that, the largest candidate that verified.
    /// Tier 3: failing that, `None` — the caller falls back to a static image.
    pub async fn resolve(&self, track: &TrackInfo) -> Resolution {
        let album_key = track.album_key();

        // Replaying a record must not re-spend the MusicBrainz budget.
        if let Some(hit) = self.ctx.cache.get_lookup(&album_key)
            && let Some(bytes) = self.ctx.cache.get(&hit.locator_key)
        {
            tracing::debug!(source = %hit.source, "lookup cache hit");
            return Resolution {
                outcomes: vec![SourceOutcome {
                    source: hit.source.clone(),
                    outcome: format!(
                        "cached {}x{}{}",
                        hit.width,
                        hit.height,
                        if hit.degraded { " (degraded)" } else { "" }
                    ),
                }],
                art: Some(ResolvedArt {
                    bytes,
                    width: hit.width,
                    height: hit.height,
                    locator_key: hit.locator_key.clone(),
                    source: hit.source.clone(),
                    degraded: hit.degraded,
                }),
            };
        }

        if self
            .ctx
            .cache
            .negative_hit(&album_key, self.negative_cache_ttl)
        {
            tracing::debug!(album = %track.album, "negative cache hit, skipping all sources");
            return Resolution {
                art: None,
                outcomes: vec![SourceOutcome {
                    source: "(all)".into(),
                    outcome: "skipped: cached as having no art".into(),
                }],
            };
        }

        let resolved = self.resolve_uncached(track).await;

        match &resolved.art {
            Some(art) => {
                self.ctx.cache.clear_negative(&album_key);
                self.ctx.cache.put_lookup(
                    &album_key,
                    &crate::cache::LookupHit {
                        locator_key: art.locator_key.clone(),
                        source: art.source.clone(),
                        width: art.width,
                        height: art.height,
                        degraded: art.degraded,
                    },
                );
            }
            None => self.ctx.cache.mark_negative(&album_key),
        }

        resolved
    }

    async fn resolve_uncached(&self, track: &TrackInfo) -> Resolution {
        let mut outcomes: Vec<SourceOutcome> = Vec::new();
        let mut record = |source: &str, outcome: String| {
            outcomes.push(SourceOutcome {
                source: source.to_string(),
                outcome,
            });
        };
        let mut best_degraded: Option<ResolvedArt> = None;
        // Candidates skipped without downloading because they advertised a size
        // below the threshold. Only worth fetching if we end up in tier 2.
        let mut deferred: Vec<ArtRef> = Vec::new();

        let searchable = track.is_searchable();

        for source in &self.sources {
            if source.needs_search_metadata() && !searchable {
                tracing::debug!(
                    source = source.name(),
                    artist = %track.search_artist(),
                    album = %track.album,
                    "skipped: metadata too thin to search a catalogue with"
                );
                record(source.name(), "skipped: metadata too thin to search".into());
                continue;
            }

            let refs = match source.find(track, &self.ctx).await {
                Ok(refs) => refs,
                Err(e) => {
                    tracing::warn!(source = source.name(), error = %e, "source failed");
                    record(source.name(), format!("error: {e}"));
                    continue;
                }
            };

            if refs.is_empty() {
                tracing::debug!(source = source.name(), "no candidates");
                record(source.name(), "no candidates".into());
                continue;
            }

            for art_ref in refs {
                // Cheap rejection: the source told us it is too small.
                if let Some((w, h)) = art_ref.declared
                    && w.min(h) < self.min_resolution
                {
                    tracing::debug!(
                        source = art_ref.source,
                        width = w,
                        height = h,
                        "below min_resolution by declaration, deferring"
                    );
                    record(
                        art_ref.source,
                        format!("{w}x{h} advertised, below min {}", self.min_resolution),
                    );
                    deferred.push(art_ref);
                    continue;
                }

                let Some(resolved) = self.try_candidate(track, &art_ref).await else {
                    record(art_ref.source, "rejected".into());
                    continue;
                };

                if resolved.short_edge() >= self.min_resolution {
                    tracing::info!(
                        source = %resolved.source,
                        width = resolved.width,
                        height = resolved.height,
                        "accepted"
                    );
                    record(
                        &resolved.source,
                        format!("{}x{} accepted", resolved.width, resolved.height),
                    );
                    return Resolution {
                        art: Some(resolved),
                        outcomes,
                    };
                }

                tracing::debug!(
                    source = %resolved.source,
                    short_edge = resolved.short_edge(),
                    min = self.min_resolution,
                    "verified but below min_resolution, keeping as fallback"
                );
                record(
                    &resolved.source,
                    format!(
                        "{}x{}, below min {}",
                        resolved.width, resolved.height, self.min_resolution
                    ),
                );
                if best_degraded
                    .as_ref()
                    .is_none_or(|b| resolved.short_edge() > b.short_edge())
                {
                    best_degraded = Some(resolved);
                }
            }
        }

        if !self.allow_degraded {
            tracing::info!("nothing met min_resolution and allow_degraded is off");
            record(
                "(policy)",
                "nothing met min_resolution; degraded disabled".into(),
            );
            return Resolution {
                art: None,
                outcomes,
            };
        }

        // Tier 2. Fetch the deferred candidates now, largest advertised first,
        // in case one of them beats what we already have.
        deferred.sort_by_key(|r| std::cmp::Reverse(r.declared.map_or(0, |(w, h)| w.min(h))));
        for art_ref in deferred {
            let advertised = art_ref.declared.map_or(0, |(w, h)| w.min(h));
            if best_degraded
                .as_ref()
                .is_some_and(|b| b.short_edge() >= advertised)
            {
                break; // Sorted, so nothing later can beat this either.
            }
            if let Some(resolved) = self.try_candidate(track, &art_ref).await
                && best_degraded
                    .as_ref()
                    .is_none_or(|b| resolved.short_edge() > b.short_edge())
            {
                best_degraded = Some(resolved);
            }
        }

        if let Some(mut art) = best_degraded {
            art.degraded = true;
            record(
                &art.source,
                format!("{}x{} accepted (DEGRADED)", art.width, art.height),
            );
            tracing::info!(
                source = %art.source,
                width = art.width,
                height = art.height,
                min = self.min_resolution,
                "no source met min_resolution, using the largest available"
            );
            return Resolution {
                art: Some(art),
                outcomes,
            };
        }

        tracing::info!("no source produced usable art");
        Resolution {
            art: None,
            outcomes,
        }
    }

    /// Download, size, and verify one candidate.
    async fn try_candidate(&self, track: &TrackInfo, art_ref: &ArtRef) -> Option<ResolvedArt> {
        // Verify before downloading — a mismatch is not worth the bytes.
        if self.verify_match
            && let Some(claim) = &art_ref.claim
        {
            let artist = matching::similarity(track.search_artist(), &claim.artist);
            let album = matching::similarity(&track.album, &claim.album);
            // Either field carrying the match is enough: single-artist records
            // often disagree on artist spelling, and self-titled albums make the
            // album field ambiguous.
            let score = artist.max(album);
            if score < self.match_threshold {
                tracing::debug!(
                    source = art_ref.source,
                    claimed_artist = %claim.artist,
                    claimed_album = %claim.album,
                    score,
                    "rejected: does not match the playing track"
                );
                return None;
            }
        }

        let bytes = match self.ctx.materialize(&art_ref.locator).await {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(source = art_ref.source, error = %e, "fetch failed");
                return None;
            }
        };

        let (width, height) = match probe_dimensions(&bytes) {
            Ok(d) => d,
            Err(e) => {
                tracing::warn!(source = art_ref.source, error = %e, "not a decodable image");
                return None;
            }
        };

        Some(ResolvedArt {
            bytes,
            width,
            height,
            source: art_ref.source.to_string(),
            locator_key: art_ref.locator.key(),
            degraded: false,
        })
    }
}

/// Read dimensions from the image header without decoding pixel data.
pub fn probe_dimensions(bytes: &[u8]) -> Result<(u32, u32)> {
    let reader = image::ImageReader::new(Cursor::new(bytes)).with_guessed_format()?;
    Ok(reader.into_dimensions()?)
}
