//! Joins typed `NetEase` stream tickets to the native decoder and output engine.

use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use netodia_audio::player::source::audio::AudioSource;
use netodia_audio::player::source::buffered::BufferedAudioSource;
use netodia_audio::player::source::file_decode::FileDecoder;
use netodia_core::entities::cloud::{Account, Quality, Song, StreamTicket};
use netodia_core::entities::track::TrackSummary;
use netodia_core::error::{AppError, AppResult};
use netodia_engine::player::engine::backend::PlaybackEngine;
use netodia_engine::player::engine::event_sink::{ExternalTrackPlayback, PlayerSinks};
use netodia_engine::player::engine::state::{PlayerStateHandle, lock_state, with_state_emit};
use netodia_engine::player::engine::types::{PlaybackStatus, RepeatMode};
use netodia_net::services::net::range_reader::RangeReader;
use netodia_playback::player::playback::replaygain::TrackReplayGain;
use parking_lot::RwLock;

use crate::cloud::{CloudService, QrStatus, SessionStatus};
use tokio::sync::watch;

#[derive(Clone)]
struct CloudQueuePlan {
    tracks: Vec<Arc<TrackSummary>>,
    start_index: usize,
    shuffle: bool,
}

enum QueueUpdate {
    Clear,
    Preserve,
    Replace(CloudQueuePlan),
}

struct ReservedPlayback {
    song_id: i64,
    quality: Quality,
    volume: f64,
    speed: f64,
    start_position: Option<Duration>,
    generation: u64,
    keep_paused: bool,
    queue_update: QueueUpdate,
}

#[derive(Clone)]
struct ActiveStreamTicket {
    ticket: StreamTicket,
    quality: Quality,
    expires_at: Instant,
}

struct ResolvedCloudTrack {
    active_ticket: ActiveStreamTicket,
    summary: Arc<TrackSummary>,
    cover_url: Option<String>,
    used_cached_ticket: bool,
}

impl ActiveStreamTicket {
    fn is_valid_for(&self, song_id: i64, quality: Quality) -> bool {
        self.ticket.song_id == song_id
            && self.quality == quality
            && self.expires_at > Instant::now()
    }
}

/// Result of resolving and handing a cloud track to the engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CloudPlaybackResult {
    /// False when a newer play, pause, stop, or account transition superseded this request.
    pub started: bool,
    /// The server marked the track as a limited trial.
    pub trial: bool,
}

/// Application-level orchestration for direct, buffered `NetEase` playback.
#[derive(Clone)]
pub struct CloudPlayback {
    cloud: CloudService,
    http: reqwest::Client,
    artwork_dir: PathBuf,
    engine: Arc<PlaybackEngine>,
    player_state: Arc<PlayerStateHandle>,
    sinks: Arc<PlayerSinks>,
    quality: Arc<RwLock<Quality>>,
    queue_generation: Arc<AtomicU64>,
    pending_queue_playback_generation: Arc<AtomicU64>,
    listening_generation: Arc<AtomicU64>,
    decoder_generation: Arc<AtomicU64>,
    active_stream_ticket: Arc<RwLock<Option<ActiveStreamTicket>>>,
}

impl CloudPlayback {
    /// Build the shared API and playback handles from the one HTTP client and artwork cache root.
    pub async fn initialize(
        http: reqwest::Client,
        artwork_dir: PathBuf,
        engine: Arc<PlaybackEngine>,
        player_state: Arc<PlayerStateHandle>,
        sinks: Arc<PlayerSinks>,
    ) -> (CloudService, Self) {
        let cloud = CloudService::new(http.clone()).await;
        let playback = Self::new(cloud.clone(), http, artwork_dir, engine, player_state, sinks);
        (cloud, playback)
    }

    pub fn new(
        cloud: CloudService,
        http: reqwest::Client,
        artwork_dir: PathBuf,
        engine: Arc<PlaybackEngine>,
        player_state: Arc<PlayerStateHandle>,
        sinks: Arc<PlayerSinks>,
    ) -> Self {
        Self {
            cloud,
            http,
            artwork_dir,
            engine,
            player_state,
            sinks,
            quality: Arc::new(RwLock::new(Quality::default())),
            queue_generation: Arc::new(AtomicU64::new(0)),
            pending_queue_playback_generation: Arc::new(AtomicU64::new(0)),
            listening_generation: Arc::new(AtomicU64::new(0)),
            decoder_generation: Arc::new(AtomicU64::new(0)),
            active_stream_ticket: Arc::new(RwLock::new(None)),
        }
    }

    pub fn quality(&self) -> Quality {
        *self.quality.read()
    }

    /// Identifies real cloud-track starts, including a repeat of the same song. Seeking and
    /// quality changes reopen the decoder with a start position and preserve this identity.
    pub fn listening_generation(&self) -> u64 {
        self.listening_generation.load(Ordering::Acquire)
    }

    /// Changes on every installed cloud decoder, allowing observers to discard seek jumps.
    pub fn decoder_generation(&self) -> u64 {
        self.decoder_generation.load(Ordering::Acquire)
    }

    pub fn set_quality(&self, quality: Quality) {
        *self.quality.write() = quality;
    }

    fn valid_stream_ticket(&self, song_id: i64, quality: Quality) -> Option<ActiveStreamTicket> {
        self.active_stream_ticket
            .read()
            .as_ref()
            .filter(|active| active.is_valid_for(song_id, quality))
            .cloned()
    }

    fn clear_stream_ticket_if_matches(&self, failed: &ActiveStreamTicket) {
        let mut active = self.active_stream_ticket.write();
        if active.as_ref().is_some_and(|current| current.ticket.url == failed.ticket.url) {
            *active = None;
        }
    }

    /// Apply a quality choice immediately and prepare the current cloud track for a source swap.
    /// The current deck is paused synchronously before the returned future starts resolving the
    /// replacement stream, so the old quality cannot continue playing during the request.
    pub fn change_quality(
        &self,
        quality: Quality,
    ) -> impl Future<Output = AppResult<CloudPlaybackResult>> + Send + 'static {
        let request = if self.quality() == quality {
            None
        } else {
            self.set_quality(quality);
            self.pause_for_quality_change()
        };
        let playback = self.clone();
        async move {
            match request {
                Some(request) => playback.play_reserved(request).await,
                None => Ok(CloudPlaybackResult { started: false, trial: false }),
            }
        }
    }

    /// Pause the active cloud source and capture its exact heard position before any await. A
    /// paused player remains paused after the replacement loads; a playing player resumes on it.
    fn pause_for_quality_change(&self) -> Option<ReservedPlayback> {
        let _exec = self.player_state.lock_exec();
        let state = lock_state(&self.player_state);
        let track = state.current_track()?;
        if !track.file_path.is_empty()
            || !matches!(state.status, PlaybackStatus::Playing | PlaybackStatus::Paused)
        {
            return None;
        }

        let song_id = track.id;
        let volume = state.effective_volume();
        let speed = state.playback_speed;
        let was_paused = state.status == PlaybackStatus::Paused;
        let position_ms = if self.engine.holds_source() {
            self.engine.query_heard_position()
        } else {
            state.position_ms
        }
        .min(state.duration_ms.saturating_sub(500));
        drop(state);

        self.engine.pause();
        let generation = self.engine.reserve_playback_request();
        with_state_emit(&self.player_state, &self.sinks, |state| {
            state.status = PlaybackStatus::Paused;
            state.position_ms = position_ms;
        });

        Some(ReservedPlayback {
            song_id,
            quality: self.quality(),
            volume,
            speed,
            start_position: Some(Duration::from_millis(position_ms)),
            generation,
            keep_paused: was_paused,
            queue_update: QueueUpdate::Preserve,
        })
    }

    pub fn account_status(&self) -> SessionStatus {
        self.cloud.status()
    }

    pub fn subscribe_account(&self) -> watch::Receiver<SessionStatus> {
        self.cloud.subscribe()
    }

    /// Queue cloud songs without querying the local library or narrowing their IDs.
    pub async fn enqueue(&self, song_ids: &[i64], play_next: bool) -> AppResult<()> {
        self.enqueue_in_generation(song_ids, play_next, None).await
    }

    pub fn queue_generation(&self) -> u64 {
        self.queue_generation.load(Ordering::Acquire)
    }

    /// Reports whether the stopped player is waiting for a queued cloud track to load.
    pub fn is_queue_track_loading(&self) -> bool {
        let generation = self.pending_queue_playback_generation.load(Ordering::Acquire);
        generation != 0 && self.engine.is_playback_request_current(generation)
    }

    /// A delayed roaming refill must not append to a playlist selected in the meantime.
    pub async fn enqueue_roaming(&self, song_ids: &[i64], generation: u64) -> AppResult<()> {
        self.enqueue_in_generation(song_ids, false, Some(generation)).await
    }

    async fn enqueue_in_generation(
        &self,
        song_ids: &[i64],
        play_next: bool,
        queue_generation: Option<u64>,
    ) -> AppResult<()> {
        if song_ids.is_empty() {
            return Ok(());
        }
        let generation = self.cloud.status().generation;
        let cancellation = self.cloud.account_tasks_token();
        let songs = self.cloud.songs(song_ids).await?;
        if cancellation.is_cancelled() || self.cloud.status().generation != generation {
            return Err(AppError::network_msg("网易云排队请求已取消"));
        }
        let mut by_id: HashMap<i64, Song> = songs.into_iter().map(|song| (song.id, song)).collect();
        let tracks = song_ids
            .iter()
            .filter_map(|id| by_id.remove(id))
            .map(|song| {
                let liked = self.cloud.cached_is_liked(song.id).unwrap_or(false);
                Arc::new(track_summary(song, None, liked))
            })
            .collect();
        with_state_emit(&self.player_state, &self.sinks, |state| {
            if queue_generation.is_some_and(|expected| self.queue_generation() != expected) {
                return;
            }
            if queue_generation.is_some()
                && ((state.status == PlaybackStatus::Stopped && !self.is_queue_track_loading())
                    || state.source.as_ref().is_none_or(|track| !track.file_path.is_empty()))
            {
                return;
            }
            queue_cloud_tracks(&mut state.queue, tracks, play_next);
        });
        Ok(())
    }

    /// Build the cloud queue in the supplied list order and start the selected song. Existing
    /// repeat and shuffle preferences are retained; `shuffle` forces shuffle on for Shuffle All.
    pub async fn play_queue(
        &self,
        song_ids: &[i64],
        selected_song_id: i64,
        shuffle: bool,
        volume: f64,
        speed: f64,
    ) -> AppResult<CloudPlaybackResult> {
        if song_ids.is_empty() {
            return Err(AppError::Queue("云端播放列表没有歌曲".to_owned()));
        }
        self.queue_generation.fetch_add(1, Ordering::AcqRel);
        let generation = self.engine.reserve_playback_request();
        let songs = self.cloud.songs(song_ids).await?;
        if !self.engine.is_playback_request_current(generation) {
            return Ok(CloudPlaybackResult { started: false, trial: false });
        }
        // Warm liked membership once so cloud queue/player hearts also reflect the account when
        // the Favorites page has not been visited in this process. A lookup failure must not make
        // an otherwise playable playlist fail.
        if self.cloud.cached_is_liked(selected_song_id).is_none() {
            let _ = self.cloud.liked_ids().await;
        }
        let mut by_id: HashMap<i64, Song> = songs.into_iter().map(|song| (song.id, song)).collect();
        let tracks: Vec<Arc<TrackSummary>> = song_ids
            .iter()
            .filter_map(|id| by_id.remove(id))
            .map(|song| {
                let liked = self.cloud.cached_is_liked(song.id).unwrap_or(false);
                Arc::new(track_summary(song, None, liked))
            })
            .collect();
        self.start_cloud_queue(generation, tracks, selected_song_id, shuffle, volume, speed).await
    }

    /// [`Self::play_queue`] for a caller that already holds the track list. The playlist detail
    /// does — its rows are the same songs, title through artwork — so a click there costs a stream
    /// ticket instead of a whole `song/detail` round trip for a list the screen is showing.
    pub async fn play_queue_tracks(
        &self,
        tracks: Vec<TrackSummary>,
        selected_song_id: i64,
        shuffle: bool,
        volume: f64,
        speed: f64,
    ) -> AppResult<CloudPlaybackResult> {
        if tracks.is_empty() {
            return Err(AppError::Queue("云端播放列表没有歌曲".to_owned()));
        }
        self.queue_generation.fetch_add(1, Ordering::AcqRel);
        let generation = self.engine.reserve_playback_request();
        let tracks: Vec<Arc<TrackSummary>> = tracks.into_iter().map(Arc::new).collect();
        self.start_cloud_queue(generation, tracks, selected_song_id, shuffle, volume, speed).await
    }

    /// Seat the queue and start `selected_song_id`, walking the candidates a repeat or shuffle
    /// preference allows until one of them actually starts.
    async fn start_cloud_queue(
        &self,
        generation: u64,
        tracks: Vec<Arc<TrackSummary>>,
        selected_song_id: i64,
        shuffle: bool,
        volume: f64,
        speed: f64,
    ) -> AppResult<CloudPlaybackResult> {
        let start_index = tracks
            .iter()
            .position(|track| track.id == selected_song_id)
            .ok_or_else(|| AppError::Queue("所选歌曲不在当前播放列表中".to_owned()))?;
        let (repeat_mode, shuffle_enabled) = {
            let state = lock_state(&self.player_state);
            (state.queue.repeat_mode, state.queue.shuffle_enabled)
        };
        let candidates =
            playback_candidates(tracks.len(), start_index, repeat_mode, shuffle || shuffle_enabled);
        let quality = self.quality();
        let mut last_error = None;
        for candidate in candidates {
            let song_id = tracks[candidate].id;
            match self
                .play_reserved(ReservedPlayback {
                    song_id,
                    quality,
                    volume,
                    speed,
                    start_position: None,
                    generation,
                    keep_paused: false,
                    queue_update: QueueUpdate::Replace(CloudQueuePlan {
                        tracks: tracks.clone(),
                        start_index: candidate,
                        shuffle,
                    }),
                })
                .await
            {
                Ok(result) if result.started => return Ok(result),
                Ok(result) => return Ok(result), // A newer selection superseded this request.
                Err(error) => last_error = Some(error),
            }
        }
        Err(last_error
            .unwrap_or_else(|| AppError::Queue("云端播放列表没有可播放的歌曲".to_owned())))
    }

    /// True when a cloud track was restored from disk and has no decoded source on the engine.
    /// A normal pause with an active deck must continue through the ordinary transport path.
    pub fn restored_cloud_track_waiting(&self) -> bool {
        let paused_cloud_track = {
            let state = lock_state(&self.player_state);
            state.status == PlaybackStatus::Paused
                && state.source.as_ref().is_some_and(|track| track.file_path.is_empty())
        };
        paused_cloud_track && !self.engine.holds_source()
    }

    /// Resolve and play the restored cloud queue from its saved track and position. The queue
    /// itself was loaded synchronously from `queue.json`, so this only fetches a fresh stream.
    pub async fn resume_restored_queue(&self) -> AppResult<()> {
        let (song_id, position_ms, volume, speed) = {
            let state = lock_state(&self.player_state);
            let Some(track) = state.current_track().filter(|track| track.file_path.is_empty())
            else {
                return Ok(());
            };
            (track.id, state.position_ms, state.effective_volume(), state.playback_speed)
        };
        if !self.restored_cloud_track_waiting() {
            return Ok(());
        }

        self.engine.stop();
        with_state_emit(&self.player_state, &self.sinks, |state| {
            state.status = PlaybackStatus::Stopped;
        });
        let generation = self.engine.reserve_playback_request();
        self.pending_queue_playback_generation.store(generation, Ordering::Release);
        let result = self
            .play_queued_track(
                song_id,
                volume,
                speed,
                generation,
                false,
                Some(Duration::from_millis(position_ms)),
            )
            .await;
        let _ = self.pending_queue_playback_generation.compare_exchange(
            generation,
            0,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
        result
    }

    /// Install the app-side resolver used when normal queue behavior selects a cloud track.
    pub fn external_track_handler(
        &self,
        runtime: tokio::runtime::Handle,
    ) -> Arc<dyn ExternalTrackPlayback> {
        Arc::new(CloudQueuePlaybackHandler { playback: self.clone(), runtime })
    }

    /// Account changes stop the previous account's audio before the service invalidates its
    /// account-scoped requests and session.
    pub async fn login_phone(
        &self,
        phone: &str,
        country_code: &str,
        password: &str,
    ) -> AppResult<Account> {
        self.engine.stop();
        self.cloud.login_phone(phone, country_code, password).await
    }

    pub async fn login_email(&self, email: &str, password: &str) -> AppResult<Account> {
        self.engine.stop();
        self.cloud.login_email(email, password).await
    }

    pub async fn begin_qr_login(&self) -> AppResult<String> {
        self.engine.stop();
        self.cloud.begin_qr_login().await
    }

    /// Starts the automatic launch-page QR flow without interrupting a restored account.
    pub async fn begin_qr_login_if_signed_out(&self) -> AppResult<Option<String>> {
        let key = self.cloud.begin_qr_login_if_signed_out().await?;
        if key.is_some() {
            self.engine.stop();
        }
        Ok(key)
    }

    pub async fn check_qr_login(&self, key: &str) -> AppResult<QrStatus> {
        let status = self.cloud.check_qr_login(key).await?;
        if status == QrStatus::Authorized {
            self.engine.stop();
        }
        Ok(status)
    }

    pub async fn logout(&self) -> AppResult<()> {
        self.engine.stop();
        self.cloud.logout().await
    }

    /// Resolve a short-lived ticket, open and seek the media source off the async executor, then
    /// hand the buffered decoder to the engine if the request is still current.
    pub async fn play(
        &self,
        song_id: i64,
        quality: Quality,
        volume: f64,
        speed: f64,
        start_position: Option<Duration>,
    ) -> AppResult<CloudPlaybackResult> {
        self.queue_generation.fetch_add(1, Ordering::AcqRel);
        let generation = self.engine.reserve_playback_request();
        self.play_reserved(ReservedPlayback {
            song_id,
            quality,
            volume,
            speed,
            start_position,
            generation,
            keep_paused: false,
            queue_update: QueueUpdate::Clear,
        })
        .await
    }

    /// Re-open a cloud stream at a new position. Cloud sources cannot use the local-file seek
    /// action, so seeking is a new range-backed decoder generation. The current-track check and
    /// generation reservation happen before the first await, preventing a delayed slider task
    /// from taking over a song selected after the user dragged it.
    pub async fn seek(&self, song_id: i64, position: Duration) -> AppResult<CloudPlaybackResult> {
        let (generation, volume, speed, was_paused, position) = {
            let _exec = self.player_state.lock_exec();
            let state = lock_state(&self.player_state);
            let Some(track) = state.source.as_ref() else {
                return Ok(CloudPlaybackResult { started: false, trial: false });
            };
            if track.id != song_id || !track.file_path.is_empty() {
                return Ok(CloudPlaybackResult { started: false, trial: false });
            }
            if !matches!(state.status, PlaybackStatus::Playing | PlaybackStatus::Paused) {
                return Ok(CloudPlaybackResult { started: false, trial: false });
            }
            let volume = state.effective_volume();
            let speed = state.playback_speed;
            let was_paused = state.status == PlaybackStatus::Paused;
            let requested_ms = u64::try_from(position.as_millis()).unwrap_or(u64::MAX);
            let position =
                Duration::from_millis(requested_ms.min(state.duration_ms.saturating_sub(500)));
            drop(state);
            let generation = self.engine.reserve_playback_request();
            (generation, volume, speed, was_paused, position)
        };
        self.play_reserved(ReservedPlayback {
            song_id,
            quality: self.quality(),
            volume,
            speed,
            start_position: Some(position),
            generation,
            keep_paused: was_paused,
            queue_update: QueueUpdate::Preserve,
        })
        .await
    }

    async fn play_reserved(&self, request: ReservedPlayback) -> AppResult<CloudPlaybackResult> {
        self.play_reserved_with_ticket(request, None).await
    }

    async fn resolve_playback_track(
        &self,
        song_id: i64,
        quality: Quality,
        cached_ticket: Option<ActiveStreamTicket>,
    ) -> AppResult<ResolvedCloudTrack> {
        let cached_ticket = cached_ticket.filter(|ticket| ticket.is_valid_for(song_id, quality));
        let cached_summary = cached_ticket.as_ref().and_then(|_| {
            lock_state(&self.player_state)
                .source
                .as_ref()
                .filter(|current| current.id == song_id && current.file_path.is_empty())
                .cloned()
        });
        if let (Some(active_ticket), Some(summary)) = (cached_ticket, cached_summary) {
            return Ok(ResolvedCloudTrack {
                active_ticket,
                summary,
                cover_url: None,
                used_cached_ticket: true,
            });
        }

        let song_ids = [song_id];
        let ticket_request = async {
            let ticket = self.cloud.stream_ticket(song_id, quality).await?;
            let issued_at = Instant::now();
            let expires_at = issued_at
                .checked_add(Duration::from_secs(ticket.expires_after_seconds))
                .unwrap_or(issued_at);
            Ok::<_, AppError>(ActiveStreamTicket { ticket, quality, expires_at })
        };
        let (active_ticket, songs) = tokio::try_join!(ticket_request, self.cloud.songs(&song_ids))?;
        let cached_artwork = lock_state(&self.player_state)
            .source
            .as_ref()
            .filter(|current| current.id == song_id && current.file_path.is_empty())
            .and_then(|current| current.artwork_path.clone());
        let song = songs
            .into_iter()
            .find(|song| song.id == song_id)
            .ok_or_else(|| AppError::network_msg("网易云未返回歌曲信息"))?;
        let cover_url = song.album.cover_url.clone();
        let is_favorite = self.cloud.cached_is_liked(song_id).unwrap_or(false);
        Ok(ResolvedCloudTrack {
            active_ticket,
            summary: Arc::new(track_summary(song, cached_artwork, is_favorite)),
            cover_url,
            used_cached_ticket: false,
        })
    }

    async fn open_source_with_expired_ticket_retry(
        &self,
        song_id: i64,
        quality: Quality,
        start: Duration,
        cancellation: &tokio_util::sync::CancellationToken,
        mut active_ticket: ActiveStreamTicket,
        mut used_cached_ticket: bool,
    ) -> AppResult<(ActiveStreamTicket, BufferedAudioSource)> {
        let source = loop {
            let url = active_ticket.ticket.url.clone();
            let extension = decoder_extension(&active_ticket.ticket.format).to_owned();
            let reported_size = active_ticket.ticket.size;
            let source_task = tokio::task::spawn_blocking(move || {
                open_buffered_source(&url, &extension, reported_size, start)
            });
            let source_result = tokio::select! {
                () = cancellation.cancelled() => {
                    return Err(AppError::network_msg("网易云播放请求已取消"));
                }
                result = source_task => {
                    result.map_err(|error| AppError::io("在线播放解码任务失败", error))?
                }
            };
            match source_result {
                Ok(source) => break source,
                Err(error) if used_cached_ticket && is_expired_stream_url_error(&error) => {
                    self.clear_stream_ticket_if_matches(&active_ticket);
                    let ticket = tokio::select! {
                        () = cancellation.cancelled() => {
                            return Err(AppError::network_msg("网易云播放请求已取消"));
                        }
                        result = self.cloud.stream_ticket(song_id, quality) => result?,
                    };
                    let issued_at = Instant::now();
                    let expires_at = issued_at
                        .checked_add(Duration::from_secs(ticket.expires_after_seconds))
                        .unwrap_or(issued_at);
                    active_ticket = ActiveStreamTicket { ticket, quality, expires_at };
                    used_cached_ticket = false;
                }
                Err(error) => return Err(error),
            }
        };
        Ok((active_ticket, source))
    }

    async fn play_reserved_with_ticket(
        &self,
        request: ReservedPlayback,
        cached_ticket: Option<ActiveStreamTicket>,
    ) -> AppResult<CloudPlaybackResult> {
        let ReservedPlayback {
            song_id,
            quality,
            volume,
            speed,
            start_position,
            generation,
            keep_paused,
            queue_update,
        } = request;
        let cancellation = self.cloud.account_tasks_token();
        let ResolvedCloudTrack { active_ticket, summary, cover_url, used_cached_ticket } =
            self.resolve_playback_track(song_id, quality, cached_ticket).await?;
        if cancellation.is_cancelled() {
            return Err(AppError::network_msg("网易云播放请求已取消"));
        }
        let should_fetch_cover = summary.artwork_path.is_none() && cover_url.is_some();

        let start = start_position.unwrap_or(Duration::ZERO);
        let (active_ticket, source) = self
            .open_source_with_expired_ticket_retry(
                song_id,
                quality,
                start,
                &cancellation,
                active_ticket,
                used_cached_ticket,
            )
            .await?;
        let trial = active_ticket.ticket.trial;
        if cancellation.is_cancelled() {
            return Err(AppError::network_msg("网易云播放请求已取消"));
        }

        // Serialize the deck handoff and the player-state publication together with local
        // transport actions. Otherwise the audio can start while the player bar still has no
        // current track, or an older network request can publish after a newer local selection.
        let _exec = self.player_state.lock_exec();
        let started = self.engine.play_prepared_source(
            generation,
            source,
            volume,
            speed,
            start_position,
            TrackReplayGain::default(),
        );
        if started {
            if keep_paused {
                self.engine.pause();
            }
            *self.active_stream_ticket.write() = Some(active_ticket);
            let duration_ms = u64::try_from(summary.duration_ms.max(0)).unwrap_or(0);
            let position_ms = u64::try_from(start.as_millis()).unwrap_or(u64::MAX);
            with_state_emit(&self.player_state, &self.sinks, |state| {
                self.decoder_generation.fetch_add(1, Ordering::AcqRel);
                if start_position.is_none() {
                    self.listening_generation.fetch_add(1, Ordering::AcqRel);
                }
                // A cloud queue replaces the prior queue only once its selected stream is ready,
                // so an unavailable song cannot destroy a working queue.
                match queue_update {
                    QueueUpdate::Clear => state.queue.clear(),
                    QueueUpdate::Preserve => {}
                    QueueUpdate::Replace(plan) => {
                        state.queue.clear();
                        state.queue.add_tracks(plan.tracks);
                        let _ = state.queue.skip_to_index(plan.start_index);
                        if plan.shuffle || state.queue.shuffle_enabled {
                            state.queue.shuffle_play_order_in_place(&mut rand::rng(), true);
                        }
                    }
                }
                state.source = Some(summary);
                state.status =
                    if keep_paused { PlaybackStatus::Paused } else { PlaybackStatus::Playing };
                state.duration_ms = duration_ms;
                state.position_ms = position_ms.min(duration_ms.saturating_sub(500));
            });
        }
        if started
            && should_fetch_cover
            && let Some(url) = cover_url
        {
            self.cache_cover_after_start(song_id, url);
        }
        Ok(CloudPlaybackResult { started, trial })
    }

    /// Cover traffic is detached from the audio handoff, so a slow image CDN cannot delay the
    /// first PCM samples. The updated summary is emitted after caching and decoded by the normal
    /// Now Playing artwork pipeline.
    fn cache_cover_after_start(&self, song_id: i64, url: String) {
        let http = self.http.clone();
        let artwork_dir = self.artwork_dir.clone();
        let player_state = self.player_state.clone();
        let sinks = self.sinks.clone();
        tokio::spawn(async move {
            let Some(path) = download_cover(&http, &url, &artwork_dir).await else { return };
            let _exec = player_state.lock_exec();
            with_state_emit(&player_state, &sinks, |state| {
                let Some(current) = state.source.as_ref() else { return };
                if current.id != song_id
                    || !current.file_path.is_empty()
                    || current.artwork_path.is_some()
                {
                    return;
                }
                let mut updated = (**current).clone();
                updated.artwork_path = Some(path);
                state.source = Some(Arc::new(updated));
            });
        });
    }
}

/// Routes normal queue transitions into the app's async cloud resolver. The queue has already
/// advanced before the engine calls this adapter, so it only needs the selected song identity.
struct CloudQueuePlaybackHandler {
    playback: CloudPlayback,
    runtime: tokio::runtime::Handle,
}

impl ExternalTrackPlayback for CloudQueuePlaybackHandler {
    fn play(&self, track: Arc<TrackSummary>) {
        let song_id = track.id;
        let (volume, speed, keep_paused) = {
            let state = lock_state(&self.playback.player_state);
            (state.effective_volume(), state.playback_speed, state.status == PlaybackStatus::Paused)
        };

        // Clear the drained/outgoing deck and mark the transition as waiting before any network
        // work begins. Otherwise the playback monitor can see the old deck at EOS and advance
        // the cloud queue again while the first ticket is still being resolved.
        self.playback.engine.stop();
        with_state_emit(&self.playback.player_state, &self.playback.sinks, |state| {
            state.status = PlaybackStatus::Stopped;
        });
        let generation = self.playback.engine.reserve_playback_request();
        self.playback.pending_queue_playback_generation.store(generation, Ordering::Release);
        let playback = self.playback.clone();
        self.runtime.spawn(async move {
            let result = playback
                .play_queued_track(song_id, volume, speed, generation, keep_paused, None)
                .await;
            let _ = playback.pending_queue_playback_generation.compare_exchange(
                generation,
                0,
                Ordering::AcqRel,
                Ordering::Acquire,
            );
            if let Err(error) = result {
                log::warn!(
                    "cloud queue track {song_id}: {}",
                    netodia_core::error::describe(&error)
                );
            }
        });
    }

    fn play_from(&self, track: Arc<TrackSummary>, position_ms: u64) {
        let song_id = track.id;
        let quality = self.playback.quality();
        let cached_ticket = self.playback.valid_stream_ticket(song_id, quality);
        let (volume, speed, position_ms) = {
            let state = lock_state(&self.playback.player_state);
            if state
                .source
                .as_ref()
                .is_none_or(|current| current.id != song_id || !current.file_path.is_empty())
            {
                return;
            }
            (
                state.effective_volume(),
                state.playback_speed,
                position_ms.min(state.duration_ms.saturating_sub(500)),
            )
        };

        // The engine released this source during a long pause. Keep the selected cloud track
        // seated but stopped while its decoder is prepared; a still-valid ticket can be reused.
        self.playback.engine.stop();
        let generation = self.playback.engine.reserve_playback_request();
        with_state_emit(&self.playback.player_state, &self.playback.sinks, |state| {
            state.status = PlaybackStatus::Stopped;
            state.position_ms = position_ms;
        });
        self.spawn_resume(song_id, quality, volume, speed, position_ms, generation, cached_ticket);
    }

    fn resume(&self, track: Arc<TrackSummary>) {
        let song_id = track.id;
        let quality = self.playback.quality();
        let (volume, speed, position_ms) = {
            let state = lock_state(&self.playback.player_state);
            if state
                .source
                .as_ref()
                .is_none_or(|current| current.id != song_id || !current.file_path.is_empty())
            {
                return;
            }
            let position_ms = if self.playback.engine.holds_source() {
                self.playback.engine.query_heard_position()
            } else {
                state.position_ms
            }
            .min(state.duration_ms.saturating_sub(500));
            (state.effective_volume(), state.playback_speed, position_ms)
        };

        let cached_ticket = self.playback.valid_stream_ticket(song_id, quality);
        if cached_ticket.is_some() && self.playback.engine.holds_source() {
            self.playback.engine.resume();
            return;
        }

        // The saved ticket has expired, does not match this source, or the decoder was released.
        // Keep any old decoder paused while a replacement is prepared.
        self.playback.engine.pause();
        let generation = self.playback.engine.reserve_playback_request();
        with_state_emit(&self.playback.player_state, &self.playback.sinks, |state| {
            state.status = PlaybackStatus::Paused;
            state.position_ms = position_ms;
        });
        self.spawn_resume(song_id, quality, volume, speed, position_ms, generation, cached_ticket);
    }
}

impl CloudQueuePlaybackHandler {
    fn spawn_resume(
        &self,
        song_id: i64,
        quality: Quality,
        volume: f64,
        speed: f64,
        position_ms: u64,
        generation: u64,
        cached_ticket: Option<ActiveStreamTicket>,
    ) {
        let playback = self.playback.clone();
        self.runtime.spawn(async move {
            let result = playback
                .play_reserved_with_ticket(
                    ReservedPlayback {
                        song_id,
                        quality,
                        volume,
                        speed,
                        start_position: Some(Duration::from_millis(position_ms)),
                        generation,
                        keep_paused: false,
                        queue_update: QueueUpdate::Preserve,
                    },
                    cached_ticket,
                )
                .await;
            if let Err(error) = result {
                log::warn!(
                    "cloud track resume {song_id}: {}",
                    netodia_core::error::describe(&error)
                );
            }
        });
    }
}

impl CloudPlayback {
    /// Try each queue entry at most once if a selected stream is unavailable. This shares the
    /// engine's repeat and shuffle order and stops rather than wrapping forever through a broken
    /// playlist.
    async fn play_queued_track(
        &self,
        first_song_id: i64,
        volume: f64,
        speed: f64,
        generation: u64,
        keep_paused: bool,
        start_position: Option<Duration>,
    ) -> AppResult<()> {
        let attempts = lock_state(&self.player_state).queue.len().max(1);
        let mut song_id = first_song_id;
        let mut last_error = None;
        for attempt in 0..attempts {
            match self
                .play_reserved(ReservedPlayback {
                    song_id,
                    quality: self.quality(),
                    volume,
                    speed,
                    start_position: if attempt == 0 { start_position } else { None },
                    generation,
                    keep_paused,
                    queue_update: QueueUpdate::Preserve,
                })
                .await
            {
                Ok(result) if result.started => return Ok(()),
                Ok(_) => return Ok(()), // A newer selection superseded this request.
                Err(error) => last_error = Some(error),
            }
            if attempt + 1 == attempts {
                break;
            }
            let Some(next_id) = self.advance_cloud_queue_after_failure(song_id, generation) else {
                break;
            };
            song_id = next_id;
        }

        if !self.engine.is_playback_request_current(generation)
            || !self.current_cloud_track_is(song_id)
        {
            return Ok(());
        }
        self.engine.stop();
        with_state_emit(&self.player_state, &self.sinks, |state| {
            state.status = PlaybackStatus::Stopped;
            state.position_ms = 0;
        });
        Err(last_error.unwrap_or_else(|| AppError::network_msg("播放列表中没有可播放的歌曲")))
    }

    fn current_cloud_track_is(&self, song_id: i64) -> bool {
        lock_state(&self.player_state)
            .source
            .as_ref()
            .is_some_and(|track| track.id == song_id && track.file_path.is_empty())
    }

    /// Advance after a failure only while the failed song still owns the waiting player state.
    fn advance_cloud_queue_after_failure(
        &self,
        failed_song_id: i64,
        generation: u64,
    ) -> Option<i64> {
        if !self.engine.is_playback_request_current(generation) {
            return None;
        }
        let mut next_id = None;
        with_state_emit(&self.player_state, &self.sinks, |state| {
            if state
                .source
                .as_ref()
                .is_none_or(|track| track.id != failed_song_id || !track.file_path.is_empty())
                || state.status != PlaybackStatus::Stopped
            {
                return;
            }
            if let Some(track) = state.queue.advance_after_failure().cloned() {
                next_id = Some(track.id);
                state.duration_ms = u64::try_from(track.duration_ms.max(0)).unwrap_or(0);
                state.position_ms = 0;
                state.source = Some(track);
            }
        });
        next_id
    }
}

async fn download_cover(
    http: &reqwest::Client,
    url: &str,
    dir: &std::path::Path,
) -> Option<String> {
    const MAX_BYTES: u64 = 1_048_576;
    let url = crate::cloud::cover_request_url(url)?;
    let bytes = netodia_net::services::net::get_capped(
        http,
        &url,
        "NetEase cover",
        Duration::from_secs(30),
        MAX_BYTES,
    )
    .await
    .ok()?;
    let dir = dir.to_path_buf();
    tokio::task::spawn_blocking(move || {
        netodia_artwork::media::image::artwork::store_image(&bytes, "jpg", &dir)
    })
    .await
    .ok()
    .flatten()
}

fn is_expired_stream_url_error(error: &AppError) -> bool {
    let mut cause: Option<&(dyn std::error::Error + 'static)> = Some(error);
    while let Some(current) = cause {
        if current
            .downcast_ref::<netodia_net::services::net::range_reader::MediaError>()
            .is_some_and(|error| {
                matches!(
                    error,
                    netodia_net::services::net::range_reader::MediaError::HttpStatus(
                        401 | 403 | 404 | 410
                    )
                )
            })
        {
            return true;
        }
        cause = std::error::Error::source(current);
    }
    false
}

/// Candidate order for a newly selected cloud queue item. Repeat-off respects the tail the user
/// selected from; looping modes wrap once. Shuffle anchors the requested item and randomizes the
/// remaining candidates before any network resolution begins.
fn playback_candidates(
    track_count: usize,
    start_index: usize,
    repeat_mode: RepeatMode,
    shuffle: bool,
) -> Vec<usize> {
    if start_index >= track_count {
        return Vec::new();
    }
    let mut candidates = vec![start_index];
    let mut remaining: Vec<usize> = if shuffle {
        (0..track_count).filter(|&index| index != start_index).collect()
    } else {
        let mut after: Vec<_> = (start_index + 1..track_count).collect();
        if repeat_mode.wraps() {
            after.extend(0..start_index);
        }
        after
    };
    if shuffle {
        use rand::seq::SliceRandom;
        remaining.shuffle(&mut rand::rng());
    }
    candidates.extend(remaining);
    candidates
}

fn track_summary(song: Song, artwork_path: Option<String>, is_favorite: bool) -> TrackSummary {
    let artist = song.artist_names();
    TrackSummary {
        id: song.id,
        file_path: String::new(),
        file_name: song.name.clone(),
        title: song.name,
        artist: (!artist.is_empty()).then_some(artist),
        album: (!song.album.name.is_empty()).then_some(song.album.name),
        duration_ms: i64::try_from(song.duration_ms).unwrap_or(i64::MAX),
        artwork_path,
        track_number: None,
        disc_number: None,
        last_position: 0,
        is_favorite,
        rating: 0,
        replaygain_track_gain: None,
        replaygain_track_peak: None,
        replaygain_album_gain: None,
        replaygain_album_peak: None,
    }
}

fn open_buffered_source(
    url: &str,
    extension: &str,
    reported_size: Option<u64>,
    start: Duration,
) -> AppResult<BufferedAudioSource> {
    let reader =
        RangeReader::open(url).map_err(|error| AppError::network("在线播放源加载失败", error))?;
    let byte_len = reader.byte_len().or(reported_size);
    let mut decoder = FileDecoder::open_reader(reader, extension, byte_len)?;
    if !start.is_zero() {
        decoder
            .try_seek(start)
            .map_err(|error| AppError::Player(format!("在线播放定位失败：{error}")))?;
    }
    BufferedAudioSource::open(move || Ok(decoder))
}

fn queue_cloud_tracks(
    queue: &mut netodia_engine::player::engine::queue::QueueState,
    tracks: Vec<Arc<TrackSummary>>,
    play_next: bool,
) {
    if play_next {
        for track in tracks.into_iter().rev() {
            queue.insert_next(track);
        }
    } else {
        queue.add_tracks(tracks);
    }
}

#[cfg(test)]
#[path = "tests/cloud_queue_tests.rs"]
mod queue_tests;

fn decoder_extension(format: &str) -> &'static str {
    match format.to_ascii_lowercase().as_str() {
        "flac" => "flac",
        "aac" => "aac",
        "m4a" => "m4a",
        _ => "mp3",
    }
}

#[cfg(test)]
mod tests {
    use super::{decoder_extension, open_buffered_source, playback_candidates, track_summary};
    use netodia_audio::player::source::audio::AudioSource;
    use netodia_core::entities::cloud::{Album, Artist, Song};
    use netodia_engine::player::engine::types::RepeatMode;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::thread;
    use std::time::Duration;

    fn serve_media(
        listener: TcpListener,
        bytes: Vec<u8>,
    ) -> thread::JoinHandle<std::io::Result<()>> {
        thread::spawn(move || {
            let (mut stream, _) = listener.accept()?;
            stream.set_read_timeout(Some(Duration::from_secs(2)))?;
            let mut request = Vec::new();
            let mut chunk = [0; 512];
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                let read = stream.read(&mut chunk)?;
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&chunk[..read]);
            }
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                bytes.len()
            )?;
            stream.write_all(&bytes)
        })
    }

    #[test]
    fn decoder_hint_accepts_known_formats_and_defaults_unknown_values() {
        for (input, expected) in
            [("mp3", "mp3"), ("FLAC", "flac"), ("aac", "aac"), ("m4a", "m4a"), ("unknown", "mp3")]
        {
            assert_eq!(decoder_extension(input), expected);
        }
    }

    #[test]
    fn unavailable_queue_candidates_follow_repeat_and_shuffle_rules() {
        assert_eq!(
            playback_candidates(4, 2, RepeatMode::Off, false),
            vec![2, 3],
            "repeat-off must not wrap before the selected queue position"
        );
        assert_eq!(playback_candidates(4, 2, RepeatMode::All, false), vec![2, 3, 0, 1]);
        assert_eq!(playback_candidates(4, 2, RepeatMode::One, false), vec![2, 3, 0, 1]);

        let shuffled = playback_candidates(8, 5, RepeatMode::Off, true);
        assert_eq!(shuffled.first(), Some(&5), "shuffle keeps the requested song first");
        let mut sorted = shuffled;
        sorted.sort_unstable();
        assert_eq!(sorted, (0..8).collect::<Vec<_>>(), "shuffle tries each queue row once");
    }

    #[test]
    fn cloud_track_summary_keeps_the_metadata_the_player_bar_renders() {
        let summary = track_summary(
            Song {
                id: 812,
                name: "Cloud song".to_owned(),
                artists: vec![Artist {
                    id: 91,
                    name: "Cloud artist".to_owned(),
                    ..Artist::default()
                }],
                album: Album { id: 77, name: "Cloud album".to_owned(), ..Album::default() },
                duration_ms: 181_000,
                ..Song::default()
            },
            Some("/cache/cover.jpg".to_owned()),
            true,
        );

        assert_eq!(summary.id, 812);
        assert_eq!(summary.title, "Cloud song");
        assert_eq!(summary.artist.as_deref(), Some("Cloud artist"));
        assert_eq!(summary.album.as_deref(), Some("Cloud album"));
        assert_eq!(summary.duration_ms, 181_000);
        assert_eq!(summary.artwork_path.as_deref(), Some("/cache/cover.jpg"));
        assert!(summary.file_path.is_empty());
        assert!(summary.is_favorite);
    }

    #[test]
    fn a_non_range_cdn_response_reaches_the_buffered_decoder()
    -> Result<(), Box<dyn std::error::Error>> {
        let bytes =
            std::fs::read(std::path::Path::new(netodia_testkit::ASSETS_DIR).join("silence.wav"))?;
        let length = u64::try_from(bytes.len())?;
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let url = format!("http://{}/track.wav", listener.local_addr()?);
        let server = serve_media(listener, bytes);

        let mut source = open_buffered_source(&url, "wav", Some(length), Duration::ZERO)?;

        assert_eq!(source.sample_rate().get(), 44_100);
        assert_eq!(source.channels().get(), 1);
        assert_eq!(source.total_duration(), Some(Duration::from_secs(1)));
        assert!(source.next().is_some());
        server.join().map_err(|_| std::io::Error::other("media server panicked"))??;
        Ok(())
    }
}
