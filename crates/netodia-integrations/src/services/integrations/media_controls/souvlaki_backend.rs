//! SMTC on Windows and `MediaPlayer` on macOS, through souvlaki.

use std::time::{Duration, Instant};

use souvlaki::{
    MediaControlEvent, MediaControls, MediaMetadata, MediaPlayback, MediaPosition, PlatformConfig,
};
use tokio::sync::mpsc;

use netodia_core::error::describe;
use netodia_engine::player::engine::event_sink::{MediaControlsSync, PlayerEvent};
#[cfg(windows)]
use netodia_engine::player::engine::now_playing::SourceSummary;
use netodia_engine::player::engine::state::{PlayerViewModelLight, amplitude_to_volume};
use netodia_engine::player::engine::types::PlaybackStatus;

use super::published::Published;
#[cfg(windows)]
use super::published::PublishedMetadata;
use super::{cover_url, forward};

/// How often a playing position is handed over. Both panels advance their own clock between
/// updates, so this only has to keep the two from visibly diverging.
const TIMELINE_REFRESH: Duration = Duration::from_secs(5);

#[cfg(windows)]
const METADATA_RETRY_INTERVAL: Duration = Duration::from_secs(5);

/// Wrapper around souvlaki's `MediaControls`.
/// `Option` allows graceful degradation when OS media controls are unavailable.
pub struct MediaControlsHandle {
    inner: std::sync::Mutex<MediaControlsInner>,
}

struct MediaControlsInner {
    controls: Option<MediaControls>,
    /// Retained on Windows so the deferred `attach_smtc` can wire souvlaki's
    /// event callback into the same channel `spawn_event_receiver` drains.
    /// SMTC can't be built until the OS window — and its `HWND` — exists,
    /// which is well after `AppState::init`.
    #[cfg(target_os = "windows")]
    event_tx: mpsc::Sender<PlayerEvent>,
    /// Last-synced state to avoid redundant SMTC calls on queue-only changes.
    published: Published,
    #[cfg(windows)]
    metadata_retry: MetadataRetry,
    last_timeline_push: Option<Instant>,
}

/// A rejected snapshot waits between state emits; changed metadata can publish at once.
#[cfg(windows)]
#[derive(Default)]
enum MetadataRetry {
    #[default]
    Ready,
    Pending {
        source: Option<PublishedMetadata>,
        after: Instant,
    },
}

#[cfg(windows)]
impl MetadataRetry {
    fn publish<E: std::error::Error>(
        &mut self,
        source: Option<&SourceSummary<'_>>,
        now: Instant,
        send: impl FnMut(MediaMetadata<'_>) -> Result<(), E>,
    ) -> bool {
        if let Self::Pending { source: held, after } = self
            && now < *after
            && PublishedMetadata::still_current(held.as_ref(), source)
        {
            return false;
        }
        let sent = publish_metadata(source, send);
        *self = if sent {
            Self::Ready
        } else {
            Self::Pending {
                source: source.map(PublishedMetadata::from),
                after: now + METADATA_RETRY_INTERVAL,
            }
        };
        sent
    }
}

impl MediaControlsHandle {
    /// macOS creates the controls eagerly, since it needs no window handle. On Windows,
    /// souvlaki's SMTC backend `expect`s a non-null `HWND` in `PlatformConfig` and panics without
    /// one; no OS window exists at `AppState::init` time, so creation is deferred to
    /// `attach_smtc`, called from the event loop once the Slint window is shown. Until then — and
    /// wherever init fails — the handle is an inert no-op.
    pub(super) fn new(tx: mpsc::Sender<PlayerEvent>) -> Self {
        #[cfg(not(target_os = "windows"))]
        let controls = {
            let controls = try_create_controls(None, tx);
            if controls.is_some() {
                log::info!("OS media controls initialized");
            }
            controls
        };

        #[cfg(target_os = "windows")]
        let controls: Option<MediaControls> = {
            log::info!("Windows SMTC init deferred until the OS window is shown");
            None
        };

        Self {
            inner: std::sync::Mutex::new(MediaControlsInner {
                controls,
                #[cfg(target_os = "windows")]
                event_tx: tx,
                published: Published::default(),
                #[cfg(windows)]
                metadata_retry: MetadataRetry::default(),
                last_timeline_push: None,
            }),
        }
    }
}

/// Build souvlaki controls and attach the event callback, folding any failure
/// into `None` with a warning. A `None` result leaves the handle an inert no-op.
fn try_create_controls(
    hwnd: Option<*mut std::ffi::c_void>,
    tx: mpsc::Sender<PlayerEvent>,
) -> Option<MediaControls> {
    match create_controls(hwnd, tx) {
        Ok(controls) => Some(controls),
        Err(e) => {
            log::warn!("Failed to initialize OS media controls: {}", describe(&e));
            None
        }
    }
}

#[cfg(target_os = "windows")]
impl MediaControlsHandle {
    /// Attach Windows System Media Transport Controls now that the OS window —
    /// and therefore a valid `HWND` — exists. Called from the event loop once
    /// the Slint window is shown (see `boot::ui_setup::install`).
    ///
    /// Returns `true` only when this call newly attached the controls.
    /// Idempotent: a call made when controls already exist (or when souvlaki
    /// fails to build them) returns `false` without rebuilding. The caller
    /// uses a `true` result to push a one-off state re-sync — `sync()` no-op'd
    /// every call while the handle was inert, so nothing has reached the OS
    /// panel yet.
    #[must_use]
    pub fn attach_smtc(&self, hwnd: *mut std::ffi::c_void) -> bool {
        // Clone the event sender under a short lock; bail if SMTC is already up.
        let tx = {
            let guard = self.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            if guard.controls.is_some() {
                return false;
            }
            guard.event_tx.clone()
        };

        // Build off-lock — `MediaControls::new` does COM/SMTC setup that should
        // not serialise behind `sync()` calls arriving from playback threads.
        let Some(controls) = try_create_controls(Some(hwnd), tx) else {
            return false;
        };

        let mut guard = self.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if guard.controls.is_some() {
            // Lost a race with another `attach_smtc` — drop the spare controls.
            return false;
        }
        guard.controls = Some(controls);
        log::info!("Windows SMTC attached");
        true
    }
}

fn create_controls(
    hwnd: Option<*mut std::ffi::c_void>,
    tx: mpsc::Sender<PlayerEvent>,
) -> Result<MediaControls, souvlaki::Error> {
    let config = PlatformConfig { dbus_name: "netodia", display_name: "Netodia", hwnd };

    let mut controls = MediaControls::new(config)?;

    controls.attach(move |event: MediaControlEvent| {
        if let Some(event) = translate_event(event) {
            forward(&tx, event);
        }
    })?;

    Ok(controls)
}

fn translate_event(event: MediaControlEvent) -> Option<PlayerEvent> {
    match event {
        MediaControlEvent::Play => Some(PlayerEvent::Play),
        MediaControlEvent::Pause => Some(PlayerEvent::Pause),
        MediaControlEvent::Toggle => Some(PlayerEvent::PlayPause),
        MediaControlEvent::Next => Some(PlayerEvent::Next),
        MediaControlEvent::Previous => Some(PlayerEvent::Previous),
        MediaControlEvent::Stop => Some(PlayerEvent::Stop),
        MediaControlEvent::SetPosition(MediaPosition(pos)) => {
            Some(PlayerEvent::SeekTo(u64::try_from(pos.as_millis()).unwrap_or(u64::MAX)))
        }
        MediaControlEvent::SetVolume(vol) => Some(PlayerEvent::SetVolume(amplitude_to_volume(vol))),
        // Relative seeks need current position; revisit when the library API lands a
        // SeekRelative variant or the sink can resolve it from state.
        MediaControlEvent::Seek(_) | MediaControlEvent::SeekBy(_, _) => {
            log::debug!("Relative seek media event ignored (needs library API support)");
            None
        }
        MediaControlEvent::Raise | MediaControlEvent::Quit => {
            // Window control needs a Slint window handle, which this layer
            // deliberately doesn't hold — see the `EventSink` split.
            log::debug!("Window control media event ignored");
            None
        }
        MediaControlEvent::OpenUri(uri) => {
            log::debug!("Media controls OpenUri requested (ignored): {uri}");
            None
        }
    }
}

impl MediaControlsSync for MediaControlsHandle {
    fn sync(&self, vm: &PlayerViewModelLight, status: PlaybackStatus) {
        let mut guard = self.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let inner = &mut *guard;

        let changes = inner.published.changes(vm, status);
        #[cfg(windows)]
        let mut changes = changes;
        let Some(controls) = inner.controls.as_mut() else {
            return;
        };

        #[cfg(windows)]
        if changes.metadata {
            changes.metadata =
                inner.metadata_retry.publish(vm.source().as_ref(), Instant::now(), |metadata| {
                    controls.set_metadata(metadata)
                });
        }

        #[cfg(target_os = "macos")]
        if changes.metadata {
            let source = vm.source();
            if let Some(source) = source.as_ref() {
                let artwork_url = source.artwork_path.map(cover_url);
                if let Err(e) = controls.set_metadata(MediaMetadata {
                    title: Some(source.title),
                    artist: source.secondary,
                    album: source.album,
                    duration: source.duration_ms.map(Duration::from_millis),
                    cover_url: artwork_url.as_deref(),
                }) {
                    log::debug!("Failed to set media metadata: {}", describe(&e));
                }
            } else if let Err(e) = controls.set_metadata(MediaMetadata::default()) {
                log::debug!("Failed to clear media metadata: {}", describe(&e));
            }
        }

        if changes.status || changes.metadata || changes.position {
            let progress = Some(MediaPosition(Duration::from_millis(vm.position_ms)));
            let playback = match status {
                PlaybackStatus::Playing => MediaPlayback::Playing { progress },
                PlaybackStatus::Paused => MediaPlayback::Paused { progress },
                PlaybackStatus::Stopped => MediaPlayback::Stopped,
            };
            if let Err(e) = controls.set_playback(playback) {
                log::debug!("Failed to set media playback status: {}", describe(&e));
            }
            inner.last_timeline_push = Some(Instant::now());
        }

        inner.published.record(vm, status, changes);
    }

    fn update_position(&self, position_ms: u64) {
        let mut guard = self.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let inner = &mut *guard;
        let Some(controls) = inner.controls.as_mut() else {
            return;
        };
        if inner.last_timeline_push.is_some_and(|at| at.elapsed() < TIMELINE_REFRESH) {
            return;
        }
        let progress = Some(MediaPosition(Duration::from_millis(position_ms)));
        if let Err(e) = controls.set_playback(MediaPlayback::Playing { progress }) {
            log::debug!("Failed to update media position: {}", describe(&e));
        }
        inner.last_timeline_push = Some(Instant::now());
    }
}

/// Windows publishes the text only after loading the cover. Retry once without artwork so an
/// unreadable file cannot keep the previous song on the panel. Accepted text settles this
/// snapshot; changed artwork will trigger another publication.
#[cfg(windows)]
fn publish_metadata<E: std::error::Error>(
    source: Option<&SourceSummary<'_>>,
    mut send: impl FnMut(MediaMetadata<'_>) -> Result<(), E>,
) -> bool {
    let Some(source) = source else {
        return match send(MediaMetadata {
            title: Some(""),
            artist: Some(""),
            album: Some(""),
            ..MediaMetadata::default()
        }) {
            Ok(()) => true,
            Err(e) => {
                log::debug!("Failed to clear media metadata: {}", describe(&e));
                false
            }
        };
    };

    let artwork_url = source.artwork_path.and_then(cover_url);
    let metadata = MediaMetadata {
        title: Some(source.title),
        artist: Some(source.secondary.unwrap_or("")),
        album: Some(source.album.unwrap_or("")),
        duration: source.duration_ms.map(Duration::from_millis),
        cover_url: artwork_url.as_deref(),
    };
    match send(metadata.clone()) {
        Ok(()) => true,
        Err(e) => {
            if metadata.cover_url.is_none() {
                log::debug!("Failed to set media metadata: {}", describe(&e));
                return false;
            }
            log::debug!("Failed to set media metadata with artwork: {}", describe(&e));
            let text_only = MediaMetadata { cover_url: None, ..metadata };
            match send(text_only) {
                Ok(()) => true,
                Err(e) => {
                    log::debug!("Failed to set media metadata: {}", describe(&e));
                    false
                }
            }
        }
    }
}

#[cfg(test)]
#[path = "tests/souvlaki_backend_tests.rs"]
mod tests;

#[cfg(all(test, windows))]
#[path = "tests/metadata_publishing_tests.rs"]
mod metadata_publishing_tests;
