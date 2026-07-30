//! Spotify Web API.
//!
//! Requires registering an app for client credentials, and tops out at 640px —
//! smaller than keyless Deezer. Its value here is metadata precision rather
//! than resolution, so it earns its place as a cross-check or as a fallback for
//! records the open catalogues miss.

use super::{ArtRef, ArtSource, Ctx, Locator};
use crate::track::TrackInfo;
use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use serde::Deserialize;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

pub struct Spotify {
    client_id: String,
    client_secret: String,
    /// Cached bearer token and the moment it stops being valid.
    token: Mutex<Option<(String, Instant)>>,
}

impl Spotify {
    /// Credentials are read from the environment, never from the config file,
    /// so a shared config cannot leak them.
    pub fn new(client_id_env: &str, client_secret_env: &str) -> Result<Self> {
        let client_id =
            std::env::var(client_id_env).with_context(|| format!("${client_id_env} is not set"))?;
        let client_secret = std::env::var(client_secret_env)
            .with_context(|| format!("${client_secret_env} is not set"))?;

        Ok(Self {
            client_id,
            client_secret,
            token: Mutex::new(None),
        })
    }

    async fn bearer(&self, http: &reqwest::Client) -> Result<String> {
        let mut slot = self.token.lock().await;

        if let Some((token, expires)) = slot.as_ref()
            && Instant::now() < *expires
        {
            return Ok(token.clone());
        }

        #[derive(Deserialize)]
        struct TokenResponse {
            access_token: String,
            #[serde(default = "default_expiry")]
            expires_in: u64,
        }
        fn default_expiry() -> u64 {
            3600
        }

        let resp = http
            .post("https://accounts.spotify.com/api/token")
            .basic_auth(&self.client_id, Some(&self.client_secret))
            .form(&[("grant_type", "client_credentials")])
            .send()
            .await?;

        if !resp.status().is_success() {
            return Err(anyhow!("spotify token HTTP {}", resp.status()));
        }

        let parsed: TokenResponse = resp.json().await?;
        // Expire a minute early so a token cannot lapse mid-request.
        let expires = Instant::now() + Duration::from_secs(parsed.expires_in.saturating_sub(60));
        *slot = Some((parsed.access_token.clone(), expires));
        Ok(parsed.access_token)
    }
}

#[derive(Deserialize)]
struct SearchResponse {
    #[serde(default)]
    albums: AlbumPage,
}

#[derive(Deserialize, Default)]
struct AlbumPage {
    #[serde(default)]
    items: Vec<Album>,
}

#[derive(Deserialize)]
struct Album {
    #[serde(default)]
    name: String,
    #[serde(default)]
    images: Vec<Image>,
    #[serde(default)]
    artists: Vec<Artist>,
}

#[derive(Deserialize)]
struct Image {
    url: String,
    #[serde(default)]
    width: u32,
    #[serde(default)]
    height: u32,
}

#[derive(Deserialize)]
struct Artist {
    #[serde(default)]
    name: String,
}

#[async_trait]
impl ArtSource for Spotify {
    fn name(&self) -> &'static str {
        "spotify"
    }

    async fn find(&self, track: &TrackInfo, ctx: &Ctx) -> Result<Vec<ArtRef>> {
        if track.album.trim().is_empty() {
            return Ok(Vec::new());
        }

        let token = self.bearer(&ctx.http).await?;
        let query = format!("album:{} artist:{}", track.album, track.search_artist());

        let resp = ctx
            .http
            .get("https://api.spotify.com/v1/search")
            .bearer_auth(&token)
            .query(&[("q", query.as_str()), ("type", "album"), ("limit", "5")])
            .send()
            .await?
            .error_for_status()?
            .json::<SearchResponse>()
            .await?;

        let mut out = Vec::new();
        for album in resp.albums.items {
            let artist = album
                .artists
                .into_iter()
                .next()
                .map(|a| a.name)
                .unwrap_or_default();

            // Spotify lists images largest first, but sort rather than trust it.
            let mut images = album.images;
            images.sort_by_key(|i| std::cmp::Reverse(i.width.min(i.height)));

            if let Some(image) = images.into_iter().next() {
                out.push(
                    ArtRef::new(Locator::Url(image.url), "spotify")
                        .with_claim(artist, album.name)
                        .with_declared(image.width, image.height),
                );
            }
        }

        Ok(out)
    }
}
