//! Stamps build identity into the binary.
//!
//! The crate version alone cannot distinguish builds: it stays `0.1.0` across
//! every rebuild during development, so "which binary is running" — the question
//! that actually matters when a daemon keeps running while you rebuild — is
//! unanswerable from it. The commit and build time are what identify a build.

use std::process::Command;

fn main() {
    // Rebuild the stamp when the checkout moves, not just when sources change.
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/index");

    println!("cargo:rustc-env=HMB_GIT_COMMIT={}", git_describe());
    println!("cargo:rustc-env=HMB_GIT_BRANCH={}", git_branch());
    println!("cargo:rustc-env=HMB_BUILD_TIME={}", build_time());
}

/// Short commit, suffixed `-dirty` when the tree had uncommitted changes.
///
/// The suffix matters more than it looks: most confusing "why is my fix not
/// working" moments are a binary built from a dirty tree.
fn git_describe() -> String {
    let Some(commit) = run("git", &["rev-parse", "--short", "HEAD"]) else {
        // Not a git checkout: a release tarball, say.
        return "unknown".into();
    };
    let dirty = run("git", &["status", "--porcelain"])
        .map(|out| !out.trim().is_empty())
        .unwrap_or(false);
    if dirty {
        format!("{commit}-dirty")
    } else {
        commit
    }
}

fn git_branch() -> String {
    run("git", &["rev-parse", "--abbrev-ref", "HEAD"]).unwrap_or_else(|| "unknown".into())
}

/// UTC, ISO 8601. Shells out rather than taking a date-formatting dependency
/// for one string; falls back to the epoch seconds if `date` is unavailable.
fn build_time() -> String {
    run("date", &["-u", "+%Y-%m-%dT%H:%M:%SZ"]).unwrap_or_else(|| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs().to_string())
            .unwrap_or_else(|_| "unknown".into())
    })
}

fn run(program: &str, args: &[&str]) -> Option<String> {
    let output = Command::new(program).args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?.trim().to_string();
    (!text.is_empty()).then_some(text)
}
