use std::collections::HashSet;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde::de::DeserializeOwned;
use serde_json::json;

use super::auth::{device_id, Credentials};
use super::models::{self, AuthResponse, Item, ItemsResponse, Kind, ServerInfo};

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

/// Normalizes a server URL as typed into the login form: trims whitespace,
/// defaults a missing scheme to `https://` (a bare `host:port` is what a user
/// actually types), and trims trailing slashes.
///
/// `Client::new` only trims the trailing slash, so a fully normalized URL is
/// safe to hand it afterwards.
pub fn normalize_base(raw: &str) -> String {
    let trimmed = raw.trim();
    let with_scheme = if trimmed.contains("://") {
        trimmed.to_string()
    } else {
        format!("https://{trimmed}")
    };
    with_scheme.trim_end_matches('/').to_string()
}

/// Unauthenticated `GET /System/Info/Public`, used to check a server URL
/// before a login is attempted.
///
/// Errors are mapped by stage so a typo'd URL gets a useful message:
/// unreachable is different from "this host speaks something else".
pub async fn probe(base: &str) -> Result<ServerInfo> {
    let base = normalize_base(base);
    let url = format!("{base}/System/Info/Public");
    let resp = match build_http()?.get(&url).send().await {
        Ok(resp) => resp,
        // Connection refused, DNS, TLS, timeout: nothing to respond at all.
        Err(err) => bail!("cannot reach {url}: {err}"),
    };
    if !resp.status().is_success() {
        bail!(
            "that does not look like a Jellyfin server (HTTP {})",
            resp.status()
        );
    }
    match resp.json().await {
        Ok(info) => Ok(info),
        // A 200 from something that is not Jellyfin (an app, a portal page).
        Err(err) => {
            tracing::debug!(%err, %url, "probe response did not parse");
            bail!("that does not look like a Jellyfin server");
        }
    }
}

/// The server refused the request's credentials: HTTP 401 on an authenticated
/// request.
///
/// A type rather than a message string so the player and the UI can recognise
/// "must sign in again" without matching text; stream fetches (see
/// `player::cache`) and library queries both map their 401 onto this same
/// error.
#[derive(Debug, Clone, Copy)]
pub struct Unauthorized;

impl std::fmt::Display for Unauthorized {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("credentials rejected by server (401)")
    }
}

impl std::error::Error for Unauthorized {}

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

    /// Returns a clone of the credentials if authenticated.
    pub fn creds_clone(&self) -> Option<Credentials> {
        self.creds.clone()
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
            if let Ok(body) = resp.text().await {
                tracing::debug!("login 401 body: {}", body);
            }
            bail!("wrong username or password");
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

    /// Verifies that the current session token is still valid.
    /// Returns an error if the server rejects it with 401.
    pub async fn validate(&self) -> Result<()> {
        // Test an authenticated endpoint that mirrors the playback auth pattern.
        // /Users/{id}/Items requires auth and uses the same header/creds as stream fetches.
        let creds = self.creds()?;
        tracing::debug!(
            user_id = %creds.user_id,
            token_len = creds.token.len(),
            "validating session token"
        );
        let url = format!(
            "{}/Users/{}/Items?limit=1&fields=ProviderIds",
            self.base, creds.user_id
        );
        let resp = self
            .http
            .get(&url)
            .header("Authorization", self.auth_header())
            .send()
            .await?;

        if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
            if let Ok(body) = resp.text().await {
                tracing::warn!("validate 401 body: {}", body);
            }
            return Err(anyhow::Error::new(Unauthorized)
                .context("token rejected by server"));
        }

        resp.error_for_status().context("validate check failed")?;
        Ok(())
    }

    /// POST with no useful reply, which is every playback-reporting endpoint.
    ///
    /// Errors come back rather than being logged here: the caller knows which
    /// report failed, and none of them is worth interrupting playback for.
    async fn post_json(&self, path: &str, body: serde_json::Value) -> Result<()> {
        let url = format!("{}{}", self.base, path);
        self.http
            .post(&url)
            .header("Authorization", self.auth_header())
            .json(&body)
            .send()
            .await
            .with_context(|| format!("POST {url}"))?
            .error_for_status()
            .with_context(|| format!("POST {url}"))?;
        Ok(())
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
            // Typed, not a bare message: the player checks for this error to
            // know the session has to be recreated. The human-readable part
            // stays for CLI callers of this method.
            return Err(anyhow::Error::new(Unauthorized)
                .context("token rejected by server, run `trayplay login` again"));
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

    /// Mixed-type search across artists, albums and tracks.
    ///
    /// Backs the Library page's filter: an artist name alone is covered by the
    /// loaded artist list, but finding an album or a track by name needs the
    /// server, so the whole filter goes through here instead of a client-side
    /// contains-match.
    ///
    /// **Two endpoints, and both are needed.** `/Items` does not answer for
    /// artists whatever `includeItemTypes` says: a `MusicArtist` is not a child
    /// of a library folder, so a recursive query never reaches one. And its
    /// `searchTerm` matches item *names*, not performer credits, so an artist's
    /// name matches neither their albums nor their tracks either. `/Items` alone
    /// therefore answered an artist name - which is most of what a filter box
    /// gets typed into it - with nothing at all, for an artist plainly sitting
    /// in the unfiltered list right behind the box.
    pub async fn search(&self, term: &str, limit: u32) -> Result<Vec<Item>> {
        // Both halves are attempted even if one fails, and only a total failure
        // is reported: the artist query is the newer of the two, and a server
        // that dislikes it must not take album and track search down with it.
        let (artists, items) =
            tokio::join!(self.search_artists(term, limit), self.search_items(term, limit));

        let items = match (artists, items) {
            (Ok(artists), Ok(items)) => merge_search(artists, items),
            (Err(err), Ok(items)) => {
                tracing::warn!(error = %err, "artist search failed, showing albums and tracks only");
                merge_search(Vec::new(), items)
            }
            (Ok(artists), Err(err)) => {
                tracing::warn!(error = %err, "album/track search failed, showing artists only");
                merge_search(artists, Vec::new())
            }
            // The `/Items` error, not the artist one: it is the half that has
            // always worked, so its failure is the more informative message.
            (Err(_), Err(err)) => return Err(err),
        };

        let mut items = items;
        items.truncate(limit as usize);
        Ok(items)
    }

    /// Artist half of `search`. `/Artists` is the only endpoint that lists
    /// `MusicArtist` items at all - see `search`.
    async fn search_artists(&self, term: &str, limit: u32) -> Result<Vec<Item>> {
        let resp: ItemsResponse = self
            .get_json(
                "/Artists",
                &[
                    ("userId", self.user_id()?.to_string()),
                    ("searchTerm", term.to_string()),
                    ("limit", limit.to_string()),
                    ("sortBy", "SortName".into()),
                    ("enableTotalRecordCount", "false".into()),
                ],
            )
            .await?;
        Ok(resp.items)
    }

    /// Album and track half of `search`.
    async fn search_items(&self, term: &str, limit: u32) -> Result<Vec<Item>> {
        let resp: ItemsResponse = self
            .get_json(
                "/Items",
                &[
                    ("userId", self.user_id()?.to_string()),
                    ("searchTerm", term.to_string()),
                    ("includeItemTypes", "MusicAlbum,Audio".into()),
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

    /// A server-built queue of tracks similar to `item_id`.
    ///
    /// Nothing here could produce this list: it comes out of Jellyfin's own
    /// similarity scoring over genres, artists and play history. The seed track
    /// is normally the first result, so the queue is played from index 0.
    pub async fn instant_mix(&self, item_id: &str, limit: u32) -> Result<Vec<Item>> {
        let resp: ItemsResponse = self
            .get_json(
                &format!("/Items/{item_id}/InstantMix"),
                &[
                    ("userId", self.user_id()?.to_string()),
                    ("limit", limit.to_string()),
                    ("fields", "Container,ArtistItems".into()),
                    ("enableTotalRecordCount", "false".into()),
                ],
            )
            .await?;
        Ok(resp.items)
    }

    /// Albums newest first, for the Library page's top section.
    ///
    /// Albums rather than tracks: a rip lands as one album's worth of files at
    /// once, so a track-level list of the same thing is one album repeated
    /// twelve times.
    pub async fn recent_albums(&self, limit: u32) -> Result<Vec<Item>> {
        let resp: ItemsResponse = self
            .get_json(
                "/Items",
                &[
                    ("userId", self.user_id()?.to_string()),
                    ("includeItemTypes", "MusicAlbum".into()),
                    ("recursive", "true".into()),
                    ("sortBy", "DateCreated".into()),
                    ("sortOrder", "Descending".into()),
                    ("limit", limit.to_string()),
                    ("enableTotalRecordCount", "false".into()),
                ],
            )
            .await?;
        Ok(resp.items)
    }

    /// Most-played and last-played tracks.
    ///
    /// `filters=IsPlayed` is what keeps these honest on a library larger than
    /// `limit`: without it the tail of the list is filled with tracks that have
    /// never been played at all, since a play count of zero still sorts.
    ///
    /// Both are empty until something has reported a play, which is what
    /// `crate::report` exists for - Jellyfin only knows what it has been told.
    pub async fn most_played_tracks(&self, limit: u32) -> Result<Vec<Item>> {
        self.played_tracks("PlayCount", limit).await
    }

    pub async fn recently_played_tracks(&self, limit: u32) -> Result<Vec<Item>> {
        self.played_tracks("DatePlayed", limit).await
    }

    async fn played_tracks(&self, sort_by: &str, limit: u32) -> Result<Vec<Item>> {
        let resp: ItemsResponse = self
            .get_json(
                "/Items",
                &[
                    ("userId", self.user_id()?.to_string()),
                    ("includeItemTypes", "Audio".into()),
                    ("recursive", "true".into()),
                    ("sortBy", sort_by.to_string()),
                    ("sortOrder", "Descending".into()),
                    ("filters", "IsPlayed".into()),
                    ("limit", limit.to_string()),
                    ("fields", "Container,ArtistItems".into()),
                    ("enableTotalRecordCount", "false".into()),
                ],
            )
            .await?;
        Ok(resp.items)
    }

    /// Tells the server a track has started.
    ///
    /// The session these reports attach to is identified by the token and the
    /// `DeviceId` already in `auth_header`, so there is nothing extra to pass -
    /// and no `PlaySessionId`, which Jellyfin only needs to reconcile its own
    /// transcodes.
    ///
    /// `PlayMethod` is deliberately omitted rather than guessed: `stream_url`
    /// uses the universal endpoint, so whether the server direct-played or
    /// transcoded is its decision and not something this side is told.
    pub async fn report_playback_start(&self, item_id: &str, position: Duration) -> Result<()> {
        self.post_json(
            "/Sessions/Playing",
            json!({
                "ItemId": item_id,
                "PositionTicks": models::ticks(position),
                "IsPaused": false,
                "CanSeek": true,
            }),
        )
        .await
    }

    /// Where playback has reached, and whether it is paused.
    ///
    /// `IsPaused` is the field the dashboard reads; the optional `EventName`
    /// that could also carry "Pause"/"Unpause" is informational, so it is left
    /// out rather than risk sending a value the server's enum does not have.
    pub async fn report_playback_progress(
        &self,
        item_id: &str,
        position: Duration,
        paused: bool,
    ) -> Result<()> {
        self.post_json(
            "/Sessions/Playing/Progress",
            json!({
                "ItemId": item_id,
                "PositionTicks": models::ticks(position),
                "IsPaused": paused,
                "CanSeek": true,
            }),
        )
        .await
    }

    /// Ends playback of a track. This is the report that decides whether
    /// Jellyfin marks the track played and bumps its play count, so the
    /// position it carries has to be the real one.
    pub async fn report_playback_stopped(&self, item_id: &str, position: Duration) -> Result<()> {
        self.post_json(
            "/Sessions/Playing/Stopped",
            json!({
                "ItemId": item_id,
                "PositionTicks": models::ticks(position),
            }),
        )
        .await
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
    /// The token is returned separately so the cache can add it as an auth
    /// header instead of relying on the deprecated api_key query param.
    pub fn stream_url(&self, item_id: &str) -> Result<(String, String)> {
        let creds = self.creds()?;
        tracing::debug!(
            user_id = %creds.user_id,
            token_len = creds.token.len(),
            "generating stream URL"
        );
        let url = format!(
            "{base}/Audio/{item_id}/universal\
             ?userId={user}\
             &deviceId={device}\
             &container={containers}\
             &audioCodec={codec}\
             &transcodingContainer={codec}\
             &transcodingProtocol=http\
             &maxStreamingBitrate={bitrate}\
             &enableRedirection=true",
            base = self.base,
            user = urlencoding::encode(&creds.user_id),
            device = urlencoding::encode(&self.device_id),
            containers = DIRECT_PLAY_CONTAINERS,
            codec = TRANSCODE_CODEC,
            bitrate = TRANSCODE_BITRATE,
        );
        Ok((url, creds.token.clone()))
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
            .with_context(|| format!("GET {url}"))?;

        if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
            tracing::debug!("fetch_bytes 401 on url={}", url);
            return Err(anyhow::Error::new(Unauthorized)
                .context("token rejected by server"));
        }

        let status = resp.status();
        let body = resp.bytes().await.with_context(|| format!("GET {url}"))?;
        if !status.is_success() {
            return Err(anyhow::format_err!("fetch failed with HTTP {}", status));
        }
        Ok(body.to_vec())
    }

    pub fn image_url(&self, item_id: &str, tag: &str, max_height: u32) -> String {
        format!(
            "{}/Items/{item_id}/Images/Primary?tag={}&maxHeight={max_height}",
            self.base,
            urlencoding::encode(tag)
        )
    }
}

/// Orders `search`'s two result sets into one list: artists, then albums, then
/// tracks.
///
/// A typed name is most likely to be the name of the narrowest thing it matches,
/// and `/Items` returns its two types interleaved by the server's own sort, so
/// the split has to be made here rather than relying on the reply's order. Ids
/// are deduplicated because the two queries would overlap on a server that does
/// return artists from `/Items`; first occurrence wins, which keeps this order.
fn merge_search(artists: Vec<Item>, items: Vec<Item>) -> Vec<Item> {
    let (albums, tracks): (Vec<Item>, Vec<Item>) =
        items.into_iter().partition(|item| item.kind() == Kind::Album);

    let mut out = artists;
    out.extend(albums);
    out.extend(tracks);

    let mut seen = HashSet::new();
    out.retain(|item| seen.insert(item.id.clone()));
    out
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
