use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde::de::DeserializeOwned;
use serde_json::json;

use super::auth::{device_id, Credentials};
use super::models::{AuthResponse, Item, ItemsResponse};

const CLIENT_NAME: &str = "trayplay";
const CLIENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Containers trayplay can decode locally. The server transcodes anything else.
///
/// Ogg is deliberately absent. It usually carries Opus, which symphonia cannot
/// decode at all, and a container whitelist cannot distinguish Opus from Vorbis.
/// Excluding it costs a re-encode on the rarer Vorbis files but makes Opus play.
const DIRECT_PLAY_CONTAINERS: &str = "flac,mp3,m4a,aac,wav";

/// Codec the server transcodes to when direct play is not possible.
///
/// mp3 is safe here only because playback decodes with `enable_gapless: false`
/// (see player::decoder). With rodio's own decoder, which forces gapless on,
/// symphonia's mp3 demuxer underflows on the Xing/LAME header ffmpeg writes for
/// a streamed transcode.
const TRANSCODE_CODEC: &str = "mp3";
const TRANSCODE_BITRATE: u32 = 320_000;

pub struct Client {
    http: reqwest::Client,
    base: String,
    device_id: String,
    /// None until authenticated; login itself is unauthenticated.
    creds: Option<Credentials>,
}

impl Client {
    pub fn new(base: &str) -> Result<Self> {
        Ok(Self {
            http: build_http()?,
            base: base.trim_end_matches('/').to_string(),
            device_id: device_id()?,
            creds: None,
        })
    }

    pub fn authenticated(creds: Credentials) -> Result<Self> {
        Ok(Self {
            http: build_http()?,
            base: creds.server.trim_end_matches('/').to_string(),
            device_id: device_id()?,
            creds: Some(creds),
        })
    }

    fn creds(&self) -> Result<&Credentials> {
        self.creds
            .as_ref()
            .context("not authenticated, run `trayplay login` first")
    }

    pub fn user_id(&self) -> Result<&str> {
        Ok(self.creds()?.user_id.as_str())
    }

    /// Shares this client's connection pool with the track cache. reqwest
    /// Clients are cheap to clone and clones share the pool.
    pub fn http(&self) -> reqwest::Client {
        self.http.clone()
    }

    /// Jellyfin's own auth scheme. The token is omitted while logging in.
    ///
    /// Device is deliberately the hostname so sessions are identifiable in the
    /// server dashboard.
    fn auth_header(&self) -> String {
        let device = hostname();
        let mut header = format!(
            r#"MediaBrowser Client="{CLIENT_NAME}", Device="{device}", DeviceId="{}", Version="{CLIENT_VERSION}""#,
            self.device_id
        );
        if let Some(creds) = &self.creds {
            header.push_str(&format!(r#", Token="{}""#, creds.token));
        }
        header
    }

    pub async fn login(&mut self, username: &str, password: &str) -> Result<Credentials> {
        let url = format!("{}/Users/AuthenticateByName", self.base);
        let resp = self
            .http
            .post(&url)
            .header("Authorization", self.auth_header())
            .json(&json!({ "Username": username, "Pw": password }))
            .send()
            .await
            .with_context(|| format!("POST {url}"))?;

        // 401 here means bad credentials, which deserves a clearer message than
        // a raw status dump.
        if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
            bail!("authentication rejected: wrong username or password");
        }
        let resp = resp.error_for_status().context("login failed")?;

        let auth: AuthResponse = resp.json().await.context("parsing login response")?;
        let creds = Credentials {
            server: self.base.clone(),
            user_id: auth.user.id,
            username: auth.user.name,
            token: auth.access_token,
        };
        self.creds = Some(creds.clone());
        Ok(creds)
    }

    async fn get_json<T: DeserializeOwned>(&self, path: &str, query: &[(&str, String)]) -> Result<T> {
        let url = format!("{}{}", self.base, path);
        let resp = self
            .http
            .get(&url)
            .header("Authorization", self.auth_header())
            .query(query)
            .send()
            .await
            .with_context(|| format!("GET {url}"))?;

        if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
            bail!("token rejected by server, run `trayplay login` again");
        }
        let resp = resp
            .error_for_status()
            .with_context(|| format!("GET {url}"))?;
        resp.json().await.with_context(|| format!("parsing {url}"))
    }

    /// Random tracks. Jellyfin reshuffles per request, so refills stay varied.
    pub async fn random_tracks(&self, limit: u32) -> Result<Vec<Item>> {
        let resp: ItemsResponse = self
            .get_json(
                "/Items",
                &[
                    ("userId", self.user_id()?.to_string()),
                    ("includeItemTypes", "Audio".into()),
                    ("recursive", "true".into()),
                    ("sortBy", "Random".into()),
                    ("limit", limit.to_string()),
                    // ArtistItems is not a default BaseItemDto field, and a
                    // random queue is exactly where every track has a different
                    // credit: without it the queue page can only fall back to
                    // the album artist. Same reason as in `search`.
                    ("fields", "Container,ArtistItems".into()),
                    // Counting the whole library on every refill is wasted work.
                    ("enableTotalRecordCount", "false".into()),
                ],
            )
            .await?;
        Ok(resp.items)
    }

    /// Mixed-type search across tracks, albums and artists.
    ///
    /// Backs the Library page's filter: an artist name alone is covered by the
    /// loaded artist list, but finding an album or a track by name needs the
    /// server, so the whole filter goes through here instead of a client-side
    /// contains-match.
    pub async fn search(&self, term: &str, limit: u32) -> Result<Vec<Item>> {
        let resp: ItemsResponse = self
            .get_json(
                "/Items",
                &[
                    ("userId", self.user_id()?.to_string()),
                    ("searchTerm", term.to_string()),
                    ("includeItemTypes", "MusicArtist,MusicAlbum,Audio".into()),
                    ("recursive", "true".into()),
                    ("limit", limit.to_string()),
                    // ArtistItems is not a default BaseItemDto field: without
                    // asking for it, a track hit deserializes with an empty
                    // artist_items and the artist fallback in play_search_track
                    // can never fire.
                    ("fields", "Container,ArtistItems".into()),
                    ("enableTotalRecordCount", "false".into()),
                ],
            )
            .await?;
        Ok(resp.items)
    }

    pub async fn artists(&self) -> Result<Vec<Item>> {
        let resp: ItemsResponse = self
            .get_json(
                "/Artists",
                &[
                    ("userId", self.user_id()?.to_string()),
                    ("sortBy", "SortName".into()),
                ],
            )
            .await?;
        Ok(resp.items)
    }

    pub async fn artist_albums(&self, artist_id: &str) -> Result<Vec<Item>> {
        let resp: ItemsResponse = self
            .get_json(
                "/Items",
                &[
                    ("userId", self.user_id()?.to_string()),
                    ("artistIds", artist_id.to_string()),
                    ("includeItemTypes", "MusicAlbum".into()),
                    ("recursive", "true".into()),
                    ("sortBy", "PremiereDate,SortName".into()),
                ],
            )
            .await?;
        Ok(resp.items)
    }

    /// Every track credited to an artist, in name order.
    ///
    /// Used to find tracks that belong to no album: Jellyfin leaves AlbumId
    /// unset for those, so they are invisible in an album list.
    pub async fn artist_tracks(&self, artist_id: &str) -> Result<Vec<Item>> {
        let resp: ItemsResponse = self
            .get_json(
                "/Items",
                &[
                    ("userId", self.user_id()?.to_string()),
                    ("artistIds", artist_id.to_string()),
                    ("includeItemTypes", "Audio".into()),
                    ("recursive", "true".into()),
                    ("sortBy", "SortName".into()),
                    ("fields", "Container,ArtistItems".into()),
                    ("enableTotalRecordCount", "false".into()),
                ],
            )
            .await?;
        Ok(resp.items)
    }

    /// Album tracks in disc/track order.
    pub async fn album_tracks(&self, album_id: &str) -> Result<Vec<Item>> {
        let resp: ItemsResponse = self
            .get_json(
                "/Items",
                &[
                    ("userId", self.user_id()?.to_string()),
                    ("parentId", album_id.to_string()),
                    ("sortBy", "ParentIndexNumber,IndexNumber,SortName".into()),
                    ("fields", "Container,ArtistItems".into()),
                ],
            )
            .await?;
        Ok(resp.items)
    }

    /// One item by id. Nothing calls it today - every page fetches lists - but
    /// it is the natural way to re-resolve a remembered id, which is what a
    /// persisted queue holds.
    #[allow(dead_code)]
    pub async fn item(&self, item_id: &str) -> Result<Item> {
        self.get_json(&format!("/Users/{}/Items/{item_id}", self.user_id()?), &[])
            .await
    }

    /// Playback URL.
    ///
    /// Uses the universal endpoint rather than `/stream?static=true`: static
    /// forbids transcoding, so anything symphonia cannot decode (Opus, WMA, APE,
    /// DSD) simply fails. Here the server direct-plays what is listed in
    /// DIRECT_PLAY_CONTAINERS and transcodes the rest to mp3.
    ///
    /// The token goes in the query string because the cache fetches this URL
    /// without our Authorization header.
    pub fn stream_url(&self, item_id: &str) -> Result<String> {
        let creds = self.creds()?;
        Ok(format!(
            "{base}/Audio/{item_id}/universal\
             ?userId={user}\
             &deviceId={device}\
             &container={containers}\
             &audioCodec={codec}\
             &transcodingContainer={codec}\
             &transcodingProtocol=http\
             &maxStreamingBitrate={bitrate}\
             &enableRedirection=true\
             &api_key={token}",
            base = self.base,
            user = urlencoding::encode(&creds.user_id),
            device = urlencoding::encode(&self.device_id),
            containers = DIRECT_PLAY_CONTAINERS,
            codec = TRANSCODE_CODEC,
            bitrate = TRANSCODE_BITRATE,
            token = urlencoding::encode(&creds.token),
        ))
    }

    /// Raw GET, used for cover art. Kept here so callers do not need the inner
    /// HTTP client just to fetch an image.
    pub async fn fetch_bytes(&self, url: &str) -> Result<Vec<u8>> {
        let resp = self
            .http
            .get(url)
            .header("Authorization", self.auth_header())
            .send()
            .await
            .with_context(|| format!("GET {url}"))?
            .error_for_status()
            .with_context(|| format!("GET {url}"))?;
        Ok(resp.bytes().await?.to_vec())
    }

    pub fn image_url(&self, item_id: &str, tag: &str, max_height: u32) -> String {
        format!(
            "{}/Items/{item_id}/Images/Primary?tag={}&maxHeight={max_height}",
            self.base,
            urlencoding::encode(tag)
        )
    }
}

/// Hostname for the Device field. Read straight from procfs to avoid pulling a
/// crate (or glib) into the client layer for one string.
fn hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|s| s.trim().to_string())
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string())
}

fn build_http() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(format!("{CLIENT_NAME}/{CLIENT_VERSION}"))
        // Generous but finite: a hung server must not wedge playback forever.
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(30))
        .build()
        .context("building HTTP client")
}
