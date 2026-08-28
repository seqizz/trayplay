use std::collections::HashMap;
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Jellyfin expresses durations in 100-nanosecond ticks.
const TICKS_PER_SECOND: i64 = 10_000_000;

/// A position expressed the way Jellyfin wants it in playback reports.
///
/// The inverse of `Item::duration`, and the reason `TICKS_PER_SECOND` is not
/// private to this file's parsing side any more.
pub fn ticks(position: Duration) -> i64 {
    (position.as_secs_f64() * TICKS_PER_SECOND as f64) as i64
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct AuthResponse {
    pub user: UserDto,
    pub access_token: String,
    /// Read by nothing yet; kept because it names the server a token belongs to,
    /// which is what would distinguish two of them.
    #[allow(dead_code)]
    #[serde(default)]
    pub server_id: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct UserDto {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct ItemsResponse {
    #[serde(default)]
    pub items: Vec<Item>,
    /// Only meaningful when a query asks for it - trayplay's paged queries pass
    /// `enableTotalRecordCount=false` precisely to avoid the server counting.
    #[allow(dead_code)]
    #[serde(default)]
    pub total_record_count: i64,
}

/// What an item actually is. A mixed-type list - which is what the Library
/// search returns - has to branch on this for both the row's subtitle and its
/// activation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Track,
    Album,
    Artist,
    Other,
}

/// Subset of Jellyfin's BaseItemDto that trayplay actually reads.
///
/// Everything past Id/Name is optional on purpose: the same struct is used for
/// audio tracks, albums and artists, and the server omits what does not apply.
///
/// `Serialize` is here for the persisted queue (`player::persist`), which stores
/// whole items rather than ids: re-fetching a hundred tracks one `/Items/{id}`
/// call at a time on every launch would be slow and would fail entirely with the
/// server offline. It round-trips through the same serde attributes, so a saved
/// queue is shaped like the server's own reply.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct Item {
    pub id: String,
    #[serde(default)]
    pub name: String,

    /// Jellyfin's BaseItemKind. Explicitly renamed because PascalCase would
    /// turn the field name into "ItemType".
    #[serde(rename = "Type", default)]
    pub item_type: Option<String>,

    #[serde(default)]
    pub album: Option<String>,
    #[serde(default)]
    pub album_id: Option<String>,
    #[serde(default)]
    pub album_artist: Option<String>,
    #[serde(default)]
    pub artists: Vec<String>,
    #[serde(default)]
    pub artist_items: Vec<NameId>,

    #[serde(default)]
    pub run_time_ticks: Option<i64>,
    /// Track number within a disc.
    #[serde(default)]
    pub index_number: Option<i32>,
    /// Disc number.
    #[serde(default)]
    pub parent_index_number: Option<i32>,
    #[serde(default)]
    pub container: Option<String>,

    /// Image tag per type ("Primary", "Backdrop", ...); needed to build image URLs.
    #[serde(default)]
    pub image_tags: HashMap<String, String>,
    /// Set on tracks whose own album carries the cover art.
    #[serde(default)]
    pub album_primary_image_tag: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct NameId {
    pub id: String,
    pub name: String,
}

impl Item {
    /// See `Kind`.
    pub fn kind(&self) -> Kind {
        match self.item_type.as_deref() {
            Some("Audio") => Kind::Track,
            Some("MusicAlbum") => Kind::Album,
            Some("MusicArtist") => Kind::Artist,
            _ => Kind::Other,
        }
    }

    pub fn duration(&self) -> Option<Duration> {
        let ticks = self.run_time_ticks?;
        if ticks <= 0 {
            return None;
        }
        Some(Duration::from_secs_f64(ticks as f64 / TICKS_PER_SECOND as f64))
    }

    /// Display artist, preferring the album artist so compilations stay coherent.
    pub fn display_artist(&self) -> &str {
        self.album_artist
            .as_deref()
            .or_else(|| self.artists.first().map(String::as_str))
            .or_else(|| self.artist_items.first().map(|a| a.name.as_str()))
            .unwrap_or("Unknown Artist")
    }

    /// Every credited artist, joined for display.
    ///
    /// Deliberately does *not* prefer the album artist the way `display_artist`
    /// does: a queue row is about one track, so an album artist of "Various
    /// Artists" would hide whoever is actually credited on it. Falls back
    /// through `Artists` (names only, no ids) to `display_artist`.
    pub fn display_artists(&self) -> String {
        if !self.artist_items.is_empty() {
            return self
                .artist_items
                .iter()
                .map(|a| a.name.as_str())
                .collect::<Vec<_>>()
                .join(", ");
        }
        if !self.artists.is_empty() {
            return self.artists.join(", ");
        }
        self.display_artist().to_string()
    }

    /// Item whose Primary image should be used as cover art: the album if the
    /// track itself has no artwork of its own.
    pub fn cover_source(&self) -> Option<(&str, &str)> {
        if let Some(tag) = self.image_tags.get("Primary") {
            return Some((self.id.as_str(), tag.as_str()));
        }
        match (&self.album_id, &self.album_primary_image_tag) {
            (Some(id), Some(tag)) => Some((id.as_str(), tag.as_str())),
            _ => None,
        }
    }
}
