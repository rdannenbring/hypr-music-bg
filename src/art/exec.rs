//! Escape hatch: shell out to any command that prints an image path.
//!
//! This is how anything not implemented natively gets plugged in — sacad,
//! beets' fetchart, a script hitting a private server — without this project
//! having to track every catalogue API. The command receives `HMB_ARTIST`,
//! `HMB_ALBUM_ARTIST`, `HMB_ALBUM` and `HMB_TITLE`, and prints one path per
//! line on stdout. A non-zero exit is treated as "nothing found", not an error.
//!
//! Results are trusted and carry no claim: you chose the command, so verifying
//! its output against the metadata is your call, not ours.

use super::{ArtRef, ArtSource, Ctx, Locator};
use crate::track::TrackInfo;
use anyhow::Result;
use async_trait::async_trait;
use std::path::PathBuf;

pub struct ExecSource {
    command: String,
    args: Vec<String>,
}

impl ExecSource {
    pub fn new(command: &str, args: &[String]) -> Self {
        Self {
            command: command.to_string(),
            args: args.to_vec(),
        }
    }
}

#[async_trait]
impl ArtSource for ExecSource {
    fn name(&self) -> &'static str {
        "exec"
    }

    async fn find(&self, track: &TrackInfo, _ctx: &Ctx) -> Result<Vec<ArtRef>> {
        let output = tokio::process::Command::new(&self.command)
            .args(&self.args)
            .env("HMB_ARTIST", &track.artist)
            .env("HMB_ALBUM_ARTIST", track.search_artist())
            .env("HMB_ALBUM", &track.album)
            .env("HMB_TITLE", &track.title)
            .output()
            .await?;

        if !output.status.success() {
            tracing::debug!(
                command = %self.command,
                status = ?output.status.code(),
                "exec source found nothing"
            );
            return Ok(Vec::new());
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let mut out = Vec::new();
        for line in stdout.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let path = PathBuf::from(line);
            if path.is_file() {
                out.push(ArtRef::new(Locator::File(path), "exec"));
            } else {
                tracing::warn!(path = line, "exec source printed a path that is not a file");
            }
        }

        Ok(out)
    }

    fn needs_search_metadata(&self) -> bool {
        false
    }
}
