//! Editing the config file without destroying it.
//!
//! Serialising a `Config` and writing it back would be far simpler, and would
//! also delete every comment in the file — 128 of them in the shipped example,
//! which is where nearly all the explanation of what the settings *mean* lives.
//! Losing that because someone ticked a checkbox in a GUI is not acceptable, so
//! edits are surgical: `toml_edit` keeps the document's formatting, ordering and
//! comments, and only the specific values being changed are touched.
//!
//! Keys are addressed by dotted path (`art.min_resolution`), and missing tables
//! are created on the way down, so a config that only sets two things does not
//! have to be complete before the GUI can edit it.

use anyhow::{Context, Result};
use std::path::Path;
use toml_edit::{DocumentMut, Item, Table, Value};

/// A config file open for editing.
pub struct ConfigDocument {
    doc: DocumentMut,
    /// The text as read, so a save that changes nothing can be skipped.
    original: String,
}

impl ConfigDocument {
    /// Read a config file, or start an empty document if it does not exist yet.
    pub fn load(path: &Path) -> Result<Self> {
        let original = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(e).context(format!("reading {}", path.display())),
        };

        let doc = original
            .parse::<DocumentMut>()
            .with_context(|| format!("parsing {}", path.display()))?;

        Ok(Self { doc, original })
    }

    pub fn from_text(text: &str) -> Result<Self> {
        Ok(Self {
            doc: text.parse::<DocumentMut>()?,
            original: text.to_string(),
        })
    }

    /// Set a dotted key, creating intermediate tables as needed.
    pub fn set(&mut self, dotted: &str, value: impl Into<Value>) {
        let mut parts: Vec<&str> = dotted.split('.').collect();
        let Some(leaf) = parts.pop() else {
            return;
        };

        let mut table: &mut Table = self.doc.as_table_mut();
        for part in parts {
            let entry = table
                .entry(part)
                .or_insert_with(|| Item::Table(Table::new()));
            match entry.as_table_mut() {
                Some(next) => table = next,
                // A scalar sits where a table should be. Refusing beats
                // clobbering something the user wrote deliberately.
                None => {
                    tracing::warn!(key = dotted, "cannot descend: not a table");
                    return;
                }
            }
        }

        let mut value = value.into();
        match table.get_mut(leaf) {
            Some(existing) => {
                // toml_edit keeps whitespace and any trailing comment in the
                // value's *decor*, so replacing the item wholesale silently
                // deletes the comment sitting beside it. Carry the old decor
                // across. (A test caught this doing exactly that.)
                if let Some(old) = existing.as_value() {
                    *value.decor_mut() = old.decor().clone();
                }
                *existing = Item::Value(value);
            }
            None => {
                table.insert(leaf, Item::Value(value));
            }
        }
    }

    /// Remove a dotted key if present.
    pub fn remove(&mut self, dotted: &str) {
        let mut parts: Vec<&str> = dotted.split('.').collect();
        let Some(leaf) = parts.pop() else {
            return;
        };

        let mut table: &mut Table = self.doc.as_table_mut();
        for part in parts {
            match table.get_mut(part).and_then(Item::as_table_mut) {
                Some(next) => table = next,
                None => return,
            }
        }
        table.remove(leaf);
    }

    pub fn get_str(&self, dotted: &str) -> Option<String> {
        self.lookup(dotted)?.as_str().map(str::to_string)
    }

    pub fn get_i64(&self, dotted: &str) -> Option<i64> {
        self.lookup(dotted)?.as_integer()
    }

    pub fn get_f64(&self, dotted: &str) -> Option<f64> {
        self.lookup(dotted)?.as_float()
    }

    pub fn get_bool(&self, dotted: &str) -> Option<bool> {
        self.lookup(dotted)?.as_bool()
    }

    fn lookup(&self, dotted: &str) -> Option<&Item> {
        let mut item: &Item = self.doc.as_item();
        for part in dotted.split('.') {
            item = item.as_table_like()?.get(part)?;
        }
        Some(item)
    }

    // --- the source chain -------------------------------------------------
    //
    // `[[art.source]]` is an array of tables, so reordering means moving whole
    // table entries rather than scalar values. Kept here, tested, rather than
    // inline in the GUI: getting it wrong silently reorders or drops a user's
    // source chain, which is the one thing in this config that is laborious to
    // reconstruct by hand.

    fn sources(&self) -> Option<&toml_edit::ArrayOfTables> {
        self.doc
            .get("art")?
            .as_table_like()?
            .get("source")?
            .as_array_of_tables()
    }

    fn sources_mut(&mut self) -> Option<&mut toml_edit::ArrayOfTables> {
        self.doc
            .get_mut("art")?
            .as_table_like_mut()?
            .get_mut("source")?
            .as_array_of_tables_mut()
    }

    /// Source names in configured order.
    pub fn source_names(&self) -> Vec<String> {
        self.sources()
            .map(|array| {
                array
                    .iter()
                    .map(|t| {
                        t.get("name")
                            .and_then(|v| v.as_str())
                            .unwrap_or("(unnamed)")
                            .to_string()
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Swap two entries, keeping each one's own settings with it.
    pub fn swap_sources(&mut self, a: usize, b: usize) {
        let Some(array) = self.sources_mut() else {
            return;
        };
        if a == b || a >= array.len() || b >= array.len() {
            return;
        }
        // ArrayOfTables has no swap, so rebuild in the new order. Cloning the
        // tables carries each source's own keys (size, token_env, paths) along
        // with it — losing those on a reorder would be a silent data loss.
        let tables: Vec<Table> = array.iter().cloned().collect();
        let mut reordered = tables;
        reordered.swap(a, b);
        array.clear();
        for table in reordered {
            array.push(table);
        }
    }

    pub fn remove_source(&mut self, index: usize) {
        if let Some(array) = self.sources_mut()
            && index < array.len()
        {
            array.remove(index);
        }
    }

    /// Append a source with only its `name` set, leaving the rest to defaults.
    pub fn add_source(&mut self, name: &str) {
        let art = self
            .doc
            .entry("art")
            .or_insert_with(|| Item::Table(Table::new()));
        let Some(art) = art.as_table_like_mut() else {
            return;
        };
        let entry = art
            .entry("source")
            .or_insert_with(|| Item::ArrayOfTables(toml_edit::ArrayOfTables::new()));
        let Some(array) = entry.as_array_of_tables_mut() else {
            return;
        };
        let mut table = Table::new();
        table.insert("name", Item::Value(name.into()));
        array.push(table);
    }

    /// Set a key on one source entry, e.g. `size` on the Deezer source.
    pub fn set_source_key(&mut self, index: usize, key: &str, value: impl Into<Value>) {
        if let Some(array) = self.sources_mut()
            && let Some(table) = array.get_mut(index)
        {
            table.insert(key, Item::Value(value.into()));
        }
    }

    pub fn source_key_i64(&self, index: usize, key: &str) -> Option<i64> {
        self.sources()?.get(index)?.get(key)?.as_integer()
    }

    pub fn source_key_str(&self, index: usize, key: &str) -> Option<String> {
        self.sources()?
            .get(index)?
            .get(key)?
            .as_str()
            .map(str::to_string)
    }

    /// Replace a string array such as `player.ignore`.
    pub fn set_string_array(&mut self, dotted: &str, values: &[String]) {
        let mut array = toml_edit::Array::new();
        for value in values {
            array.push(value.as_str());
        }
        self.set(dotted, array);
    }

    pub fn get_string_array(&self, dotted: &str) -> Option<Vec<String>> {
        Some(
            self.lookup(dotted)?
                .as_array()?
                .iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect(),
        )
    }

    pub fn to_text(&self) -> String {
        self.doc.to_string()
    }

    pub fn is_modified(&self) -> bool {
        self.to_text() != self.original
    }

    /// Write back, atomically, only if something actually changed.
    ///
    /// The temporary file is created in the same directory so the rename is on
    /// one filesystem and therefore atomic; a crash mid-write cannot leave a
    /// half-written config that would fail to parse on next start.
    pub fn save(&mut self, path: &Path) -> Result<bool> {
        if !self.is_modified() {
            return Ok(false);
        }

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }

        let text = self.to_text();
        // Parse what we are about to write. A GUI bug that produced invalid TOML
        // would otherwise leave the user with a config the daemon cannot load.
        text.parse::<DocumentMut>()
            .context("refusing to write a config that would not parse back")?;

        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, &text).with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))?;

        self.original = text;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ANNOTATED: &str = r#"# hypr-music-bg configuration
#
# Every section is optional.

[art]
# The shortest edge a cover must have. This comment explains the single most
# important setting and must survive an edit.
min_resolution = 600
verify_match = true  # trailing comment

[render]
style = "blur"
"#;

    /// The reason this module exists rather than a serialize-and-write.
    #[test]
    fn editing_preserves_comments_and_layout() {
        let mut doc = ConfigDocument::from_text(ANNOTATED).unwrap();
        doc.set("art.min_resolution", 900);
        let out = doc.to_text();

        assert!(out.contains("min_resolution = 900"), "value must change");
        assert!(
            out.contains("# The shortest edge a cover must have"),
            "the explanatory comment must survive"
        );
        assert!(
            out.contains("# hypr-music-bg configuration"),
            "the header must survive"
        );
        assert!(
            out.contains("# trailing comment"),
            "a trailing comment on an untouched line must survive"
        );
        assert!(out.contains("[render]"), "other sections must survive");
    }

    #[test]
    fn a_trailing_comment_survives_editing_its_own_line() {
        let mut doc = ConfigDocument::from_text(ANNOTATED).unwrap();
        doc.set("art.verify_match", false);
        let out = doc.to_text();
        assert!(out.contains("verify_match = false"));
        assert!(
            out.contains("# trailing comment"),
            "editing a value must not eat the comment beside it: {out}"
        );
    }

    #[test]
    fn missing_tables_are_created() {
        let mut doc = ConfigDocument::from_text(ANNOTATED).unwrap();
        doc.set("theme.mode", "auto");
        let out = doc.to_text();
        assert!(out.contains("[theme]"));
        assert!(out.contains(r#"mode = "auto""#));
        // And the original content is still intact.
        assert!(out.contains("min_resolution = 600"));
    }

    #[test]
    fn values_round_trip_by_type() {
        let mut doc = ConfigDocument::from_text("").unwrap();
        doc.set("art.min_resolution", 1200_i64);
        doc.set("art.match_threshold", 0.75_f64);
        doc.set("art.verify_match", true);
        doc.set("render.style", "fill");

        assert_eq!(doc.get_i64("art.min_resolution"), Some(1200));
        assert_eq!(doc.get_f64("art.match_threshold"), Some(0.75));
        assert_eq!(doc.get_bool("art.verify_match"), Some(true));
        assert_eq!(doc.get_str("render.style"), Some("fill".into()));
    }

    #[test]
    fn removing_a_key_leaves_the_rest_alone() {
        let mut doc = ConfigDocument::from_text(ANNOTATED).unwrap();
        doc.remove("art.verify_match");
        let out = doc.to_text();
        assert!(!out.contains("verify_match"));
        assert!(out.contains("min_resolution = 600"));
        assert!(out.contains("# The shortest edge"));
    }

    #[test]
    fn an_unchanged_document_is_not_rewritten() {
        let mut doc = ConfigDocument::from_text(ANNOTATED).unwrap();
        assert!(!doc.is_modified());

        let path = std::env::temp_dir().join(format!("hmb-cfg-{}.toml", std::process::id()));
        // save() reports whether it wrote, so an unmodified document must not
        // touch the file's mtime at all.
        assert!(!doc.save(&path).unwrap());
        assert!(!path.exists(), "nothing should have been written");
    }

    /// Refusing to descend beats silently replacing something deliberate.
    #[test]
    fn a_scalar_where_a_table_belongs_is_not_clobbered() {
        let mut doc = ConfigDocument::from_text("art = 5\n").unwrap();
        doc.set("art.min_resolution", 900);
        assert_eq!(doc.to_text(), "art = 5\n");
    }

    const WITH_SOURCES: &str = r#"[art]
min_resolution = 600

# The player already handed us this one.
[[art.source]]
name = "mpris"

[[art.source]]
name = "deezer"
size = 1200

[[art.source]]
name = "coverartarchive"
size = 1200
"#;

    #[test]
    fn reads_the_source_chain_in_order() {
        let doc = ConfigDocument::from_text(WITH_SOURCES).unwrap();
        assert_eq!(doc.source_names(), ["mpris", "deezer", "coverartarchive"]);
    }

    /// Reordering must carry each source's own settings with it. Dropping them
    /// would silently reset sizes and credentials on a drag.
    #[test]
    fn reordering_keeps_each_sources_own_settings() {
        let mut doc = ConfigDocument::from_text(WITH_SOURCES).unwrap();
        assert_eq!(doc.source_key_i64(1, "size"), Some(1200));

        doc.swap_sources(0, 1);
        assert_eq!(doc.source_names(), ["deezer", "mpris", "coverartarchive"]);
        // deezer moved to index 0 and kept its size; mpris never had one.
        assert_eq!(doc.source_key_i64(0, "size"), Some(1200));
        assert_eq!(doc.source_key_i64(1, "size"), None);
    }

    #[test]
    fn out_of_range_reorders_are_ignored() {
        let mut doc = ConfigDocument::from_text(WITH_SOURCES).unwrap();
        doc.swap_sources(0, 99);
        doc.swap_sources(2, 2);
        assert_eq!(doc.source_names(), ["mpris", "deezer", "coverartarchive"]);
    }

    #[test]
    fn sources_can_be_added_and_removed() {
        let mut doc = ConfigDocument::from_text(WITH_SOURCES).unwrap();
        doc.add_source("itunes");
        assert_eq!(doc.source_names().last().unwrap(), "itunes");

        doc.remove_source(0);
        assert_eq!(doc.source_names(), ["deezer", "coverartarchive", "itunes"]);
        // Untouched settings survive the removal.
        assert_eq!(doc.source_key_i64(0, "size"), Some(1200));
    }

    #[test]
    fn a_source_added_to_an_empty_config_creates_the_array() {
        let mut doc = ConfigDocument::from_text("").unwrap();
        doc.add_source("mpris");
        assert_eq!(doc.source_names(), ["mpris"]);
        assert!(doc.to_text().contains("[[art.source]]"));
    }

    #[test]
    fn string_arrays_round_trip() {
        let mut doc = ConfigDocument::from_text("").unwrap();
        doc.set_string_array("player.ignore", &["chromium".into(), "firefox".into()]);
        assert_eq!(
            doc.get_string_array("player.ignore"),
            Some(vec!["chromium".to_string(), "firefox".to_string()])
        );
    }

    #[test]
    fn saving_writes_atomically_and_leaves_no_temp_file() {
        let dir = std::env::temp_dir().join(format!("hmb-save-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");

        let mut doc = ConfigDocument::from_text(ANNOTATED).unwrap();
        doc.set("art.min_resolution", 800);
        assert!(doc.save(&path).unwrap());

        let written = std::fs::read_to_string(&path).unwrap();
        assert!(written.contains("min_resolution = 800"));
        assert!(written.contains("# The shortest edge"));
        assert!(
            !dir.join("config.toml.tmp").exists(),
            "temp file must be gone"
        );

        // A second save with no further change must be a no-op.
        assert!(!doc.save(&path).unwrap());

        std::fs::remove_dir_all(&dir).ok();
    }
}
