//! Which build is this, exactly.
//!
//! The daemon and the CLI are separate processes that are replaced
//! independently — a daemon keeps running while `cargo build` swaps the binary
//! the CLI invokes, and an installed copy in `~/.local/bin` drifts from the one
//! in `target/release`. The crate version cannot tell them apart, because it
//! stays the same across every rebuild. The commit and build time can.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct BuildInfo {
    pub version: String,
    /// Short git commit, `-dirty` when built from an uncommitted tree.
    pub commit: String,
    pub branch: String,
    /// UTC, ISO 8601.
    pub built: String,
    /// Absolute path of the running executable, resolved through symlinks.
    pub exe: Option<String>,
}

impl BuildInfo {
    /// The build this process was compiled from.
    pub fn current() -> Self {
        Self {
            version: env!("CARGO_PKG_VERSION").to_string(),
            commit: env!("HMB_GIT_COMMIT").to_string(),
            branch: env!("HMB_GIT_BRANCH").to_string(),
            built: env!("HMB_BUILD_TIME").to_string(),
            // Canonicalised so a symlinked ~/.local/bin entry reports the file it
            // points at, which is the thing that actually differs between builds.
            exe: std::env::current_exe()
                .ok()
                .and_then(|p| std::fs::canonicalize(p).ok())
                .map(|p| p.display().to_string()),
        }
    }

    /// One line, for a menu or a log.
    pub fn summary(&self) -> String {
        format!("{} ({}) built {}", self.version, self.commit, self.built)
    }

    /// How this build relates to another.
    ///
    /// Single source of truth so `about` and `doctor` cannot disagree — they
    /// did, with `doctor` calling two views of one binary a mismatch.
    pub fn compare(&self, other: &Self) -> BuildMatch {
        // Same file, same build time: literally the same image, whatever the
        // commit says. This is what rescues the dirty case, where the commit
        // alone cannot prove sameness but the identity of the binary can.
        if !self.built.is_empty() && self.built == other.built && self.exe == other.exe {
            return BuildMatch::Same;
        }

        // A daemon predating build stamping reports nothing.
        if other.commit.is_empty() {
            return BuildMatch::Different;
        }

        // The suffix says a tree had uncommitted changes but not *which*, so two
        // dirty builds cannot be assumed equal.
        if self.commit.ends_with("-dirty") || other.commit.ends_with("-dirty") {
            return BuildMatch::Indeterminate;
        }

        if self.commit != "unknown" && self.commit == other.commit {
            BuildMatch::Same
        } else {
            BuildMatch::Different
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuildMatch {
    Same,
    /// Cannot be proven either way, because a tree was dirty.
    Indeterminate,
    Different,
}

impl BuildMatch {
    pub fn message(self) -> &'static str {
        match self {
            Self::Same => "Same build.",
            Self::Indeterminate => {
                "One of these was built from an uncommitted tree, so they cannot be \
                 assumed identical."
            }
            Self::Different => "MISMATCH: the daemon is running a different build. Restart it.",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build(commit: &str) -> BuildInfo {
        BuildInfo {
            version: "0.1.0".into(),
            commit: commit.into(),
            branch: "main".into(),
            built: "2026-08-02T02:10:21Z".into(),
            exe: Some("/usr/bin/hypr-music-bg".into()),
        }
    }

    #[test]
    fn the_build_stamp_is_populated() {
        let info = BuildInfo::current();
        assert!(!info.version.is_empty());
        assert!(!info.commit.is_empty(), "build.rs must stamp a commit");
        assert!(!info.built.is_empty(), "build.rs must stamp a build time");
    }

    #[test]
    fn matching_commits_are_the_same_build() {
        let mut a = build("91b626f");
        let mut b = build("91b626f");
        // Different install locations and build times, same commit.
        a.exe = Some("/usr/bin/hypr-music-bg".into());
        b.exe = Some("/home/x/.local/bin/hypr-music-bg".into());
        b.built = "2026-08-02T09:00:00Z".into();
        assert_eq!(a.compare(&b), BuildMatch::Same);
    }

    /// The case this exists for: a daemon left running across a rebuild.
    #[test]
    fn different_commits_are_not() {
        let mut b = build("edd46a8");
        b.built = "2026-07-31T00:25:13Z".into();
        assert_eq!(build("91b626f").compare(&b), BuildMatch::Different);
    }

    /// A dirty tree says nothing about *what* was uncommitted, so two dirty
    /// builds cannot be assumed equal...
    #[test]
    fn dirty_builds_are_indeterminate() {
        let mut b = build("91b626f-dirty");
        b.built = "2026-08-02T09:00:00Z".into();
        assert_eq!(
            build("91b626f-dirty").compare(&b),
            BuildMatch::Indeterminate
        );
    }

    /// ...unless they are demonstrably the same file, built at the same instant,
    /// which is the common case of a client talking to a daemon it launched.
    /// `doctor` used to call this a mismatch.
    #[test]
    fn the_same_binary_matches_even_when_dirty() {
        let a = build("91b626f-dirty");
        let b = a.clone();
        assert_eq!(a.compare(&b), BuildMatch::Same);
    }

    #[test]
    fn a_daemon_predating_build_stamping_is_different() {
        let mut old = build("");
        old.built = String::new();
        assert_eq!(build("91b626f").compare(&old), BuildMatch::Different);
    }
}
