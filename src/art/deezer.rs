//! Deezer public catalog.
//!
//! One hop, no API key, no registration — the best zero-config remote source.
//! Its documented `cover_xl` is 1000px, but the CDN will re-render to arbitrary
//! dimensions from the URL, and measurement shows it caps out at 1200: a
//! request for 1500 comes back as 1200. `size` is clamped accordingly so the
//! engine is not told to expect pixels that will never arrive.

use super::{ArtRef, ArtSource, Ctx, Locator};
use crate::track::TrackInfo;
use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;

/// Measured ceiling: asking for more than this silently returns this.
const CDN_MAX: u32 = 1200;

pub struct Deezer {
    size: u32,
}

impl Deezer {
    pub fn new(size: u32) -> Self {
        Self {
            size: size.clamp(56, CDN_MAX),
        }
    }
}

#[derive(Deserialize)]
struct SearchResponse {
    #[serde(default)]
    data: Vec<Album>,
}

#[derive(Deserialize)]
struct Album {
    #[serde(default)]
    title: String,
    #[serde(default)]
    cover_big: String,
    #[serde(default)]
    cover_xl: String,
    #[serde(default)]
    artist: Artist,
}

#[derive(Deserialize, Default)]
struct Artist {
    #[serde(default)]
    name: String,
}

/// Deezer's CDN URLs look like
/// `.../cover/<hash>/500x500-000000-80-0-0.jpg`, and the dimensions are just
/// path segments — swapping them re-renders at any size up to the cap.
fn resize(url: &str, size: u32) -> Option<String> {
    let (prefix, tail) = url.rsplit_once('/')?;
    if !tail.contains('x') {
        return None;
    }
    // Preserve the trailing quality/background parameters, which differ
    // between endpoints.
    let params = tail.split('-').skip(1).collect::<Vec<_>>().join("-");
    if params.is_empty() {
        return None;
    }
    Some(format!("{prefix}/{size}x{size}-{params}"))
}

#[async_trait]
impl ArtSource for Deezer {
    fn name(&self) -> &'static str {
        "deezer"
    }

    async fn find(&self, track: &TrackInfo, ctx: &Ctx) -> Result<Vec<ArtRef>> {
        if track.album.trim().is_empty() {
            return Ok(Vec::new());
        }

        // Deezer supports fielded search, which is far more precise than
        // concatenating the artist and album into one free-text blob.
        let query = format!(
            "artist:\"{}\" album:\"{}\"",
            track.search_artist().replace('"', ""),
            track.album.replace('"', "")
        );

        let resp = ctx
            .http
            .get("https://api.deezer.com/search/album")
            .query(&[("q", query.as_str()), ("limit", "5")])
            .send()
            .await?
            .error_for_status()?
            .json::<SearchResponse>()
            .await?;

        let mut out = Vec::new();
        for album in resp.data {
            let base = if album.cover_xl.is_empty() {
                &album.cover_big
            } else {
                &album.cover_xl
            };
            if base.is_empty() {
                continue;
            }

            let url = resize(base, self.size).unwrap_or_else(|| base.clone());
            out.push(
                ArtRef::new(Locator::Url(url), "deezer")
                    .with_claim(album.artist.name, album.title)
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
    fn rewrites_cdn_dimensions() {
        let url = "https://cdn-images.dzcdn.net/images/cover/df8e1ba1/500x500-000000-80-0-0.jpg";
        assert_eq!(
            resize(url, 1200).unwrap(),
            "https://cdn-images.dzcdn.net/images/cover/df8e1ba1/1200x1200-000000-80-0-0.jpg"
        );
    }

    #[test]
    fn leaves_unrecognized_urls_alone() {
        assert_eq!(resize("https://example.com/cover.jpg", 1200), None);
    }

    #[test]
    fn clamps_to_the_measured_cdn_ceiling() {
        // Asking for 2000 would silently yield 1200, so promising 2000 to the
        // resolution gate would be a lie.
        assert_eq!(Deezer::new(2000).size, CDN_MAX);
        assert_eq!(Deezer::new(800).size, 800);
    }
}
