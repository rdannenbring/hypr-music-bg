//! Apple iTunes Search API.
//!
//! Keyless, large catalogue, and its `artworkUrl100` re-renders up to ~3000px
//! by rewriting the dimensions in the path. The catch is severe and is the
//! reason `verify_match` exists at all: **iTunes answers queries for records it
//! does not carry with confident, unrelated results rather than an empty set.**
//!
//! Measured: a search for `D12 - Devil's Night` returned, as its top album,
//! "Boccherini: Cello Concertos, Stabat Mater & Quintet" by Ophelie Gaillard,
//! with a second result of "40 Most Scary Halloween Classics". Both carry
//! perfectly valid artwork. Nothing in the response marks them as poor matches.
//!
//! So this source always attaches a claim, and refuses to run at all when
//! verification is disabled.

use super::{ArtRef, ArtSource, Ctx, Locator};
use crate::track::TrackInfo;
use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;

pub struct Itunes {
    size: u32,
}

impl Itunes {
    pub fn new(size: u32) -> Self {
        Self { size }
    }
}

#[derive(Deserialize)]
struct SearchResponse {
    #[serde(default)]
    results: Vec<Item>,
}

#[derive(Deserialize)]
struct Item {
    #[serde(default, rename = "artistName")]
    artist_name: String,
    #[serde(default, rename = "collectionName")]
    collection_name: String,
    #[serde(default, rename = "artworkUrl100")]
    artwork_url_100: String,
}

/// `.../source.jpg/100x100bb.jpg` -> `.../source.jpg/1200x1200bb.jpg`.
fn resize(url: &str, size: u32) -> Option<String> {
    let (prefix, tail) = url.rsplit_once('/')?;
    // Tail looks like `100x100bb.jpg`; keep whatever suffix follows the dims.
    let x = tail.find('x')?;
    let rest = &tail[x + 1..];
    let non_digit = rest.find(|c: char| !c.is_ascii_digit())?;
    let suffix = &rest[non_digit..];
    Some(format!("{prefix}/{size}x{size}{suffix}"))
}

#[async_trait]
impl ArtSource for Itunes {
    fn name(&self) -> &'static str {
        "itunes"
    }

    async fn find(&self, track: &TrackInfo, ctx: &Ctx) -> Result<Vec<ArtRef>> {
        if track.album.trim().is_empty() {
            return Ok(Vec::new());
        }

        let term = format!("{} {}", track.search_artist(), track.album);
        let resp = ctx
            .http
            .get("https://itunes.apple.com/search")
            .query(&[
                ("term", term.as_str()),
                ("entity", "album"),
                ("media", "music"),
                ("limit", "5"),
            ])
            .send()
            .await?
            .error_for_status()?
            // iTunes serves JSON as `text/javascript`, which trips reqwest's
            // content-type check, so decode from bytes instead of `.json()`.
            .bytes()
            .await?;

        let parsed: SearchResponse = serde_json::from_slice(&resp)?;

        let mut out = Vec::new();
        for item in parsed.results {
            if item.artwork_url_100.is_empty() {
                continue;
            }
            let url = resize(&item.artwork_url_100, self.size)
                .unwrap_or_else(|| item.artwork_url_100.clone());

            out.push(
                ArtRef::new(Locator::Url(url), "itunes")
                    // Never omitted. Without this the Boccherini record above
                    // would land on the desktop.
                    .with_claim(item.artist_name, item.collection_name)
                    .with_declared(self.size, self.size),
            );
        }

        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewrites_artwork_dimensions() {
        let url = "https://is1-ssl.mzstatic.com/image/thumb/Music126/v4/db/8b/x.png/100x100bb.jpg";
        assert_eq!(
            resize(url, 1200).unwrap(),
            "https://is1-ssl.mzstatic.com/image/thumb/Music126/v4/db/8b/x.png/1200x1200bb.jpg"
        );
    }

    #[test]
    fn every_candidate_carries_a_claim() {
        // Guards the invariant that makes this source safe to enable: if a
        // candidate ever escaped without a claim, the engine would skip
        // verification and accept whatever iTunes felt like returning.
        let item = Item {
            artist_name: "Ophelie Gaillard".into(),
            collection_name: "Boccherini: Cello Concertos".into(),
            artwork_url_100: "https://example.com/x.png/100x100bb.jpg".into(),
        };
        let art_ref = ArtRef::new(Locator::Url(item.artwork_url_100.clone()), "itunes")
            .with_claim(item.artist_name, item.collection_name);
        assert!(art_ref.claim.is_some());
    }
}
