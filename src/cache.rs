//! On-disk cache for fetched artwork.
//!
//! Keyed on the locator (URL or path) rather than the album, so the same image
//! is not re-downloaded when it is reachable from more than one source. Album
//! *lookups* are cached separately by the engine so that replaying a record
//! does not re-hit MusicBrainz's one-request-per-second budget.

use anyhow::{Context, Result};
use std::collections::{HashMap, HashSet};
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

/// Bounds on how large the cache may grow.
#[derive(Debug, Clone, Copy)]
pub struct CacheLimits {
    /// Total budget for cached art and renders. 0 disables the size limit.
    pub max_bytes: u64,
    /// Age after which an entry is dropped regardless of budget. 0 disables.
    pub max_age_secs: u64,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct PruneReport {
    pub removed: usize,
    pub freed: u64,
}

impl Cache {
    /// Drop old and excess entries.
    ///
    /// Two passes: anything past `max_age_secs` goes first, then oldest-first
    /// until the total fits `max_bytes`.
    ///
    /// `protected` must contain every render currently on screen. Those files
    /// are what the compositor is displaying — DMS and swww hold a path, not a
    /// copy — so deleting one breaks the live wallpaper rather than merely
    /// costing a re-render. In practice renders dominate the cache (measured:
    /// 116 MB of 126 MB), so they are exactly what a size limit wants to
    /// reclaim, which makes the exclusion load-bearing rather than theoretical.
    ///
    /// `original.json` is untouched: it lives at the cache root rather than in a
    /// pruned subdirectory, and losing it would make restore impossible forever.
    pub fn prune(&self, limits: &CacheLimits, protected: &HashSet<PathBuf>) -> PruneReport {
        let mut entries = Vec::new();
        for sub in ["art", "render", "lookup", "negative"] {
            let Ok(dir) = std::fs::read_dir(self.root.join(sub)) else {
                continue;
            };
            for entry in dir.flatten() {
                let path = entry.path();
                if protected.contains(&path) {
                    continue;
                }
                let Ok(meta) = entry.metadata() else { continue };
                if !meta.is_file() {
                    continue;
                }
                let age = meta
                    .modified()
                    .ok()
                    .and_then(|m| m.elapsed().ok())
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                entries.push((path, meta.len(), age));
            }
        }

        let mut report = PruneReport::default();

        // Age pass.
        if limits.max_age_secs > 0 {
            entries.retain(|(path, size, age)| {
                if *age <= limits.max_age_secs {
                    return true;
                }
                if std::fs::remove_file(path).is_ok() {
                    report.removed += 1;
                    report.freed += size;
                }
                false
            });
        }

        // Size pass, oldest first.
        if limits.max_bytes > 0 {
            let mut total: u64 = entries.iter().map(|(_, size, _)| *size).sum();
            if total > limits.max_bytes {
                entries.sort_by_key(|(_, _, age)| std::cmp::Reverse(*age));
                for (path, size, _) in &entries {
                    if total <= limits.max_bytes {
                        break;
                    }
                    if std::fs::remove_file(path).is_ok() {
                        total = total.saturating_sub(*size);
                        report.removed += 1;
                        report.freed += size;
                    }
                }
            }
        }

        if report.removed > 0 {
            tracing::info!(
                removed = report.removed,
                freed_kib = report.freed / 1024,
                "pruned cache"
            );
        }
        report
    }

    pub fn total_bytes(&self) -> u64 {
        ["art", "render", "lookup", "negative"]
            .iter()
            .filter_map(|sub| std::fs::read_dir(self.root.join(sub)).ok())
            .flat_map(|dir| dir.flatten())
            .filter_map(|entry| entry.metadata().ok())
            .filter(|meta| meta.is_file())
            .map(|meta| meta.len())
            .sum()
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

    /// Build a cache with files of known size and age.
    fn seeded(name: &str, files: &[(&str, usize, u64)]) -> (Cache, PathBuf) {
        let root = std::env::temp_dir().join(format!("hmb-prune-{}-{name}", std::process::id()));
        std::fs::remove_dir_all(&root).ok();
        let cache = Cache::new(&root).unwrap();

        for (rel, size, age_secs) in files {
            let path = root.join(rel);
            std::fs::write(&path, vec![0u8; *size]).unwrap();
            // Backdate via mtime, which is what prune reads.
            let when = std::time::SystemTime::now() - std::time::Duration::from_secs(*age_secs);
            let file = std::fs::File::options().write(true).open(&path).unwrap();
            file.set_modified(when).unwrap();
        }
        (cache, root)
    }

    /// The dangerous case. Renders dominate the cache, so a size limit will
    /// reach for them first — but the compositor holds the *path* of the one on
    /// screen, so evicting it breaks the live wallpaper rather than costing a
    /// re-render.
    #[test]
    fn never_evicts_the_wallpaper_that_is_on_screen() {
        let (cache, root) = seeded(
            "protected",
            &[
                ("render/live.png", 4000, 9999), // oldest, so first in line
                ("render/stale.png", 4000, 5000),
                ("art/old", 4000, 8000),
            ],
        );

        let live = root.join("render/live.png");
        let protected = HashSet::from([live.clone()]);

        // Budget far below the total, forcing eviction of everything it may touch.
        let report = cache.prune(
            &CacheLimits {
                max_bytes: 1000,
                max_age_secs: 0,
            },
            &protected,
        );

        assert!(live.exists(), "the applied wallpaper must survive pruning");
        assert!(!root.join("render/stale.png").exists());
        assert!(!root.join("art/old").exists());
        assert_eq!(report.removed, 2);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn evicts_oldest_first_until_within_budget() {
        let (cache, root) = seeded(
            "budget",
            &[
                ("art/newest", 1000, 10),
                ("art/middle", 1000, 500),
                ("art/oldest", 1000, 5000),
            ],
        );

        cache.prune(
            &CacheLimits {
                max_bytes: 2000,
                max_age_secs: 0,
            },
            &HashSet::new(),
        );

        assert!(!root.join("art/oldest").exists(), "oldest goes first");
        assert!(root.join("art/middle").exists());
        assert!(root.join("art/newest").exists());
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn age_limit_applies_regardless_of_budget() {
        let (cache, root) = seeded("age", &[("art/ancient", 10, 100_000), ("art/fresh", 10, 5)]);

        // Budget is generous, so only the age rule can remove anything.
        cache.prune(
            &CacheLimits {
                max_bytes: 10_000_000,
                max_age_secs: 60_000,
            },
            &HashSet::new(),
        );

        assert!(!root.join("art/ancient").exists());
        assert!(root.join("art/fresh").exists());
        std::fs::remove_dir_all(&root).ok();
    }

    /// Losing this file makes restoring the pre-daemon wallpaper impossible
    /// forever, so it must survive even the most aggressive prune.
    #[test]
    fn never_touches_the_saved_original_wallpaper() {
        let (cache, root) = seeded("originals", &[("art/big", 9999, 9999)]);
        let mut originals = HashMap::new();
        originals.insert("DP-1".to_string(), PathBuf::from("/mnt/nfs/a.jpg"));
        cache.save_originals(&originals);

        cache.prune(
            &CacheLimits {
                max_bytes: 1,
                max_age_secs: 1,
            },
            &HashSet::new(),
        );

        assert_eq!(cache.load_originals(), originals);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn zero_limits_disable_pruning_entirely() {
        let (cache, root) = seeded("disabled", &[("art/ancient", 5000, 999_999)]);
        let report = cache.prune(
            &CacheLimits {
                max_bytes: 0,
                max_age_secs: 0,
            },
            &HashSet::new(),
        );
        assert_eq!(report, PruneReport::default());
        assert!(root.join("art/ancient").exists());
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn hashes_are_stable_and_distinct() {
        assert_eq!(hash_hex("a"), hash_hex("a"));
        assert_ne!(hash_hex("a"), hash_hex("b"));
        assert_eq!(hash_hex("a").len(), 16);
    }
}
