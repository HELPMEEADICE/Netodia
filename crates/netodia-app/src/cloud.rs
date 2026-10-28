//! Account sessions for the direct `NetEase` client.
//!
//! This module is the application boundary for login and session lifetime. Views receive typed
//! account state and never see cookies, device credentials, or raw HTTP responses.

mod favorites_cache;

use std::collections::HashSet;
use std::future::Future;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use lru::LruCache;
use netodia_core::entities::cloud::{
    Account, Album, Artist, DAILY_PLAYLIST_ID, LikedSong, Lyrics, MusicLibraryCategory, PlayedSong,
    Playlist, Quality, RecentSong, RoamingMode, SearchKind, SearchResult, Song, StreamTicket,
};
use netodia_core::error::{AppError, AppResult};
use netodia_net::netease::{Client, QrStatus as NetEaseQrStatus};
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, watch};
use tokio_util::sync::CancellationToken;

use favorites_cache::FavoritesCache;

const KEYRING_SERVICE: &str = "Netodia";
const KEYRING_USER: &str = "netease-session";
const SESSION_VERSION: u8 = 1;
const COVER_SIZE: &str = "512y512";
/// How long a fetched playlist may be reopened without another round trip. Long enough to make
/// flipping between two playlists free, short enough that a list edited on another device shows up
/// on its own.
const PLAYLIST_CACHE_TTL: Duration = Duration::from_mins(5);
/// An entry is a whole track list — roughly a megabyte for a thousand songs — so a few, not a stack.
const PLAYLIST_CACHE_CAP: NonZeroUsize = match NonZeroUsize::new(4) {
    Some(cap) => cap,
    None => panic!("PLAYLIST_CACHE_CAP > 0"),
};
// Entry count alone cannot bound a cache whose playlists can contain tens of thousands of songs.
const PLAYLIST_CACHE_BYTES: usize = 8 * 1024 * 1024;
type LikedIdsCache = Arc<parking_lot::RwLock<Option<(i64, u64, HashSet<i64>)>>>;
type RecentSongsCache = Arc<parking_lot::RwLock<Option<(i64, u64, Vec<RecentSong>)>>>;
/// `(account, generation, playlist id)` → when it was fetched, and the response itself.
type PlaylistCache = Arc<parking_lot::Mutex<LruCache<(i64, u64, i64), CachedPlaylist>>>;

struct CachedPlaylist {
    fetched_at: Instant,
    playlist: Playlist,
    songs: Arc<[Song]>,
    bytes: usize,
}

fn playlist_bytes(playlist: &Playlist, songs: &[Song]) -> usize {
    let optional = |value: &Option<String>| value.as_ref().map_or(0, String::capacity);
    let artists = |artists: &Vec<Artist>| {
        artists.capacity() * std::mem::size_of::<Artist>()
            + artists
                .iter()
                .map(|artist| {
                    artist.name.capacity()
                        + optional(&artist.cover_url)
                        + optional(&artist.alternate_cover_url)
                })
                .sum::<usize>()
    };
    std::mem::size_of_val(songs)
        + std::mem::size_of::<CachedPlaylist>()
        + playlist.name.capacity()
        + optional(&playlist.cover_url)
        + optional(&playlist.description)
        + songs
            .iter()
            .map(|song| {
                song.name.capacity()
                    + song.album.name.capacity()
                    + optional(&song.album.cover_url)
                    + artists(&song.artists)
                    + artists(&song.album.artists)
            })
            .sum::<usize>()
}

/// Whether a cached playlist is still worth serving. Pure, so both sides of the TTL boundary are
/// table-testable without a clock to steer.
fn playlist_cache_is_fresh(fetched_at: Instant, now: Instant) -> bool {
    now.saturating_duration_since(fetched_at) < PLAYLIST_CACHE_TTL
}

/// QR sign-in progress exposed to the application and view layers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QrStatus {
    Expired,
    Waiting,
    Confirming,
    Authorized,
}

/// Returns a bounded `NetEase` image URL while leaving other HTTP artwork sources intact.
pub fn cover_request_url(source: &str) -> Option<reqwest::Url> {
    let mut url = netodia_net::services::net::http_url(source)?;
    let host = url.host_str()?;
    if host == "music.126.net" || host.ends_with(".music.126.net") {
        url.set_scheme("https").ok()?;
        if !url.query_pairs().any(|(key, _)| key == "param") {
            url.query_pairs_mut().append_pair("param", COVER_SIZE);
        }
    }
    Some(url)
}

/// Fetch a bounded cover body for the view layer without exposing its network response.
pub async fn fetch_cover_bytes(
    client: &reqwest::Client,
    source: &str,
    max_bytes: usize,
) -> Option<Vec<u8>> {
    let url = cover_request_url(source)?;
    let response = client.get(url).send().await.ok()?.error_for_status().ok()?;
    let max_bytes = u64::try_from(max_bytes).ok()?;
    let bytes =
        netodia_net::services::net::read_capped(response, "Cover image", max_bytes).await.ok()?;
    (!bytes.is_empty()).then_some(bytes)
}

/// UI-safe account state. The credential and the session cookie intentionally have no public
/// representation in this type.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SessionStatus {
    pub account: Option<Account>,
    pub remember_available: bool,
    pub notice: Option<String>,
    /// Changes on account transitions so views can discard private cached models.
    pub generation: u64,
}

/// Owns one `NetEase` client and coordinates login, restoration, logout, and account-scoped work.
#[derive(Clone)]
pub struct CloudService {
    client: Client,
    http: reqwest::Client,
    pending_restore: Arc<Mutex<Option<StoredSession>>>,
    status: watch::Sender<SessionStatus>,
    transition: Arc<Mutex<()>>,
    account_tasks: Arc<parking_lot::RwLock<CancellationToken>>,
    guest_initialized: Arc<AtomicBool>,
    liked_ids_cache: LikedIdsCache,
    liked_ids_revision: Arc<AtomicU64>,
    recent_songs_cache: RecentSongsCache,
    playlist_cache: PlaylistCache,
    // Serializes disk reads, atomic writes and invalidation, including logout racing a fetch.
    favorites_cache: Arc<Mutex<FavoritesCache>>,
}

#[derive(Serialize, Deserialize)]
struct StoredSession {
    version: u8,
    device_id: String,
    cookie: String,
}

impl CloudService {
    /// Reads only the OS credential store. If it is unavailable, the service remains usable and
    /// reports that this process cannot remember a login.
    pub async fn new(http: reqwest::Client, favorites_cache_path: std::path::PathBuf) -> Self {
        let read = tokio::task::spawn_blocking(read_credential).await;
        let (stored, remember_available, notice) = match read {
            Ok(Ok(Some(secret))) => {
                if let Some(stored) = decode_session(&secret) {
                    (Some(stored), true, None)
                } else {
                    let removed = tokio::task::spawn_blocking(delete_credential)
                        .await
                        .is_ok_and(|result| result.is_ok());
                    (None, removed, Some("已清除无法读取的登录信息。".into()))
                }
            }
            Ok(Ok(None)) => (None, true, None),
            _ => (None, false, Some("系统凭据存储不可用，登录信息只会保留在本次运行中。".into())),
        };

        let device_id = stored
            .as_ref()
            .map(|session| session.device_id.clone())
            .filter(|id| valid_device_id(id));
        let client = device_id
            .and_then(|id| Client::with_device_id(http.clone(), id).ok())
            .unwrap_or_else(|| Client::new(http.clone()));
        let initial = SessionStatus { account: None, remember_available, notice, generation: 0 };
        let (status, _) = watch::channel(initial);

        Self {
            client,
            http,
            pending_restore: Arc::new(Mutex::new(stored)),
            status,
            transition: Arc::new(Mutex::new(())),
            account_tasks: Arc::new(parking_lot::RwLock::new(CancellationToken::new())),
            guest_initialized: Arc::new(AtomicBool::new(false)),
            liked_ids_cache: Arc::new(parking_lot::RwLock::new(None)),
            liked_ids_revision: Arc::new(AtomicU64::new(0)),
            recent_songs_cache: Arc::new(parking_lot::RwLock::new(None)),
            playlist_cache: Arc::new(parking_lot::Mutex::new(LruCache::new(PLAYLIST_CACHE_CAP))),
            favorites_cache: Arc::new(Mutex::new(FavoritesCache::new(favorites_cache_path))),
        }
    }

    pub fn subscribe(&self) -> watch::Receiver<SessionStatus> {
        self.status.subscribe()
    }

    pub fn status(&self) -> SessionStatus {
        self.status.borrow().clone()
    }

    /// Tasks that hold account-private data should select on this token. It is cancelled at the
    /// beginning of every account transition, before a new session becomes visible.
    pub fn account_tasks_token(&self) -> CancellationToken {
        self.account_tasks.read().clone()
    }

    /// Resolve a short-lived CDN ticket under the current account task generation. Session changes
    /// cancel the HTTP future before its result can reach a player.
    pub async fn stream_ticket(&self, song_id: i64, quality: Quality) -> AppResult<StreamTicket> {
        validate_id("歌曲", song_id)?;
        self.public_request(self.client.stream(song_id, quality)).await
    }

    pub async fn discover(&self) -> AppResult<Vec<Playlist>> {
        self.public_request(self.client.discover()).await
    }

    pub async fn charts(&self) -> AppResult<Vec<Playlist>> {
        self.public_request(self.client.charts()).await
    }

    pub async fn search(
        &self,
        keywords: &str,
        kind: SearchKind,
        offset: u64,
    ) -> AppResult<SearchResult> {
        if keywords.trim().is_empty() {
            return Err(AppError::Validation("搜索内容不能为空".to_owned()));
        }
        if keywords.chars().count() > 200 {
            return Err(AppError::Validation("搜索内容不能超过 200 个字符".to_owned()));
        }
        self.public_request(self.client.search(keywords.trim(), kind, offset)).await
    }

    pub async fn songs(&self, ids: &[i64]) -> AppResult<Vec<Song>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        validate_ids("歌曲", ids)?;
        self.public_request(self.client.songs(ids)).await
    }

    pub async fn playlist(&self, id: i64) -> AppResult<(Playlist, Vec<Song>)> {
        validate_id("歌单", id)?;
        self.public_request(self.client.playlist(id)).await
    }

    /// Load a signed-in user's saved playlist and its tracks under one account generation.
    ///
    /// A playlist is the one cloud response big enough that a re-open is worth avoiding: its
    /// track list costs a round trip plus one per 300 songs, so flipping back to a list you just
    /// looked at pays that again. The cache is keyed by account *and* generation, and cleared on
    /// every transition, so a stale entry can't outlive the session that fetched it.
    pub async fn saved_playlist(&self, id: i64) -> AppResult<(Playlist, Arc<[Song]>)> {
        validate_id("歌单", id)?;
        let (account, cancellation) = self.account_snapshot()?;
        let generation = self.status().generation;
        if let Some(cached) = self.cached_playlist(account.id, generation, id) {
            return Ok(cached);
        }
        let (playlist, songs) =
            self.account_request_in_generation(self.client.playlist(id), cancellation).await?;
        let loaded = (playlist, songs.into());
        self.store_playlist(account.id, generation, id, &loaded);
        Ok(loaded)
    }

    /// Total duration from an already loaded playlist in the current account session.
    /// List cards can reuse this metadata without fetching every playlist's song list.
    pub fn cached_playlist_duration(&self, id: i64) -> Option<u64> {
        let status = self.status();
        let account = status.account?;
        let (_, songs) = self.cached_playlist(account.id, status.generation, id)?;
        Some(songs.iter().fold(0_u64, |sum, song| sum.saturating_add(song.duration_ms)))
    }

    fn cached_playlist(
        &self,
        account_id: i64,
        generation: u64,
        playlist_id: i64,
    ) -> Option<(Playlist, Arc<[Song]>)> {
        let key = (account_id, generation, playlist_id);
        let mut cache = self.playlist_cache.lock();
        match cache.peek(&key).map(|entry| entry.fetched_at) {
            Some(at) if playlist_cache_is_fresh(at, Instant::now()) => {
                cache.get(&key).map(|entry| (entry.playlist.clone(), Arc::clone(&entry.songs)))
            }
            Some(_) => {
                cache.pop(&key);
                None
            }
            None => None,
        }
    }

    /// Stored under the generation the *fetch* ran in, never the one current at this instant: a
    /// sign-in that lands between the two would otherwise file an old session's track list under
    /// the new one, and the next read would serve it.
    fn store_playlist(
        &self,
        account_id: i64,
        generation: u64,
        playlist_id: i64,
        loaded: &(Playlist, Arc<[Song]>),
    ) {
        let key = (account_id, generation, playlist_id);
        let bytes = playlist_bytes(&loaded.0, &loaded.1);
        let mut cache = self.playlist_cache.lock();
        cache.pop(&key);
        if bytes > PLAYLIST_CACHE_BYTES {
            return;
        }
        let mut retained: usize = cache.iter().map(|(_, entry)| entry.bytes).sum();
        while retained + bytes > PLAYLIST_CACHE_BYTES {
            let Some((_, evicted)) = cache.pop_lru() else { break };
            retained -= evicted.bytes;
        }
        cache.put(
            key,
            CachedPlaylist {
                fetched_at: Instant::now(),
                playlist: loaded.0.clone(),
                songs: Arc::clone(&loaded.1),
                bytes,
            },
        );
    }

    pub async fn album(&self, id: i64) -> AppResult<(Album, Vec<Song>)> {
        validate_id("专辑", id)?;
        self.public_request(self.client.album(id)).await
    }

    pub async fn artist(&self, id: i64) -> AppResult<(Artist, Vec<Song>)> {
        validate_id("歌手", id)?;
        self.public_request(self.client.artist(id)).await
    }

    pub async fn lyrics(&self, song_id: i64) -> AppResult<Lyrics> {
        validate_id("歌曲", song_id)?;
        self.public_request(self.client.lyrics(song_id)).await
    }

    pub async fn daily(&self) -> AppResult<Vec<Song>> {
        self.private_request(self.client.daily()).await
    }

    /// Daily recommendations use the same detail surface as ordinary remote playlists.
    pub async fn library_playlist(&self, id: i64) -> AppResult<(Playlist, Arc<[Song]>)> {
        if id != DAILY_PLAYLIST_ID {
            return self.saved_playlist(id).await;
        }
        let songs = self.daily().await?;
        let playlist = Playlist {
            id,
            name: "每日推荐".to_owned(),
            track_count: u64::try_from(songs.len()).unwrap_or(u64::MAX),
            cover_url: songs.first().and_then(|song| song.album.cover_url.clone()),
            ..Playlist::default()
        };
        Ok((playlist, songs.into()))
    }

    pub async fn recommendations(&self) -> AppResult<Vec<Playlist>> {
        self.private_request(self.client.recommendations()).await
    }

    /// Fetch a roaming batch using an actual `NetEase` song ID as its seed.
    pub async fn roaming_songs(
        &self,
        mode: RoamingMode,
        seed: Option<i64>,
    ) -> AppResult<Vec<Song>> {
        match mode {
            RoamingMode::Personal => self.fm().await,
            RoamingMode::Similar => {
                let id = seed.ok_or_else(|| {
                    AppError::Validation("请先播放一首歌曲，再开启相似歌曲".to_owned())
                })?;
                validate_id("歌曲", id)?;
                self.private_request(self.client.similar_songs(id)).await
            }
            RoamingMode::Heart => {
                let (account, cancellation) = self.account_snapshot()?;
                self.account_request_in_generation(
                    async {
                        let playlist_id = self.client.liked_playlist_id(account.id).await?;
                        let liked = self.client.liked_ids(account.id).await?;
                        let liked_seed =
                            seed.filter(|id| liked.contains(id)).or_else(|| liked.first().copied());
                        let Some(liked_seed) = liked_seed else {
                            return Ok(Vec::new());
                        };
                        self.client
                            .intelligence(liked_seed, playlist_id, seed.unwrap_or(liked_seed))
                            .await
                    },
                    cancellation,
                )
                .await
            }
        }
    }

    pub async fn fm(&self) -> AppResult<Vec<Song>> {
        self.private_request(self.client.fm()).await
    }

    pub async fn fm_trash(&self, song_id: i64) -> AppResult<()> {
        validate_id("歌曲", song_id)?;
        self.private_request(self.client.fm_trash(song_id)).await
    }

    pub async fn user_playlists(&self, offset: u64) -> AppResult<Vec<Playlist>> {
        let (account, cancellation) = self.account_snapshot()?;
        self.account_request_in_generation(
            self.client.user_playlists(account.id, offset),
            cancellation,
        )
        .await
    }

    /// Playlists the signed-in user has collected, excluding playlists they created.
    pub async fn saved_playlists(&self, offset: u64) -> AppResult<Vec<Playlist>> {
        self.user_playlists(offset).await
    }

    pub async fn saved_playlists_page(&self, offset: u64) -> AppResult<(Vec<Playlist>, bool)> {
        let (account, cancellation) = self.account_snapshot()?;
        self.account_request_in_generation(
            self.client.user_playlists_page(account.id, offset),
            cancellation,
        )
        .await
    }

    pub async fn library_playlists_page(
        &self,
        category: MusicLibraryCategory,
        offset: u64,
    ) -> AppResult<(Vec<Playlist>, bool)> {
        match category {
            MusicLibraryCategory::Collected => self.saved_playlists_page(offset).await,
            MusicLibraryCategory::Created => {
                let (account, cancellation) = self.account_snapshot()?;
                self.account_request_in_generation(
                    self.client.created_playlists_page(account.id, offset),
                    cancellation,
                )
                .await
            }
            MusicLibraryCategory::Recommendations => {
                self.recommendations().await.map(|items| (items, false))
            }
            MusicLibraryCategory::Roaming => Ok((Vec::new(), false)),
        }
    }

    pub async fn liked_ids(&self) -> AppResult<Vec<i64>> {
        let (account, cancellation) = self.account_snapshot()?;
        let generation = self.status().generation;
        let revision = self.liked_ids_revision.load(Ordering::Acquire);
        let ids = self
            .account_request_in_generation(self.client.liked_ids(account.id), cancellation)
            .await?;
        if self.status().generation == generation
            && self.liked_ids_revision.load(Ordering::Acquire) == revision
        {
            *self.liked_ids_cache.write() =
                Some((account.id, generation, ids.iter().copied().collect()));
        }
        Ok(ids)
    }

    /// Load the liked playlist's server order without allowing an account transition during fetch.
    pub async fn liked_songs(&self) -> AppResult<Vec<LikedSong>> {
        let (account, cancellation) = self.account_snapshot()?;
        let generation = self.status().generation;
        let revision = self.liked_ids_revision.load(Ordering::Acquire);
        let songs = self
            .account_request_in_generation(self.client.liked_songs(account.id), cancellation)
            .await?;
        if self.status().generation == generation
            && self.liked_ids_revision.load(Ordering::Acquire) == revision
        {
            *self.liked_ids_cache.write() =
                Some((account.id, generation, songs.iter().map(|entry| entry.song.id).collect()));
            self.persist_liked_songs(account.id, generation, revision, &songs).await;
        }
        Ok(songs)
    }

    /// Reads the last successful list for this account without treating it as a fresh response.
    /// Views paint it first and still refresh from `NetEase` on entry. Session validation remains
    /// the credential store's responsibility; this file contains display metadata only.
    pub async fn cached_liked_songs(&self) -> Option<Vec<LikedSong>> {
        let status = self.status();
        let account = status.account?;
        let generation = status.generation;
        let revision = self.liked_ids_revision();
        let cache = self.favorites_cache.clone().lock_owned().await;
        let result = tokio::task::spawn_blocking(move || cache.load(account.id)).await;
        if !self.liked_snapshot_current(generation, revision) {
            return None;
        }
        match result {
            Ok(Ok(songs)) => songs,
            Ok(Err(error)) => {
                log::warn!("cloud::cached_liked_songs: {}", netodia_core::error::describe(&error));
                None
            }
            Err(error) => {
                log::warn!("cloud::cached_liked_songs: {}", netodia_core::error::describe(&error));
                None
            }
        }
    }

    fn liked_snapshot_current(&self, generation: u64, revision: u64) -> bool {
        self.status().generation == generation && self.liked_ids_revision() == revision
    }

    async fn persist_liked_songs(
        &self,
        account_id: i64,
        generation: u64,
        revision: u64,
        songs: &[LikedSong],
    ) {
        let cache = self.favorites_cache.clone().lock_owned().await;
        let cloud = self.clone();
        let songs = songs.to_vec();
        let result = tokio::task::spawn_blocking(move || {
            if cloud.liked_snapshot_current(generation, revision) {
                cache.store(account_id, &songs)
            } else {
                Ok(())
            }
        })
        .await;
        match result {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                log::warn!("cloud::persist_liked_songs: {}", netodia_core::error::describe(&error));
            }
            Err(error) => {
                log::warn!("cloud::persist_liked_songs: {}", netodia_core::error::describe(&error));
            }
        }
    }

    async fn clear_liked_songs_cache(&self, generation: u64) {
        let cache = self.favorites_cache.clone().lock_owned().await;
        let cloud = self.clone();
        match tokio::task::spawn_blocking(move || {
            if cloud.status().generation == generation { cache.clear() } else { Ok(()) }
        })
        .await
        {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                log::warn!(
                    "cloud::clear_liked_songs_cache: {}",
                    netodia_core::error::describe(&error)
                );
            }
            Err(error) => {
                log::warn!(
                    "cloud::clear_liked_songs_cache: {}",
                    netodia_core::error::describe(&error)
                );
            }
        }
    }

    /// Fetch the signed-in account's `NetEase` play history, preserving the order returned by the
    /// server. The snapshot is retained per account so the page can paint it immediately while a
    /// fresh request runs each time the user opens the history view.
    pub async fn recent_songs(&self) -> AppResult<Vec<RecentSong>> {
        let (account, cancellation) = self.account_snapshot()?;
        let generation = self.status().generation;
        let songs =
            self.account_request_in_generation(self.client.recent_songs(), cancellation).await?;
        if self.status().generation == generation {
            *self.recent_songs_cache.write() = Some((account.id, generation, songs.clone()));
        }
        Ok(songs)
    }

    /// The signed-in account's all-time listening ranking, independent of the local library.
    pub async fn most_played_songs(&self) -> AppResult<Vec<PlayedSong>> {
        let (account, cancellation) = self.account_snapshot()?;
        self.account_request_in_generation(self.client.most_played_songs(account.id), cancellation)
            .await
    }

    /// A recent-history snapshot for the visible account. It is only an in-process presentation
    /// cache; opening the page still refreshes it from `NetEase`.
    pub fn cached_recent_songs(&self) -> Option<Vec<RecentSong>> {
        let status = self.status();
        let account_id = status.account?.id;
        let cache = self.recent_songs_cache.read();
        let (cached_account, generation, songs) = cache.as_ref()?;
        (*cached_account == account_id && *generation == status.generation).then(|| songs.clone())
    }

    /// Report a completed listening session only to the account that actually heard it.
    /// Reporting failures stay in the background and never change the login or player state.
    pub async fn report_listen(
        &self,
        song_id: i64,
        seconds: u64,
        generation: u64,
    ) -> AppResult<()> {
        let (_account, cancellation) = self.account_snapshot()?;
        if self.status().generation != generation {
            return Ok(());
        }
        validate_id("歌曲", song_id)?;
        self.account_request_in_generation(
            self.client.report_listen(song_id, seconds),
            cancellation,
        )
        .await?;
        if self.status().generation == generation {
            *self.recent_songs_cache.write() = None;
        }
        Ok(())
    }

    /// Cached membership for player controls. `None` means this account's liked set has not yet
    /// been fetched in this process; callers can still write the toggle and refresh it afterward.
    pub fn cached_is_liked(&self, song_id: i64) -> Option<bool> {
        let status = self.status();
        let account_id = status.account?.id;
        let cache = self.liked_ids_cache.read();
        let (cached_account, generation, ids) = cache.as_ref()?;
        (*cached_account == account_id && *generation == status.generation)
            .then(|| ids.contains(&song_id))
    }

    /// The whole liked set this process already holds for the visible account, or `None` when it
    /// was never fetched (or belongs to a previous account or generation). A view flagging every
    /// row of a list is the caller that needs the set rather than one membership answer, and a
    /// list open is not worth a second whole-list request when the snapshot is already there.
    pub fn cached_liked_ids(&self) -> Option<HashSet<i64>> {
        let status = self.status();
        let account_id = status.account?.id;
        let cache = self.liked_ids_cache.read();
        let (cached_account, generation, ids) = cache.as_ref()?;
        (*cached_account == account_id && *generation == status.generation).then(|| ids.clone())
    }

    /// Version of the liked-song snapshot. Views use it to discard a read that raced a successful
    /// like/unlike write, so an older response cannot paint stale membership after the button.
    pub fn liked_ids_revision(&self) -> u64 {
        self.liked_ids_revision.load(Ordering::Acquire)
    }

    pub async fn like(&self, song_id: i64, liked: bool) -> AppResult<()> {
        validate_id("歌曲", song_id)?;
        let (account, cancellation) = self.account_snapshot()?;
        let generation = self.status().generation;
        self.account_request_in_generation(self.client.like(song_id, liked), cancellation).await?;
        if self.status().generation == generation {
            self.liked_ids_revision.fetch_add(1, Ordering::AcqRel);
            if let Some((cached_account, cached_generation, ids)) =
                self.liked_ids_cache.write().as_mut()
                && *cached_account == account.id
                && *cached_generation == generation
            {
                if liked {
                    ids.insert(song_id);
                } else {
                    ids.remove(&song_id);
                }
            }
        }
        // A successful mutation invalidates the disk snapshot even when Favorites is hidden.
        // The next successful list refresh writes the new server order.
        self.clear_liked_songs_cache(generation).await;
        Ok(())
    }

    pub async fn saved_albums(&self, offset: u64) -> AppResult<Vec<Album>> {
        self.private_request(self.client.saved_albums(offset)).await
    }

    /// Read the entire collection in server order under one account cancellation scope.
    pub async fn all_saved_albums(&self) -> AppResult<Vec<Album>> {
        self.private_request(async {
            let mut albums = Vec::new();
            let mut seen = HashSet::new();
            let mut offset = 0;
            loop {
                let (page, more) = self.client.saved_albums_page(offset).await?;
                let page_len = page.len();
                let before = albums.len();
                albums.extend(page.into_iter().filter(|album| seen.insert(album.id)));
                // A repeated page must not spin forever if the server ignores its offset.
                if !more || page_len == 0 || albums.len() == before {
                    break;
                }
                offset += u64::try_from(page_len).unwrap_or(u64::MAX);
            }
            Ok(albums)
        })
        .await
    }

    pub async fn saved_artists(&self, offset: u64) -> AppResult<Vec<Artist>> {
        self.private_request(self.client.saved_artists(offset)).await
    }

    pub async fn subscribe_playlist(&self, id: i64, subscribe: bool) -> AppResult<()> {
        validate_id("歌单", id)?;
        self.private_request(self.client.subscribe_playlist(id, subscribe)).await
    }

    pub async fn subscribe_album(&self, id: i64, subscribe: bool) -> AppResult<()> {
        validate_id("专辑", id)?;
        self.private_request(self.client.subscribe_album(id, subscribe)).await
    }

    pub async fn subscribe_artist(&self, id: i64, subscribe: bool) -> AppResult<()> {
        validate_id("歌手", id)?;
        self.private_request(self.client.subscribe_artist(id, subscribe)).await
    }

    fn require_account(&self) -> AppResult<Account> {
        self.status().account.ok_or_else(|| AppError::network_msg("请先登录网易云账号"))
    }

    fn account_snapshot(&self) -> AppResult<(Account, CancellationToken)> {
        // Capture the account's cancellation generation before reading its identity. A transition
        // cancels that generation before publishing signed-out state, so a request carrying the
        // previous uid can never slip through under a newly installed account.
        let cancellation = self.account_tasks_token();
        let account = self.require_account()?;
        if cancellation.is_cancelled() {
            return Err(AppError::network_msg("网易云请求已取消"));
        }
        Ok((account, cancellation))
    }

    async fn public_request<T>(
        &self,
        request: impl Future<Output = netodia_net::netease::Result<T>>,
    ) -> AppResult<T> {
        self.ensure_guest().await?;
        self.account_request(request).await
    }

    async fn private_request<T>(
        &self,
        request: impl Future<Output = netodia_net::netease::Result<T>>,
    ) -> AppResult<T> {
        let cancellation = self.account_tasks_token();
        self.require_account()?;
        self.account_request_in_generation(request, cancellation).await
    }

    async fn account_request<T>(
        &self,
        request: impl Future<Output = netodia_net::netease::Result<T>>,
    ) -> AppResult<T> {
        let cancellation = self.account_tasks_token();
        self.account_request_in_generation(request, cancellation).await
    }

    async fn account_request_in_generation<T>(
        &self,
        request: impl Future<Output = netodia_net::netease::Result<T>>,
        cancellation: CancellationToken,
    ) -> AppResult<T> {
        if cancellation.is_cancelled() {
            return Err(AppError::network_msg("网易云请求已取消"));
        }
        tokio::select! {
            () = cancellation.cancelled() => Err(AppError::network_msg("网易云请求已取消")),
            result = request => {
                if cancellation.is_cancelled() {
                    Err(AppError::network_msg("网易云请求已取消"))
                } else {
                    result.map_err(Into::into)
                }
            },
        }
    }

    /// Restores a saved cookie once during startup. A rejected cookie is deleted and leaves the
    /// application signed out; it is never exposed to the UI or written to logs.
    pub async fn restore_session(&self) -> AppResult<bool> {
        let _guard = self.transition.lock().await;
        let Some(saved) = self.pending_restore.lock().await.take() else {
            return Ok(false);
        };
        if saved.version != SESSION_VERSION || !valid_device_id(&saved.device_id) {
            let removed = tokio::task::spawn_blocking(delete_credential)
                .await
                .is_ok_and(|result| result.is_ok());
            self.publish(None, Some("已清除无法读取的登录信息。".into()), removed);
            return Ok(false);
        }

        self.begin_transition().await;
        self.client.restore_cookie(&saved.cookie).await;
        match self.client.account().await {
            Ok(account) => {
                self.guest_initialized.store(true, Ordering::Release);
                self.publish(Some(account), None, self.status().remember_available);
                Ok(true)
            }
            Err(error) => {
                self.client.clear_session().await;
                self.guest_initialized.store(false, Ordering::Release);
                if matches!(error, netodia_net::netease::Error::Authentication) {
                    let removed = tokio::task::spawn_blocking(delete_credential)
                        .await
                        .is_ok_and(|result| result.is_ok());
                    self.publish(None, Some("保存的登录已失效，请重新登录。".into()), removed);
                    Ok(false)
                } else {
                    *self.pending_restore.lock().await = Some(saved);
                    self.publish(
                        None,
                        Some("暂时无法连接网易云，登录恢复将在网络可用后重试。".into()),
                        self.status().remember_available,
                    );
                    Err(error.into())
                }
            }
        }
    }

    /// Initializes a guest session for endpoints that allow anonymous discovery.
    pub async fn ensure_guest(&self) -> AppResult<()> {
        if self.status().account.is_some() || self.guest_initialized.load(Ordering::Acquire) {
            return Ok(());
        }
        let _guard = self.transition.lock().await;
        if self.status().account.is_some() || self.guest_initialized.load(Ordering::Acquire) {
            return Ok(());
        }
        self.client.initialize_guest().await?;
        self.guest_initialized.store(true, Ordering::Release);
        Ok(())
    }

    pub async fn login_phone(
        &self,
        phone: &str,
        country_code: &str,
        password: &str,
    ) -> AppResult<Account> {
        validate_nonempty("手机号", phone)?;
        validate_nonempty("国家区号", country_code)?;
        validate_nonempty("密码", password)?;
        let _guard = self.transition.lock().await;
        self.begin_login().await;
        let account = self.client.login_phone(phone, country_code, password).await?;
        self.complete_login(account).await
    }

    pub async fn login_email(&self, email: &str, password: &str) -> AppResult<Account> {
        validate_nonempty("邮箱", email)?;
        validate_nonempty("密码", password)?;
        let _guard = self.transition.lock().await;
        self.begin_login().await;
        let account = self.client.login_email(email, password).await?;
        self.complete_login(account).await
    }

    pub async fn begin_qr_login(&self) -> AppResult<String> {
        let _guard = self.transition.lock().await;
        self.begin_login().await;
        self.client.qr_key().await.map_err(Into::into)
    }

    /// Starts the launch-page QR flow only when session restoration has left the user signed out.
    /// The transition lock keeps this check from racing a saved-cookie restore.
    pub async fn begin_qr_login_if_signed_out(&self) -> AppResult<Option<String>> {
        let _guard = self.transition.lock().await;
        if self.status().account.is_some() || self.pending_restore.lock().await.is_some() {
            return Ok(None);
        }
        self.begin_login().await;
        self.client.qr_key().await.map(Some).map_err(Into::into)
    }

    pub async fn check_qr_login(&self, key: &str) -> AppResult<QrStatus> {
        let _guard = self.transition.lock().await;
        let result = match self.client.qr_check(key).await? {
            NetEaseQrStatus::Expired => QrStatus::Expired,
            NetEaseQrStatus::Waiting => QrStatus::Waiting,
            NetEaseQrStatus::Confirming => QrStatus::Confirming,
            NetEaseQrStatus::Authorized => QrStatus::Authorized,
        };
        if result == QrStatus::Authorized {
            let account = self.client.account().await?;
            self.complete_login(account).await?;
        }
        Ok(result)
    }

    /// Invalidates account tasks and local session state before asking the server to end its
    /// session. Local logout still completes if the remote endpoint is unavailable.
    pub async fn logout(&self) -> AppResult<()> {
        let _guard = self.transition.lock().await;
        let cookie = self.client.cookie_for_storage().await;
        let device_id = self.client.device_id().to_owned();
        let http = self.http.clone();
        self.begin_transition().await;
        self.clear_liked_songs_cache(self.status().generation).await;
        if !cookie.is_empty()
            && let Ok(remote_client) = Client::with_device_id(http, device_id)
        {
            tokio::spawn(async move {
                remote_client.restore_cookie(&cookie).await;
                let _ = remote_client.logout().await;
            });
        }
        self.guest_initialized.store(false, Ordering::Release);
        self.pending_restore.lock().await.take();
        let deleted =
            tokio::task::spawn_blocking(delete_credential).await.is_ok_and(|result| result.is_ok());
        let notice = (!deleted).then(|| "已退出当前账号，但系统未能删除已保存的登录信息。".into());
        self.publish(None, notice, deleted);
        Ok(())
    }

    async fn begin_transition(&self) {
        self.account_tasks.read().cancel();
        self.liked_ids_revision.fetch_add(1, Ordering::AcqRel);
        *self.liked_ids_cache.write() = None;
        *self.recent_songs_cache.write() = None;
        self.playlist_cache.lock().clear();
        self.publish(None, None, self.status().remember_available);
        *self.account_tasks.write() = CancellationToken::new();
        self.client.clear_session().await;
        self.guest_initialized.store(false, Ordering::Release);
    }

    async fn begin_login(&self) {
        self.begin_transition().await;
        self.clear_liked_songs_cache(self.status().generation).await;
        self.pending_restore.lock().await.take();
        let deleted =
            tokio::task::spawn_blocking(delete_credential).await.is_ok_and(|result| result.is_ok());
        if !deleted {
            self.publish(
                None,
                Some("无法清除先前保存的登录信息；新登录成功后会尝试覆盖。".into()),
                false,
            );
        }
    }

    async fn complete_login(&self, account: Account) -> AppResult<Account> {
        self.guest_initialized.store(true, Ordering::Release);
        let cookie = self.client.cookie_for_storage().await;
        let saved = StoredSession {
            version: SESSION_VERSION,
            device_id: self.client.device_id().to_owned(),
            cookie,
        };
        let serialized = serde_json::to_string(&saved)
            .map_err(|_| AppError::network_msg("无法保存网易云登录状态"))?;
        let write = tokio::task::spawn_blocking(move || write_credential(&serialized)).await;
        let remembered = matches!(write, Ok(Ok(())));
        let notice = if remembered {
            None
        } else {
            Some("本机凭据存储不可用，本次登录只保留在内存中。".into())
        };
        self.publish(Some(account.clone()), notice, remembered);
        Ok(account)
    }

    fn publish(&self, account: Option<Account>, notice: Option<String>, remember_available: bool) {
        let generation = self.status.borrow().generation.wrapping_add(1);
        self.status.send_replace(SessionStatus { account, remember_available, notice, generation });
    }
}

fn validate_id(label: &str, id: i64) -> AppResult<()> {
    if id <= 0 { Err(AppError::Validation(format!("{label} ID 无效"))) } else { Ok(()) }
}

fn validate_ids(label: &str, ids: &[i64]) -> AppResult<()> {
    if ids.iter().any(|id| *id <= 0) {
        Err(AppError::Validation(format!("{label} ID 无效")))
    } else {
        Ok(())
    }
}

fn validate_nonempty(label: &str, value: &str) -> AppResult<()> {
    if value.trim().is_empty() {
        Err(AppError::Validation(format!("{label}不能为空")))
    } else {
        Ok(())
    }
}

fn valid_device_id(value: &str) -> bool {
    value.len() == 52 && value.bytes().all(|b| b.is_ascii_digit() || (b'A'..=b'F').contains(&b))
}

fn decode_session(value: &str) -> Option<StoredSession> {
    let session = serde_json::from_str::<StoredSession>(value).ok()?;
    (session.version == SESSION_VERSION
        && valid_device_id(&session.device_id)
        && !session.cookie.is_empty()
        && !session.cookie.contains(['\r', '\n']))
    .then_some(session)
}

fn read_credential() -> Result<Option<String>, ()> {
    let entry = keyring::Entry::new(KEYRING_SERVICE, KEYRING_USER).map_err(|_| ())?;
    match entry.get_password() {
        Ok(value) => Ok(Some(value)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(_) => Err(()),
    }
}

fn write_credential(value: &str) -> Result<(), ()> {
    let entry = keyring::Entry::new(KEYRING_SERVICE, KEYRING_USER).map_err(|_| ())?;
    entry.set_password(value).map_err(|_| ())
}

fn delete_credential() -> Result<(), ()> {
    let entry = keyring::Entry::new(KEYRING_SERVICE, KEYRING_USER).map_err(|_| ())?;
    match entry.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(_) => Err(()),
    }
}

#[cfg(test)]
#[path = "tests/cloud_favorites_tests.rs"]
mod favorites_tests;

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    #[test]
    fn netease_cover_requests_use_https_and_a_bounded_size() {
        assert_eq!(
            cover_request_url("http://p2.music.126.net/cover.jpg?token=abc")
                .as_ref()
                .map(reqwest::Url::as_str),
            Some("https://p2.music.126.net/cover.jpg?token=abc&param=512y512")
        );
    }

    #[test]
    fn existing_sizes_and_other_artwork_hosts_are_preserved() {
        assert_eq!(
            cover_request_url("https://p1.music.126.net/cover.jpg?param=224y224")
                .as_ref()
                .map(reqwest::Url::as_str),
            Some("https://p1.music.126.net/cover.jpg?param=224y224")
        );
        assert_eq!(
            cover_request_url("https://example.com/cover.jpg?token=abc")
                .as_ref()
                .map(reqwest::Url::as_str),
            Some("https://example.com/cover.jpg?token=abc")
        );
    }

    #[test]
    fn cover_requests_reject_non_http_urls() {
        assert!(cover_request_url("file:///tmp/cover.jpg").is_none());
        assert!(cover_request_url("https://").is_none());
    }

    const DEVICE_ID: &str = "0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123";

    pub(super) fn service_for_test() -> CloudService {
        let http = reqwest::Client::new();
        let (status, _) =
            watch::channel(SessionStatus { remember_available: true, ..SessionStatus::default() });
        CloudService {
            client: Client::new(http.clone()),
            http,
            pending_restore: Arc::new(Mutex::new(None)),
            status,
            transition: Arc::new(Mutex::new(())),
            account_tasks: Arc::new(parking_lot::RwLock::new(CancellationToken::new())),
            guest_initialized: Arc::new(AtomicBool::new(false)),
            liked_ids_cache: Arc::new(parking_lot::RwLock::new(None)),
            liked_ids_revision: Arc::new(AtomicU64::new(0)),
            recent_songs_cache: Arc::new(parking_lot::RwLock::new(None)),
            playlist_cache: Arc::new(parking_lot::Mutex::new(LruCache::new(PLAYLIST_CACHE_CAP))),
            favorites_cache: Arc::new(Mutex::new(FavoritesCache::new(std::path::PathBuf::new()))),
        }
    }

    // A cancelled account token makes an accidental metadata fetch fail before any HTTP traffic.
    pub(crate) fn service_without_network_for_test() -> CloudService {
        let service = service_for_test();
        service.guest_initialized.store(true, Ordering::Release);
        service.account_tasks_token().cancel();
        service
    }

    #[tokio::test]
    async fn private_requests_require_login_before_polling_the_network_future() {
        let service = service_for_test();
        let polled = Arc::new(AtomicBool::new(false));
        let future_polled = polled.clone();

        let result = service
            .private_request(async move {
                future_polled.store(true, Ordering::Release);
                Ok::<(), netodia_net::netease::Error>(())
            })
            .await;

        assert!(result.is_err());
        assert!(!polled.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn public_inputs_are_validated_without_initializing_a_guest_session() {
        let service = service_for_test();

        assert!(matches!(
            service.search("  ", SearchKind::Songs, 0).await,
            Err(AppError::Validation(_))
        ));
        assert!(matches!(service.songs(&[1, 0]).await, Err(AppError::Validation(_))));
        assert!(!service.guest_initialized.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn automatic_qr_login_preserves_a_pending_saved_session() {
        let service = service_for_test();
        *service.pending_restore.lock().await = Some(StoredSession {
            version: SESSION_VERSION,
            device_id: DEVICE_ID.to_owned(),
            cookie: "saved-cookie".to_owned(),
        });

        assert!(matches!(service.begin_qr_login_if_signed_out().await, Ok(None)));
        assert!(service.pending_restore.lock().await.is_some());
    }

    #[tokio::test]
    async fn cancelled_account_generation_does_not_poll_a_request() {
        let service = service_for_test();
        service.account_tasks_token().cancel();
        let polled = Arc::new(AtomicBool::new(false));
        let future_polled = polled.clone();

        let result = service
            .account_request(async move {
                future_polled.store(true, Ordering::Release);
                Ok::<(), netodia_net::netease::Error>(())
            })
            .await;

        assert!(result.is_err());
        assert!(!polled.load(Ordering::Acquire));
    }

    #[test]
    fn liked_membership_cache_is_scoped_to_account_and_generation() {
        let service = service_for_test();
        service.status.send_replace(SessionStatus {
            account: Some(Account { id: 42, ..Account::default() }),
            generation: 7,
            ..SessionStatus::default()
        });
        *service.liked_ids_cache.write() = Some((42, 7, HashSet::from([1001, 1002])));

        assert_eq!(service.cached_is_liked(1001), Some(true));
        assert_eq!(service.cached_is_liked(1003), Some(false));

        service.status.send_replace(SessionStatus {
            account: Some(Account { id: 43, ..Account::default() }),
            generation: 8,
            ..SessionStatus::default()
        });
        assert_eq!(service.cached_is_liked(1001), None);
    }

    #[test]
    fn recent_history_cache_is_scoped_to_account_and_generation() {
        let service = service_for_test();
        let recent = vec![RecentSong {
            song: Song { id: 42, name: "Cloud history".into(), ..Song::default() },
            played_at_ms: 1_700_000_000_000,
        }];
        service.status.send_replace(SessionStatus {
            account: Some(Account { id: 42, ..Account::default() }),
            generation: 7,
            ..SessionStatus::default()
        });
        *service.recent_songs_cache.write() = Some((42, 7, recent.clone()));

        assert_eq!(service.cached_recent_songs(), Some(recent));
        service.status.send_replace(SessionStatus {
            account: Some(Account { id: 43, ..Account::default() }),
            generation: 8,
            ..SessionStatus::default()
        });
        assert!(service.cached_recent_songs().is_none());
    }

    #[tokio::test]
    async fn an_account_transition_discards_the_previous_liked_snapshot() {
        let service = service_for_test();
        *service.liked_ids_cache.write() = Some((42, 0, HashSet::from([1001])));

        service.begin_transition().await;

        assert!(service.liked_ids_cache.read().is_none());
    }

    #[test]
    fn a_playlist_cache_entry_is_scoped_to_account_generation_and_playlist() {
        let service = service_for_test();
        service.status.send_replace(SessionStatus {
            account: Some(Account { id: 42, ..Account::default() }),
            generation: 7,
            ..SessionStatus::default()
        });
        let loaded = (Playlist { id: 500, ..Playlist::default() }, Arc::from([]));
        service.store_playlist(42, 7, 500, &loaded);

        assert!(service.cached_playlist(42, 7, 500).is_some());
        // Another account, another generation, or another playlist: all misses.
        assert!(service.cached_playlist(43, 7, 500).is_none());
        assert!(service.cached_playlist(42, 8, 500).is_none());
        assert!(service.cached_playlist(42, 7, 501).is_none());
    }

    #[test]
    fn a_cached_playlist_stops_being_served_at_the_ttl() {
        let fetched = Instant::now();
        let inside = PLAYLIST_CACHE_TTL.saturating_sub(Duration::from_millis(1));
        assert!(playlist_cache_is_fresh(fetched, fetched));
        assert!(playlist_cache_is_fresh(fetched, fetched + inside));
        assert!(!playlist_cache_is_fresh(fetched, fetched + PLAYLIST_CACHE_TTL));
    }

    fn playlist_fixture(id: i64, name: String) -> (Playlist, Arc<[Song]>) {
        (
            Playlist { id, ..Playlist::default() },
            vec![Song { id: 1, name, ..Song::default() }].into(),
        )
    }

    #[test]
    fn reopening_a_playlist_shares_its_song_storage() -> Result<(), &'static str> {
        let service = service_for_test();
        let loaded = playlist_fixture(500, "Cached song".to_owned());
        service.store_playlist(42, 7, 500, &loaded);

        let reopened = service.cached_playlist(42, 7, 500).ok_or("playlist not cached")?;

        assert!(Arc::ptr_eq(&loaded.1, &reopened.1));
        Ok(())
    }

    #[test]
    fn a_playlist_larger_than_the_byte_budget_is_not_retained() {
        let service = service_for_test();
        let loaded = playlist_fixture(500, "x".repeat(PLAYLIST_CACHE_BYTES));

        service.store_playlist(42, 7, 500, &loaded);

        assert!(service.cached_playlist(42, 7, 500).is_none());
    }

    #[test]
    fn playlist_bytes_evict_the_least_recently_used_entry_before_the_count_cap() {
        let service = service_for_test();
        let first = playlist_fixture(500, "a".repeat(3 * 1024 * 1024));
        let second = playlist_fixture(501, "b".repeat(3 * 1024 * 1024));
        let third = playlist_fixture(502, "c".repeat(3 * 1024 * 1024));
        service.store_playlist(42, 7, 500, &first);
        service.store_playlist(42, 7, 501, &second);
        assert!(service.cached_playlist(42, 7, 500).is_some());

        service.store_playlist(42, 7, 502, &third);

        assert!(service.cached_playlist(42, 7, 500).is_some());
        assert!(service.cached_playlist(42, 7, 501).is_none());
        assert!(service.cached_playlist(42, 7, 502).is_some());
    }

    #[test]
    fn saved_session_round_trips_without_changing_cookie_pairs() -> Result<(), serde_json::Error> {
        let session = StoredSession {
            version: SESSION_VERSION,
            device_id: DEVICE_ID.into(),
            cookie: "MUSIC_U=session-value; __csrf=csrf-value".into(),
        };
        let encoded = serde_json::to_string(&session)?;
        let decoded = decode_session(&encoded);
        assert!(decoded.is_some(), "the serialized session should be valid");
        let decoded = decoded.unwrap_or(StoredSession {
            version: 0,
            device_id: String::new(),
            cookie: String::new(),
        });
        assert_eq!(decoded.version, SESSION_VERSION);
        assert_eq!(decoded.device_id, DEVICE_ID);
        assert_eq!(decoded.cookie, "MUSIC_U=session-value; __csrf=csrf-value");
        Ok(())
    }

    #[test]
    fn invalid_saved_sessions_are_rejected() {
        for session in [
            format!(r#"{{"version":2,"device_id":"{DEVICE_ID}","cookie":"MUSIC_U=x"}}"#),
            r#"{"version":1,"device_id":"bad","cookie":"MUSIC_U=x"}"#.into(),
            format!(
                r#"{{"version":1,"device_id":"{DEVICE_ID}","cookie":"MUSIC_U=x\nInjected: y"}}"#
            ),
        ] {
            assert!(decode_session(&session).is_none());
        }
    }

    #[test]
    fn login_fields_reject_blank_values_before_network_access() {
        assert!(validate_nonempty("邮箱", " \t").is_err());
        assert!(validate_nonempty("邮箱", "user@example.com").is_ok());
    }
}
