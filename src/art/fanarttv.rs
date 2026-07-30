//! fanart.tv.
//!
//! The odd one out: its `artistbackground` images are 1920x1080 and are drawn
//! specifically to be used as desktop backgrounds, which is a better fit for a
//! wallpaper than a square cover zoomed and blurred to fill a 16:9 screen.
//!
//! The trade-off is that a background belongs to the *artist*, not the album,
//! so it will not change between records by the same act. fanart.tv also
//! exposes real per-album covers under `albums`, keyed by release-group MBID,
//! and both are returned here — album covers first, since they track the music
//! more closely, with backgrounds behind them.
//!
//! Everything is keyed on MusicBrainz IDs, so this inherits the MusicBrainz
//! lookup and its rate limit.

use super::musicbrainz;
use super::{ArtRef, ArtSource, Ctx, Locator};
use crate::track::TrackInfo;
use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::Deserialize;
use std::collections::HashMap;

pub struct FanartTv {
    api_key: String,
    /// Skip entirely unless a release group was resolved, rather than falling
    /// back to artist-level imagery.
    require_mbid: bool,
}

impl FanartTv {
    pub fn new(api_key_env: &str, require_mbid: bool) -> Result<Self> {
        let api_key =
            std::env::var(api_key_env).with_context(|| format!("${api_key_env} is not set"))?;
        Ok(Self {
            api_key,
            require_mbid,
        })
    }
}

#[derive(Deserialize, Default)]
struct MusicResponse {
    #[serde(default)]
    artistbackground: Vec<Entry>,
    #[serde(default)]
    albums: HashMap<String, AlbumEntry>,
}

#[derive(Deserialize, Default)]
struct AlbumEntry {
    #[serde(default)]
    albumcover: Vec<Entry>,
}

#[derive(Deserialize)]
struct Entry {
    #[serde(default)]
    url: String,
}

#[async_trait]
impl ArtSource for FanartTv {
    fn name(&self) -> &'static str {
        "fanarttv"
    }

    async fn find(&self, track: &TrackInfo, ctx: &Ctx) -> Result<Vec<ArtRef>> {
        let release_group = musicbrainz::find_release_group(&ctx.http, track).await?;

        let Some(artist_id) = release_group.as_ref().and_then(|rg| rg.artist_id.clone()) else {
            if self.require_mbid {
                return Ok(Vec::new());
            }
            // No artist MBID came back with the release group, so fall back to
            // looking the artist up directly.
            let Some((id, _)) =
                musicbrainz::find_artist_mbid(&ctx.http, track.search_artist()).await?
            else {
                return Ok(Vec::new());
            };
            return self.fetch(&id, None, track, ctx).await;
        };

        let rg_id = release_group.as_ref().map(|rg| rg.id.clone());
        self.fetch(&artist_id, rg_id, track, ctx).await
    }
}

impl FanartTv {
    async fn fetch(
        &self,
        artist_mbid: &str,
        release_group_id: Option<String>,
        track: &TrackInfo,
        ctx: &Ctx,
    ) -> Result<Vec<ArtRef>> {
        let resp = ctx
            .http
            .get(format!(
                "https://webservice.fanart.tv/v3/music/{artist_mbid}"
            ))
            .query(&[("api_key", self.api_key.as_str())])
            .send()
            .await?;

        // 404 simply means fanart.tv has nothing for this artist.
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(Vec::new());
        }
        let parsed: MusicResponse = resp.error_for_status()?.json().await?;

        let mut out = Vec::new();

        // Real album art first: it changes per record, which a background does not.
        if let Some(rg_id) = release_group_id
            && let Some(album) = parsed.albums.get(&rg_id)
        {
            for entry in &album.albumcover {
                if entry.url.is_empty() {
                    continue;
                }
                out.push(
                    ArtRef::new(Locator::Url(entry.url.clone()), "fanarttv")
                        .with_claim(track.search_artist(), &track.album),
                );
            }
        }

        // Then artist backgrounds. These are 1920x1080 by fanart.tv's own spec,
        // so they clear any sane `min_resolution` on the long edge — but the
        // short edge is 1080, which is what the gate actually measures.
        for entry in &parsed.artistbackground {
            if entry.url.is_empty() {
                continue;
            }
            out.push(
                ArtRef::new(Locator::Url(entry.url.clone()), "fanarttv")
                    // Artist-level: the album field would never match, so only
                    // the artist is claimed.
                    .with_claim(track.search_artist(), track.search_artist())
                    .with_declared(1920, 1080),
            );
        }

        Ok(out)
    }
}
