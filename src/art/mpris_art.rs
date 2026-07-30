//! The `mpris:artUrl` the player already gave us.
//!
//! Zero latency, no network for local files, and structurally incapable of
//! naming the wrong album — so it carries no claim and skips verification. Its
//! weakness is resolution: players and servers frequently expose a small
//! thumbnail (Navidrome via Feishin serves 300x300 regardless of the `size`
//! parameter), which is exactly what the resolution gate is for.

use super::{ArtRef, ArtSource, Ctx, Locator};
use crate::track::TrackInfo;
use crate::util;
use anyhow::Result;
use async_trait::async_trait;

pub struct MprisArt;

#[async_trait]
impl ArtSource for MprisArt {
    fn name(&self) -> &'static str {
        "mpris"
    }

    async fn find(&self, track: &TrackInfo, _ctx: &Ctx) -> Result<Vec<ArtRef>> {
        let Some(url) = track.art_url.as_deref().filter(|u| !u.is_empty()) else {
            return Ok(Vec::new());
        };

        let locator = if let Some(path) = util::file_url_to_path(url) {
            if !path.exists() {
                tracing::debug!(path = %path.display(), "artUrl points at a missing file");
                return Ok(Vec::new());
            }
            Locator::File(path)
        } else if url.starts_with("http://") || url.starts_with("https://") {
            Locator::Url(url.to_string())
        } else {
            tracing::debug!(url, "unsupported artUrl scheme");
            return Ok(Vec::new());
        };

        Ok(vec![ArtRef::new(locator, "mpris")])
    }

    fn needs_search_metadata(&self) -> bool {
        false
    }
}
