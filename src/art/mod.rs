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

    /// Build a resolver over a fixed set of sources.
    ///
    /// Exists so the acceptance policy can be tested against real image files
    /// without touching the network: the tiering is the most consequential logic
    /// here and was previously covered only by its inputs.
    #[cfg(test)]
    fn with_sources(sources: Vec<Arc<dyn ArtSource>>, cache: Cache, cfg: &ArtConfig) -> Self {
        Self {
            sources,
            ctx: Ctx {
                http: reqwest::Client::new(),
                cache,
            },
            min_resolution: cfg.min_resolution,
            verify_match: cfg.verify_match,
            match_threshold: cfg.match_threshold,
            allow_degraded: cfg.allow_degraded,
            negative_cache_ttl: cfg.negative_cache_ttl,
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::track::TrackInfo;

    /// A source that returns whatever it was handed.
    struct FakeSource {
        name: &'static str,
        refs: Vec<ArtRef>,
        needs_search: bool,
        fail: bool,
    }

    impl FakeSource {
        fn new(name: &'static str, refs: Vec<ArtRef>) -> Self {
            Self {
                name,
                refs,
                needs_search: true,
                fail: false,
            }
        }

        fn failing(name: &'static str) -> Self {
            Self {
                name,
                refs: Vec::new(),
                needs_search: true,
                fail: true,
            }
        }

        fn local(mut self) -> Self {
            self.needs_search = false;
            self
        }

        fn boxed(self) -> Arc<dyn ArtSource> {
            Arc::new(self)
        }
    }

    #[async_trait]
    impl ArtSource for FakeSource {
        fn name(&self) -> &'static str {
            self.name
        }

        async fn find(&self, _track: &TrackInfo, _ctx: &Ctx) -> Result<Vec<ArtRef>> {
            if self.fail {
                return Err(anyhow!("source exploded"));
            }
            Ok(self.refs.clone())
        }

        fn needs_search_metadata(&self) -> bool {
            self.needs_search
        }
    }

    struct Fixture {
        root: PathBuf,
        cache: Cache,
    }

    impl Fixture {
        fn new(name: &str) -> Self {
            let root = std::env::temp_dir().join(format!(
                "hmb-resolve-{}-{name}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            std::fs::remove_dir_all(&root).ok();
            let cache = Cache::new(&root).unwrap();
            Self { root, cache }
        }

        /// A real PNG on disk, so the resolver decodes genuine dimensions
        /// rather than trusting what a source advertised.
        fn image(&self, name: &str, width: u32, height: u32) -> PathBuf {
            let path = self.root.join(format!("{name}.png"));
            image::RgbaImage::from_pixel(width, height, image::Rgba([10, 20, 30, 255]))
                .save(&path)
                .unwrap();
            path
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.root).ok();
        }
    }

    fn track() -> TrackInfo {
        TrackInfo {
            artist: "D12".into(),
            album: "Devil's Night".into(),
            title: "Fight Music".into(),
            ..TrackInfo::default()
        }
    }

    fn config() -> ArtConfig {
        ArtConfig {
            min_resolution: 600,
            negative_cache_ttl: 0,
            ..ArtConfig::default()
        }
    }

    #[tokio::test]
    async fn takes_the_first_source_that_clears_the_floor() {
        let fx = Fixture::new("first");
        let big = fx.image("big", 1200, 1200);
        let bigger = fx.image("bigger", 2000, 2000);

        let resolver = Resolver::with_sources(
            vec![
                FakeSource::new("a", vec![ArtRef::new(Locator::File(big), "a")])
                    .local()
                    .boxed(),
                FakeSource::new("b", vec![ArtRef::new(Locator::File(bigger), "b")])
                    .local()
                    .boxed(),
            ],
            fx.cache.clone(),
            &config(),
        );

        let art = resolver
            .resolve(&track())
            .await
            .art
            .expect("should resolve");
        // Order wins over size once the floor is met: later sources are not
        // consulted at all, which is what keeps the fast path fast.
        assert_eq!(art.source, "a");
        assert_eq!(art.width, 1200);
        assert!(!art.degraded);
    }

    /// The behaviour the whole design exists for: a source that answers
    /// instantly but small must not block a later, larger one.
    #[tokio::test]
    async fn falls_through_a_source_below_the_floor() {
        let fx = Fixture::new("fallthrough");
        let small = fx.image("small", 300, 300);
        let large = fx.image("large", 1200, 1200);

        let resolver = Resolver::with_sources(
            vec![
                FakeSource::new("mpris", vec![ArtRef::new(Locator::File(small), "mpris")])
                    .local()
                    .boxed(),
                FakeSource::new("remote", vec![ArtRef::new(Locator::File(large), "remote")])
                    .local()
                    .boxed(),
            ],
            fx.cache.clone(),
            &config(),
        );

        let resolution = resolver.resolve(&track()).await;
        let art = resolution.art.expect("should resolve");
        assert_eq!(art.source, "remote");
        assert_eq!(art.width, 1200);

        // The walk must record why the first source lost, not just who won.
        let outcomes: Vec<_> = resolution
            .outcomes
            .iter()
            .map(|o| (o.source.as_str(), o.outcome.as_str()))
            .collect();
        assert!(
            outcomes
                .iter()
                .any(|(s, o)| *s == "mpris" && o.contains("below min")),
            "expected a below-min outcome for mpris, got {outcomes:?}"
        );
    }

    #[tokio::test]
    async fn degrades_to_the_largest_when_nothing_clears_the_floor() {
        let fx = Fixture::new("degraded");
        let tiny = fx.image("tiny", 200, 200);
        let mid = fx.image("mid", 450, 450);

        let resolver = Resolver::with_sources(
            vec![
                FakeSource::new("a", vec![ArtRef::new(Locator::File(tiny), "a")])
                    .local()
                    .boxed(),
                FakeSource::new("b", vec![ArtRef::new(Locator::File(mid), "b")])
                    .local()
                    .boxed(),
            ],
            fx.cache.clone(),
            &config(),
        );

        let art = resolver
            .resolve(&track())
            .await
            .art
            .expect("should degrade");
        assert_eq!(art.source, "b", "the largest sub-threshold candidate wins");
        assert_eq!(art.width, 450);
        assert!(art.degraded, "must be flagged so the caller can warn");
    }

    #[tokio::test]
    async fn allow_degraded_off_gives_up_instead() {
        let fx = Fixture::new("nodegrade");
        let tiny = fx.image("tiny", 200, 200);

        let cfg = ArtConfig {
            allow_degraded: false,
            ..config()
        };
        let resolver = Resolver::with_sources(
            vec![
                FakeSource::new("a", vec![ArtRef::new(Locator::File(tiny), "a")])
                    .local()
                    .boxed(),
            ],
            fx.cache.clone(),
            &cfg,
        );

        assert!(resolver.resolve(&track()).await.art.is_none());
    }

    /// The iTunes failure mode: a confident answer for a different record.
    #[tokio::test]
    async fn rejects_a_candidate_that_does_not_match_the_track() {
        let fx = Fixture::new("verify");
        let wrong = fx.image("wrong", 1200, 1200);
        let right = fx.image("right", 900, 900);

        let resolver = Resolver::with_sources(
            vec![
                FakeSource::new(
                    "itunes",
                    vec![ArtRef::new(Locator::File(wrong), "itunes").with_claim(
                        "Ophelie Gaillard",
                        "Boccherini: Cello Concertos, Stabat Mater & Quintet",
                    )],
                )
                .local()
                .boxed(),
                FakeSource::new(
                    "deezer",
                    vec![
                        ArtRef::new(Locator::File(right), "deezer")
                            .with_claim("D12", "Devil's Night"),
                    ],
                )
                .local()
                .boxed(),
            ],
            fx.cache.clone(),
            &config(),
        );

        let art = resolver
            .resolve(&track())
            .await
            .art
            .expect("should resolve");
        assert_eq!(
            art.source, "deezer",
            "the larger but unrelated cover must be refused"
        );
    }

    #[tokio::test]
    async fn verification_can_be_turned_off() {
        let fx = Fixture::new("noverify");
        let wrong = fx.image("wrong", 1200, 1200);

        let cfg = ArtConfig {
            verify_match: false,
            ..config()
        };
        let resolver = Resolver::with_sources(
            vec![
                FakeSource::new(
                    "itunes",
                    vec![
                        ArtRef::new(Locator::File(wrong), "itunes")
                            .with_claim("Someone Else", "A Different Record"),
                    ],
                )
                .local()
                .boxed(),
            ],
            fx.cache.clone(),
            &cfg,
        );

        assert!(resolver.resolve(&track()).await.art.is_some());
    }

    /// Sources declaring a size below the floor should not be downloaded during
    /// the preferred pass, but must still be reachable if we reach tier 2.
    #[tokio::test]
    async fn deferred_candidates_are_used_only_when_degrading() {
        let fx = Fixture::new("deferred");
        let declared_small = fx.image("declared", 800, 800);

        let resolver = Resolver::with_sources(
            vec![
                FakeSource::new(
                    "declares-small",
                    // Advertises 100x100 but is really 800x800: the engine should
                    // skip it on the strength of the declaration, then fall back
                    // to it and discover the real size.
                    vec![
                        ArtRef::new(Locator::File(declared_small), "declares-small")
                            .with_declared(100, 100),
                    ],
                )
                .local()
                .boxed(),
            ],
            fx.cache.clone(),
            &config(),
        );

        let art = resolver
            .resolve(&track())
            .await
            .art
            .expect("should degrade to it");
        assert_eq!(art.width, 800, "real dimensions win over the declaration");
        assert!(art.degraded);
    }

    #[tokio::test]
    async fn a_failing_source_does_not_stop_the_chain() {
        let fx = Fixture::new("failing");
        let good = fx.image("good", 1200, 1200);

        let resolver = Resolver::with_sources(
            vec![
                FakeSource::failing("broken").local().boxed(),
                FakeSource::new("good", vec![ArtRef::new(Locator::File(good), "good")])
                    .local()
                    .boxed(),
            ],
            fx.cache.clone(),
            &config(),
        );

        let resolution = resolver.resolve(&track()).await;
        assert_eq!(resolution.art.expect("should resolve").source, "good");
        assert!(
            resolution
                .outcomes
                .iter()
                .any(|o| o.source == "broken" && o.outcome.contains("error")),
            "the failure should be reported, not swallowed"
        );
    }

    /// Thin metadata must not reach catalogue sources: a lookup would be spent
    /// and a negative-cache entry written for an album that does not exist.
    #[tokio::test]
    async fn search_sources_are_skipped_when_metadata_is_too_thin() {
        let fx = Fixture::new("thin");
        let art = fx.image("art", 1200, 1200);

        let stream = TrackInfo {
            artist: String::new(),
            album: "WLIR Radio".into(),
            ..TrackInfo::default()
        };

        let resolver = Resolver::with_sources(
            vec![
                FakeSource::new(
                    "catalogue",
                    vec![ArtRef::new(Locator::File(art), "catalogue")],
                )
                .boxed(),
            ],
            fx.cache.clone(),
            &config(),
        );

        let resolution = resolver.resolve(&stream).await;
        assert!(resolution.art.is_none(), "must not query a catalogue");
        assert!(
            resolution
                .outcomes
                .iter()
                .any(|o| o.outcome.contains("too thin")),
            "the skip should be explained"
        );
    }

    #[tokio::test]
    async fn a_remembered_winner_short_circuits_the_chain() {
        let fx = Fixture::new("lookup");
        let good = fx.image("good", 1200, 1200);

        let sources = || {
            vec![
                FakeSource::new(
                    "slow",
                    vec![ArtRef::new(Locator::File(good.clone()), "slow")],
                )
                .local()
                .boxed(),
            ]
        };

        let first = Resolver::with_sources(sources(), fx.cache.clone(), &config());
        assert_eq!(first.resolve(&track()).await.art.unwrap().source, "slow");

        // Second time round there are no sources at all, so anything returned
        // can only have come from the remembered lookup.
        let second = Resolver::with_sources(Vec::new(), fx.cache.clone(), &config());
        let art = second
            .resolve(&track())
            .await
            .art
            .expect("cache should answer");
        assert_eq!(art.source, "slow");
        assert_eq!(art.width, 1200);
    }

    #[tokio::test]
    async fn a_negative_result_is_remembered() {
        let fx = Fixture::new("negative");
        let cfg = ArtConfig {
            negative_cache_ttl: 3600,
            ..config()
        };

        let empty = Resolver::with_sources(
            vec![FakeSource::new("nothing", Vec::new()).local().boxed()],
            fx.cache.clone(),
            &cfg,
        );
        assert!(empty.resolve(&track()).await.art.is_none());

        // A source that *would* answer must not even be consulted, because the
        // album is now known to have no art.
        let good = fx.image("good", 1200, 1200);
        let later = Resolver::with_sources(
            vec![
                FakeSource::new("late", vec![ArtRef::new(Locator::File(good), "late")])
                    .local()
                    .boxed(),
            ],
            fx.cache.clone(),
            &cfg,
        );
        let resolution = later.resolve(&track()).await;
        assert!(resolution.art.is_none());
        assert!(
            resolution
                .outcomes
                .iter()
                .any(|o| o.outcome.contains("no art")),
            "the short circuit should be visible in the walk"
        );
    }
}
