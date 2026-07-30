//! Shared MusicBrainz lookups.
//!
//! Both Cover Art Archive and fanart.tv are keyed on MusicBrainz IDs, so the
//! lookup and its rate limiting live here rather than being duplicated.
//! MusicBrainz asks for at most one request per second per client; exceeding
//! that gets a client blocked, so the limiter is process-global rather than
//! per-source.

use crate::track::TrackInfo;
use anyhow::{Result, anyhow};
use serde::Deserialize;
use std::sync::OnceLock;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

const MIN_INTERVAL: Duration = Duration::from_millis(1100);

fn gate() -> &'static Mutex<Option<Instant>> {
    static GATE: OnceLock<Mutex<Option<Instant>>> = OnceLock::new();
    GATE.get_or_init(|| Mutex::new(None))
}

/// Block until at least `MIN_INTERVAL` has passed since the previous call.
async fn throttle() {
    let mut last = gate().lock().await;
    if let Some(prev) = *last {
        let elapsed = prev.elapsed();
        if elapsed < MIN_INTERVAL {
            tokio::time::sleep(MIN_INTERVAL - elapsed).await;
        }
    }
    *last = Some(Instant::now());
}

#[derive(Debug, Clone)]
pub struct ReleaseGroup {
    pub id: String,
    pub title: String,
    pub artist: String,
    /// The artist's own MBID, carried along because fanart.tv is keyed on it.
    /// Taking it from this response saves a second rate-limited round trip.
    pub artist_id: Option<String>,
}

#[derive(Deserialize)]
struct RgResponse {
    #[serde(default, rename = "release-groups")]
    release_groups: Vec<RgItem>,
}

#[derive(Deserialize)]
struct RgItem {
    id: String,
    #[serde(default)]
    title: String,
    #[serde(default, rename = "artist-credit")]
    artist_credit: Vec<ArtistCredit>,
}

#[derive(Deserialize)]
struct ArtistCredit {
    #[serde(default)]
    name: String,
    #[serde(default)]
    artist: Option<ArtistRef>,
}

#[derive(Deserialize)]
struct ArtistRef {
    #[serde(default)]
    id: String,
}

/// Find the best-scoring release group for a track.
pub async fn find_release_group(
    http: &reqwest::Client,
    track: &TrackInfo,
) -> Result<Option<ReleaseGroup>> {
    if track.album.trim().is_empty() {
        return Ok(None);
    }

    let query = format!(
        "artist:\"{}\" AND releasegroup:\"{}\"",
        escape_lucene(track.search_artist()),
        escape_lucene(&track.album)
    );

    throttle().await;
    let resp = http
        .get("https://musicbrainz.org/ws/2/release-group/")
        .query(&[("query", query.as_str()), ("fmt", "json"), ("limit", "3")])
        .send()
        .await?;

    if !resp.status().is_success() {
        return Err(anyhow!("musicbrainz HTTP {}", resp.status()));
    }

    let parsed: RgResponse = resp.json().await?;
    Ok(parsed.release_groups.into_iter().next().map(|rg| {
        let credit = rg.artist_credit.into_iter().next();
        ReleaseGroup {
            id: rg.id,
            title: rg.title,
            artist: credit.as_ref().map(|a| a.name.clone()).unwrap_or_default(),
            artist_id: credit
                .and_then(|a| a.artist)
                .map(|a| a.id)
                .filter(|id| !id.is_empty()),
        }
    }))
}

#[derive(Deserialize)]
struct ArtistResponse {
    #[serde(default)]
    artists: Vec<ArtistItem>,
}

#[derive(Deserialize)]
struct ArtistItem {
    id: String,
    #[serde(default)]
    name: String,
}

/// Find the MusicBrainz ID for an artist. fanart.tv is keyed on these.
pub async fn find_artist_mbid(
    http: &reqwest::Client,
    artist: &str,
) -> Result<Option<(String, String)>> {
    if artist.trim().is_empty() {
        return Ok(None);
    }

    throttle().await;
    let resp = http
        .get("https://musicbrainz.org/ws/2/artist/")
        .query(&[
            (
                "query",
                format!("artist:\"{}\"", escape_lucene(artist)).as_str(),
            ),
            ("fmt", "json"),
            ("limit", "1"),
        ])
        .send()
        .await?;

    if !resp.status().is_success() {
        return Err(anyhow!("musicbrainz HTTP {}", resp.status()));
    }

    let parsed: ArtistResponse = resp.json().await?;
    Ok(parsed.artists.into_iter().next().map(|a| (a.id, a.name)))
}

/// MusicBrainz search is Lucene-backed, so an unescaped quote or colon in an
/// album title turns into a syntax error rather than a miss.
fn escape_lucene(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        if matches!(
            ch,
            '+' | '-'
                | '&'
                | '|'
                | '!'
                | '('
                | ')'
                | '{'
                | '}'
                | '['
                | ']'
                | '^'
                | '"'
                | '~'
                | '*'
                | '?'
                | ':'
                | '\\'
                | '/'
        ) {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_lucene_metacharacters() {
        assert_eq!(escape_lucene("AC/DC"), "AC\\/DC");
        assert_eq!(escape_lucene(r#"Say "Hello""#), r#"Say \"Hello\""#);
        assert_eq!(escape_lucene("Plain Title"), "Plain Title");
    }
}
