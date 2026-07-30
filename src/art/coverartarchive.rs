//! MusicBrainz release-group lookup, then Cover Art Archive.
//!
//! Keyless and high quality, at the cost of two hops and MusicBrainz's
//! one-request-per-second budget. The pre-rendered thumbnails come in 250, 500
//! and 1200; `size = 0` fetches the original upload instead, which is often
//! 1000-3000px but is a larger download with no size guarantee.

use super::musicbrainz;
use super::{ArtRef, ArtSource, Ctx, Locator};
use crate::track::TrackInfo;
use anyhow::Result;
use async_trait::async_trait;

pub struct CoverArtArchive {
    size: u32,
}

impl CoverArtArchive {
    pub fn new(size: u32) -> Self {
        Self { size }
    }

    /// CAA only renders these thumbnail sizes; anything else 404s.
    fn endpoint(&self, mbid: &str) -> String {
        let base = format!("https://coverartarchive.org/release-group/{mbid}");
        match self.size {
            0 => format!("{base}/front"),
            s if s <= 250 => format!("{base}/front-250"),
            s if s <= 500 => format!("{base}/front-500"),
            s if s <= 1200 => format!("{base}/front-1200"),
            // Larger than any thumbnail, so the original is the only way to
            // possibly satisfy it.
            _ => format!("{base}/front"),
        }
    }
}

#[async_trait]
impl ArtSource for CoverArtArchive {
    fn name(&self) -> &'static str {
        "coverartarchive"
    }

    async fn find(&self, track: &TrackInfo, ctx: &Ctx) -> Result<Vec<ArtRef>> {
        let Some(rg) = musicbrainz::find_release_group(&ctx.http, track).await? else {
            return Ok(Vec::new());
        };

        tracing::debug!(mbid = %rg.id, title = %rg.title, "musicbrainz release group");

        // Dimensions are deliberately left undeclared: `front-1200` bounds the
        // *long* edge, so a non-square scan can come back shorter than asked.
        // The engine probes the real size after download anyway.
        Ok(vec![
            ArtRef::new(Locator::Url(self.endpoint(&rg.id)), "coverartarchive")
                .with_claim(rg.artist, rg.title),
        ])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_requested_size_to_a_real_endpoint() {
        let mbid = "cdd5b9f6-ebf3-3ada-b421-3c65a3e0ebff";
        assert!(CoverArtArchive::new(0).endpoint(mbid).ends_with("/front"));
        assert!(
            CoverArtArchive::new(250)
                .endpoint(mbid)
                .ends_with("/front-250")
        );
        assert!(
            CoverArtArchive::new(600)
                .endpoint(mbid)
                .ends_with("/front-1200")
        );
        assert!(
            CoverArtArchive::new(1200)
                .endpoint(mbid)
                .ends_with("/front-1200")
        );
        // Above every thumbnail, so only the original can satisfy it.
        assert!(
            CoverArtArchive::new(2000)
                .endpoint(mbid)
                .ends_with("/front")
        );
    }
}
