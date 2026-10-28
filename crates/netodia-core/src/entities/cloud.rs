//! `NetEase` identities and display data. Temporary CDN URLs never belong in persisted queues.

use serde::{Deserialize, Serialize};

/// Library categories share one playlist grid and detail surface.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MusicLibraryCategory {
    #[default]
    Collected,
    Created,
    Recommendations,
    Roaming,
}

impl MusicLibraryCategory {
    pub const fn from_index(index: i32) -> Self {
        match index {
            1 => Self::Created,
            2 => Self::Recommendations,
            3 => Self::Roaming,
            _ => Self::Collected,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RoamingMode {
    Personal,
    Heart,
    Similar,
}

/// Synthetic identity outside `NetEase`'s entity range, preserving the shared detail's ID contract.
pub const DAILY_PLAYLIST_ID: i64 = 9_000_000_000_000_001;
pub const PERSONAL_RADAR_ID: i64 = 3_136_952_023;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Artist {
    pub id: i64,
    #[serde(deserialize_with = "deserialize_nullable_name")]
    pub name: String,
    #[serde(default, rename = "picUrl")]
    pub cover_url: Option<String>,
    #[serde(default, rename = "img1v1Url")]
    pub alternate_cover_url: Option<String>,
}

impl Artist {
    pub fn image_url(&self) -> Option<&str> {
        self.cover_url.as_deref().or(self.alternate_cover_url.as_deref())
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Album {
    pub id: i64,
    #[serde(deserialize_with = "deserialize_nullable_name")]
    pub name: String,
    #[serde(default, rename = "picUrl")]
    pub cover_url: Option<String>,
    #[serde(default)]
    pub artists: Vec<Artist>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Song {
    pub id: i64,
    pub name: String,
    #[serde(default, rename = "ar", alias = "artists")]
    pub artists: Vec<Artist>,
    #[serde(default, rename = "al", alias = "album")]
    pub album: Album,
    #[serde(default, rename = "dt", alias = "duration")]
    pub duration_ms: u64,
    #[serde(default)]
    pub fee: i32,
    #[serde(default)]
    pub privilege: Option<Privilege>,
}

/// A song entry from the account's special "liked songs" playlist.
///
/// `favorite_order` is the one-based position in the `trackIds` array returned by `NetEase` for
/// that playlist. `added_at_ms` keeps the server's favorite timestamp when the endpoint supplies
/// it; the client must not infer favorite order from a later song-detail response.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LikedSong {
    pub song: Song,
    pub favorite_order: i32,
    pub added_at_ms: Option<i64>,
}

/// A song in the account's `NetEase` recent-play history. `played_at_ms` and the server's list
/// position are authoritative; views must preserve the returned order instead of re-sorting it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RecentSong {
    pub song: Song,
    pub played_at_ms: i64,
}

/// An entry in the account's all-time listening ranking, with the server's play count.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlayedSong {
    pub song: Song,
    #[serde(rename = "playCount")]
    pub play_count: i32,
}

impl Song {
    pub fn artist_names(&self) -> String {
        self.artists.iter().map(|a| a.name.as_str()).collect::<Vec<_>>().join(" / ")
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Privilege {
    pub id: i64,
    #[serde(default)]
    pub st: i32,
    #[serde(default)]
    pub pl: i64,
    #[serde(default)]
    pub fee: i32,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Playlist {
    pub id: i64,
    pub name: String,
    #[serde(default, rename = "coverImgUrl", alias = "picUrl")]
    pub cover_url: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default, rename = "trackCount")]
    pub track_count: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Account {
    #[serde(rename = "userId")]
    pub id: i64,
    pub nickname: String,
    #[serde(default, rename = "avatarUrl")]
    pub avatar_url: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Quality {
    Standard,
    Higher,
    #[default]
    Exhigh,
    Lossless,
    Hires,
}

impl Quality {
    /// Bitrate used by the song URL endpoint. `YesPlayMusic` requests 350000 for
    /// lossless and 999000 for Hi-Res instead of sending a format name.
    pub const fn bitrate(self) -> u32 {
        match self {
            Self::Standard => 128_000,
            Self::Higher => 192_000,
            Self::Exhigh => 320_000,
            Self::Lossless => 350_000,
            Self::Hires => 999_000,
        }
    }

    pub const fn level(self) -> &'static str {
        match self {
            Self::Standard => "standard",
            Self::Higher => "higher",
            Self::Exhigh => "exhigh",
            Self::Lossless => "lossless",
            Self::Hires => "hires",
        }
    }

    pub fn from_level(level: &str) -> Self {
        match level {
            "standard" => Self::Standard,
            "higher" => Self::Higher,
            "lossless" => Self::Lossless,
            "hires" => Self::Hires,
            _ => Self::Exhigh,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SearchKind {
    #[default]
    Songs,
    Albums,
    Artists,
    Playlists,
}

impl SearchKind {
    pub const fn code(self) -> u32 {
        match self {
            Self::Songs => 1,
            Self::Albums => 10,
            Self::Artists => 100,
            Self::Playlists => 1000,
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SearchResult {
    #[serde(default)]
    pub songs: Vec<Song>,
    #[serde(default)]
    pub albums: Vec<Album>,
    #[serde(default)]
    pub artists: Vec<Artist>,
    #[serde(default)]
    pub playlists: Vec<Playlist>,
    #[serde(default, rename = "songCount")]
    pub song_count: u64,
    #[serde(default, rename = "albumCount")]
    pub album_count: u64,
    #[serde(default, rename = "artistCount")]
    pub artist_count: u64,
    #[serde(default, rename = "playlistCount")]
    pub playlist_count: u64,
}

impl SearchResult {
    pub const fn total(&self, kind: SearchKind) -> u64 {
        match kind {
            SearchKind::Songs => self.song_count,
            SearchKind::Albums => self.album_count,
            SearchKind::Artists => self.artist_count,
            SearchKind::Playlists => self.playlist_count,
        }
    }
}

/// Transient media URL plus the server's permission verdict. Never serialized.
#[derive(Clone)]
pub struct StreamTicket {
    pub song_id: i64,
    pub url: String,
    pub format: String,
    pub expires_after_seconds: u64,
    pub trial: bool,
    pub size: Option<u64>,
}

#[derive(Clone, Debug, Default)]
pub struct Lyrics {
    pub original: String,
    pub translation: String,
    pub romanization: String,
}

// Tracks can retain their IDs while NetEase returns null artist/album names. Missing display
// metadata must not reject the whole song batch; other malformed field types still fail.
fn deserialize_nullable_name<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<String, D::Error> {
    Option::<String>::deserialize(deserializer).map(Option::unwrap_or_default)
}

#[cfg(test)]
#[path = "tests/cloud_tests.rs"]
mod tests;
