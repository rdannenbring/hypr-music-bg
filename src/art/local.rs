//! Cover files already on disk.
//!
//! Two ways in: the directory holding the audio file the player told us about
//! (`xesam:url`), which is exact; and configured library roots searched by
//! artist/album, which is a guess but a cheap one. Both are trusted — a file
//! sitting next to the track is not going to be a different record — so
//! neither carries a claim.

use super::{ArtRef, ArtSource, Ctx, Locator};
use crate::config::expand_tilde;
use crate::track::TrackInfo;
use crate::util;
use anyhow::Result;
use async_trait::async_trait;
use std::path::{Path, PathBuf};

const STEMS: [&str; 7] = [
    "cover", "folder", "front", "album", "albumart", "artwork", "art",
];
const EXTS: [&str; 5] = ["jpg", "jpeg", "png", "webp", "bmp"];

pub struct LocalArt {
    roots: Vec<PathBuf>,
}

impl LocalArt {
    pub fn new(paths: &[String]) -> Self {
        Self {
            roots: paths.iter().map(|p| expand_tilde(p)).collect(),
        }
    }
}

/// Look for `cover.jpg`, `Folder.png`, and friends directly inside `dir`.
fn scan_dir(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };

    let mut found = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let Some(ext) = path.extension().and_then(|s| s.to_str()) else {
            continue;
        };

        // Case-insensitive: real libraries contain Folder.jpg, cover.JPG, and
        // every combination in between.
        let stem = stem.to_ascii_lowercase();
        let ext = ext.to_ascii_lowercase();
        if STEMS.contains(&stem.as_str()) && EXTS.contains(&ext.as_str()) {
            found.push(path);
        }
    }

    // Stable ordering so the same album does not pick a different file between
    // runs, with the STEMS priority respected.
    found.sort_by_key(|p| {
        let stem = p
            .file_stem()
            .and_then(|s| s.to_str())
            .map(|s| s.to_ascii_lowercase())
            .unwrap_or_default();
        STEMS.iter().position(|s| *s == stem).unwrap_or(usize::MAX)
    });
    found
}

#[async_trait]
impl ArtSource for LocalArt {
    fn name(&self) -> &'static str {
        "local"
    }

    async fn find(&self, track: &TrackInfo, _ctx: &Ctx) -> Result<Vec<ArtRef>> {
        let mut dirs: Vec<PathBuf> = Vec::new();

        // Exact: the folder the playing file lives in.
        if let Some(url) = track.track_url.as_deref()
            && let Some(path) = util::file_url_to_path(url)
            && let Some(parent) = path.parent()
        {
            dirs.push(parent.to_path_buf());
        }

        // Guessed: the conventional <root>/<artist>/<album> layout.
        for root in &self.roots {
            for artist in [track.search_artist(), &track.artist] {
                if artist.trim().is_empty() {
                    continue;
                }
                let candidate = root.join(artist).join(&track.album);
                if candidate.is_dir() {
                    dirs.push(candidate);
                }
            }
        }

        // Not `Vec::dedup`, which only collapses *consecutive* duplicates: the
        // same folder can be reached both from `xesam:url` and from a configured
        // root without the two landing next to each other. Order is preserved so
        // the exact match from the playing file keeps its priority.
        let mut seen = std::collections::HashSet::new();
        dirs.retain(|dir| seen.insert(dir.clone()));

        let mut out = Vec::new();
        for dir in dirs {
            for path in scan_dir(&dir) {
                out.push(ArtRef::new(Locator::File(path), "local"));
            }
        }

        Ok(out)
    }

    fn needs_search_metadata(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefers_cover_over_other_stems() {
        let dir = std::env::temp_dir().join(format!("hmb-local-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("artwork.png"), b"x").unwrap();
        std::fs::write(dir.join("cover.jpg"), b"x").unwrap();
        std::fs::write(dir.join("notes.txt"), b"x").unwrap();

        let found = scan_dir(&dir);
        std::fs::remove_dir_all(&dir).ok();

        assert_eq!(found.len(), 2, "only image stems should match");
        assert_eq!(found[0].file_name().unwrap(), "cover.jpg");
    }
}
