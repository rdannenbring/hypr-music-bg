//! Subsonic-compatible servers: Navidrome, Airsonic, Gonic.
//!
//! For a player that already exposes the server's cover URL over MPRIS this is
//! redundant — measured against a Navidrome instance, the bytes are identical.
//! It earns its place when the player does not surface art at all (mpv on an
//! NFS mount, a terminal client), or when you want your own library to win over
//! the public catalogues.
//!
//! Worth knowing: Subsonic's `size` parameter only ever scales *down*. If the
//! stored art is 300x300, `size=1200` returns 300x300 — verified byte-identical
//! against a live Navidrome. The resolution gate handles this by falling
//! through to a source that has something bigger.

use super::{ArtRef, ArtSource, Ctx, Locator};
use crate::track::TrackInfo;
use anyhow::{Context, Result};
use async_trait::async_trait;
use md5::{Digest, Md5};
use serde::Deserialize;

pub struct Subsonic {
    base_url: String,
    username: String,
    password: String,
    size: Option<u32>,
}

impl Subsonic {
    pub fn new(url: &str, username: &str, password_env: &str, size: Option<u32>) -> Result<Self> {
        let password =
            std::env::var(password_env).with_context(|| format!("${password_env} is not set"))?;
        Ok(Self {
            base_url: url.trim_end_matches('/').to_string(),
            username: username.to_string(),
            password,
            size,
        })
    }

    /// Subsonic's token scheme: `t = md5(password + salt)`, so the password
    /// itself never crosses the wire.
    fn auth_params(&self) -> Vec<(String, String)> {
        // A per-process salt is sufficient here — the point of the salt is to
        // keep the password out of the URL, not to defeat replay.
        let salt = format!("{:x}", std::process::id() as u64 ^ crate::util::now_secs());
        let mut hasher = Md5::new();
        hasher.update(self.password.as_bytes());
        hasher.update(salt.as_bytes());
        let token = hasher
            .finalize()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>();

        vec![
            ("u".into(), self.username.clone()),
            ("t".into(), token),
            ("s".into(), salt),
            ("v".into(), "1.16.1".into()),
            ("c".into(), "hypr-music-bg".into()),
            ("f".into(), "json".into()),
        ]
    }
}

#[derive(Deserialize)]
struct Envelope {
    #[serde(rename = "subsonic-response")]
    response: SearchBody,
}

#[derive(Deserialize)]
struct SearchBody {
    #[serde(default, rename = "searchResult3")]
    search_result3: SearchResult,
}

#[derive(Deserialize, Default)]
struct SearchResult {
    #[serde(default)]
    album: Vec<Album>,
}

#[derive(Deserialize)]
struct Album {
    #[serde(default)]
    name: String,
    #[serde(default)]
    artist: String,
    #[serde(default, rename = "coverArt")]
    cover_art: String,
}

#[async_trait]
impl ArtSource for Subsonic {
    fn name(&self) -> &'static str {
        "subsonic"
    }

    async fn find(&self, track: &TrackInfo, ctx: &Ctx) -> Result<Vec<ArtRef>> {
        if track.album.trim().is_empty() {
            return Ok(Vec::new());
        }

        let mut params = self.auth_params();
        params.push(("query".into(), track.album.clone()));
        params.push(("albumCount".into(), "5".into()));
        params.push(("songCount".into(), "0".into()));
        params.push(("artistCount".into(), "0".into()));

        let envelope: Envelope = ctx
            .http
            .get(format!("{}/rest/search3.view", self.base_url))
            .query(&params)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;

        let mut out = Vec::new();
        for album in envelope.response.search_result3.album {
            if album.cover_art.is_empty() {
                continue;
            }

            let mut params = self.auth_params();
            params.push(("id".into(), album.cover_art.clone()));
            if let Some(size) = self.size {
                params.push(("size".into(), size.to_string()));
            }

            let query = params
                .iter()
                .map(|(k, v)| format!("{k}={}", urlencode(v)))
                .collect::<Vec<_>>()
                .join("&");

            out.push(
                ArtRef::new(
                    Locator::Url(format!("{}/rest/getCoverArt.view?{query}", self.base_url)),
                    "subsonic",
                )
                .with_claim(album.artist, album.name),
            );
        }

        Ok(out)
    }
}

fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for byte in s.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_reserved_characters() {
        assert_eq!(urlencode("Devil's Night"), "Devil%27s%20Night");
        assert_eq!(urlencode("plain-name_1.0~"), "plain-name_1.0~");
    }

    #[test]
    fn auth_never_includes_the_password() {
        // The token scheme exists so the password stays off the wire; a
        // regression here would put it in every request URL.
        unsafe { std::env::set_var("HMB_TEST_SUBSONIC_PW", "hunter2") };
        let s = Subsonic::new(
            "https://music.example.com",
            "rob",
            "HMB_TEST_SUBSONIC_PW",
            None,
        )
        .unwrap();
        let params = s.auth_params();
        assert!(!params.iter().any(|(_, v)| v.contains("hunter2")));
        assert!(params.iter().any(|(k, _)| k == "t"));
        assert!(params.iter().any(|(k, _)| k == "s"));
    }
}
