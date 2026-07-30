//! Discogs release database.
//!
//! Strong release matching — it distinguishes pressings and regional variants
//! that other catalogues collapse — at around 600px. Needs a personal token,
//! and its images carry licensing terms worth reading before redistributing
//! anything built on them.

use super::{ArtRef, ArtSource, Ctx, Locator};
use crate::track::TrackInfo;
use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::Deserialize;

pub struct Discogs {
    token: String,
}

impl Discogs {
    pub fn new(token_env: &str) -> Result<Self> {
        let token = std::env::var(token_env).with_context(|| format!("${token_env} is not set"))?;
        Ok(Self { token })
    }
}

#[derive(Deserialize)]
struct SearchResponse {
    #[serde(default)]
    results: Vec<Item>,
}

#[derive(Deserialize)]
struct Item {
    /// Formatted as "Artist - Album".
    #[serde(default)]
    title: String,
    #[serde(default)]
    cover_image: String,
}

#[async_trait]
impl ArtSource for Discogs {
    fn name(&self) -> &'static str {
        "discogs"
    }

    async fn find(&self, track: &TrackInfo, ctx: &Ctx) -> Result<Vec<ArtRef>> {
        if track.album.trim().is_empty() {
            return Ok(Vec::new());
        }

        let resp = ctx
            .http
            .get("https://api.discogs.com/database/search")
            .header("Authorization", format!("Discogs token={}", self.token))
            .query(&[
                ("artist", track.search_artist()),
                ("release_title", track.album.as_str()),
                ("type", "release"),
                ("per_page", "5"),
            ])
            .send()
            .await?
            .error_for_status()?
            .json::<SearchResponse>()
            .await?;

        let mut out = Vec::new();
        for item in resp.results {
            if item.cover_image.is_empty() {
                continue;
            }
            // Discogs returns a combined "Artist - Album" title. Split on the
            // first dash so each half can be verified independently; a dash in
            // the album name is harmless because either half matching is enough.
            let (artist, album) = item
                .title
                .split_once(" - ")
                .map(|(a, b)| (a.to_string(), b.to_string()))
                .unwrap_or_else(|| (String::new(), item.title.clone()));

            out.push(
                ArtRef::new(Locator::Url(item.cover_image), "discogs").with_claim(artist, album),
            );
        }

        Ok(out)
    }
}
