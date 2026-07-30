//! On-disk cache for fetched artwork.
//!
//! Keyed on the locator (URL or path) rather than the album, so the same image
//! is not re-downloaded when it is reachable from more than one source. Album
//! *lookups* are cached separately by the engine so that replaying a record
//! does not re-hit MusicBrainz's one-request-per-second budget.

use anyhow::{Context, Result};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct Cache {
    root: PathBuf,
}

/// A previously resolved lookup, so replaying an album does not re-walk the
/// chain and spend its rate-limited MusicBrainz budget again.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct LookupHit {
    pub locator_key: String,
    pub source: String,
    pub width: u32,
    pub height: u32,
    pub degraded: bool,
}

impl Cache {
    pub fn new(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        std::fs::create_dir_all(root.join("art"))
            .with_context(|| format!("creating cache dir {}", root.display()))?;
        std::fs::create_dir_all(root.join("render"))?;
        std::fs::create_dir_all(root.join("lookup"))?;
        std::fs::create_dir_all(root.join("negative"))?;
        Ok(Self { root })
    }

    /// True if this path is something we produced.
    ///
    /// Load-bearing for restore. The wallpaper the compositor reports at startup
    /// is only the user's own if we have not already replaced it — after any
    /// previous run, querying the backend hands back one of our rendered covers.
    /// Recording that as "the original" would mean restore puts back a stale
    /// album cover forever, and the real wallpaper would be lost after the first
    /// run.
    pub fn is_ours(&self, path: &Path) -> bool {
        path.starts_with(self.root())
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The pre-daemon wallpaper per monitor, persisted so it survives restarts.
    pub fn load_originals(&self) -> HashMap<String, PathBuf> {
        let path = self.root.join("original.json");
        std::fs::read(path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    pub fn save_originals(&self, originals: &HashMap<String, PathBuf>) {
        let path = self.root.join("original.json");
        match serde_json::to_vec_pretty(originals) {
            Ok(bytes) => {
                if let Err(e) = write_atomic(&path, &bytes) {
                    tracing::warn!(error = %e, "failed to persist original wallpapers");
                }
            }
            Err(e) => tracing::warn!(error = %e, "failed to encode original wallpapers"),
        }
    }

    /// Remember which source won for an album, and at what size.
    pub fn put_lookup(&self, album_key: &str, hit: &LookupHit) {
        let path = self.root.join("lookup").join(hash_hex(album_key));
        match serde_json::to_vec(hit) {
            Ok(bytes) => {
                if let Err(e) = write_atomic(&path, &bytes) {
                    tracing::warn!(error = %e, "failed to record lookup");
                }
            }
            Err(e) => tracing::warn!(error = %e, "failed to encode lookup"),
        }
    }

    pub fn get_lookup(&self, album_key: &str) -> Option<LookupHit> {
        let path = self.root.join("lookup").join(hash_hex(album_key));
        let bytes = std::fs::read(path).ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    /// Drop the remembered winner, so the next resolve walks the chain again.
    pub fn forget_lookup(&self, album_key: &str) {
        std::fs::remove_file(self.root.join("lookup").join(hash_hex(album_key))).ok();
    }

    /// Record that nothing anywhere had art for this album.
    pub fn mark_negative(&self, album_key: &str) {
        let path = self.root.join("negative").join(hash_hex(album_key));
        if let Err(e) = write_atomic(&path, b"") {
            tracing::warn!(error = %e, "failed to record negative lookup");
        }
    }

    /// True if this album was marked as having no art within `ttl` seconds.
    /// The marker file's mtime is the timestamp, which avoids serializing
    /// anything and survives restarts.
    pub fn negative_hit(&self, album_key: &str, ttl: u64) -> bool {
        if ttl == 0 {
            return false;
        }
        let path = self.root.join("negative").join(hash_hex(album_key));
        let Ok(meta) = std::fs::metadata(&path) else {
            return false;
        };
        let Ok(modified) = meta.modified() else {
            return false;
        };
        match modified.elapsed() {
            Ok(age) => age.as_secs() < ttl,
            // A marker dated in the future is a clock change, not a hit.
            Err(_) => false,
        }
    }

    pub fn clear_negative(&self, album_key: &str) {
        let path = self.root.join("negative").join(hash_hex(album_key));
        std::fs::remove_file(path).ok();
    }

    pub fn art_path(&self, locator: &str) -> PathBuf {
        self.root.join("art").join(hash_hex(locator))
    }

    pub fn render_path(&self, key: &str) -> PathBuf {
        self.root
            .join("render")
            .join(format!("{}.png", hash_hex(key)))
    }

    pub fn get(&self, locator: &str) -> Option<Vec<u8>> {
        let path = self.art_path(locator);
        std::fs::read(&path).ok().filter(|b| !b.is_empty())
    }

    pub fn put(&self, locator: &str, bytes: &[u8]) {
        let path = self.art_path(locator);
        if let Err(e) = write_atomic(&path, bytes) {
            tracing::warn!(error = %e, path = %path.display(), "failed to write cache entry");
        }
    }
}

/// Write via a temporary file and rename, so a crash mid-write cannot leave a
/// truncated image that later reads would treat as valid cache.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// FNV-1a. Cache keys need to be stable and well-distributed, not
/// cryptographically strong, so this avoids pulling in a hashing crate.
fn hash_hex(s: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.as_bytes() {
        hash ^= u64::from(*b);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bug this guards: after one run, the backend reports our own render
    /// as the current wallpaper. Recording that as "the original" loses the real
    /// wallpaper permanently and makes restore replay a stale album cover.
    #[test]
    fn recognizes_its_own_output() {
        let root = std::env::temp_dir().join(format!("hmb-cache-{}", std::process::id()));
        let cache = Cache::new(&root).unwrap();

        assert!(cache.is_ours(&root.join("render").join("abc.png")));
        assert!(!cache.is_ours(Path::new("/mnt/wallpapers/sunset.jpg")));

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn originals_round_trip() {
        let root = std::env::temp_dir().join(format!("hmb-orig-{}", std::process::id()));
        let cache = Cache::new(&root).unwrap();

        assert!(cache.load_originals().is_empty());

        let mut originals = HashMap::new();
        originals.insert("DP-1".to_string(), PathBuf::from("/mnt/wallpapers/a.jpg"));
        cache.save_originals(&originals);

        assert_eq!(cache.load_originals(), originals);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn hashes_are_stable_and_distinct() {
        assert_eq!(hash_hex("a"), hash_hex("a"));
        assert_ne!(hash_hex("a"), hash_hex("b"));
        assert_eq!(hash_hex("a").len(), 16);
    }
}
