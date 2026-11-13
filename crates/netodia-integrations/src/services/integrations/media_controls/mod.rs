//! OS media controls: MPRIS on Linux, SMTC on Windows, `MediaPlayer` on macOS.
//!
//! `cfg`-split, with this module the shared façade. Linux serves MPRIS itself rather than through
//! souvlaki, whose `Position` is whatever it was last pushed: keeping that current means pushing
//! on a timer, and every push re-announces `PlaybackStatus`, which KDE Connect forwards to each
//! paired phone as a packet.

use std::sync::Arc;

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use netodia_core::error::describe;
use netodia_engine::player::engine::event_sink::{EventSink, PlayerEvent};

#[cfg(target_os = "linux")]
mod mpris_backend;
mod published;
#[cfg(any(target_os = "windows", target_os = "macos"))]
mod souvlaki_backend;

#[cfg(target_os = "linux")]
pub use mpris_backend::MediaControlsHandle;
#[cfg(any(target_os = "windows", target_os = "macos"))]
pub use souvlaki_backend::MediaControlsHandle;

/// Commands waiting between the OS callback and the player. Bounded, so a stalled consumer costs
/// key presses rather than memory.
const EVENT_CHANNEL_CAPACITY: usize = 32;

/// Initialize the OS media controls and the channel their commands arrive on.
///
/// A handle that could not reach the OS is an inert no-op rather than an error: a headless
/// session or a missing bus leaves playback exactly as usable.
pub fn init_media_controls() -> (MediaControlsHandle, mpsc::Receiver<PlayerEvent>) {
    let (tx, rx) = mpsc::channel(EVENT_CHANNEL_CAPACITY);
    (MediaControlsHandle::new(tx), rx)
}

/// Spawn a background task that feeds OS media control commands into the application's
/// `EventSink`.
///
/// The channel decouples the OS callback thread from the player state — the callback never
/// blocks on `PlayerState` or `MediaControlsHandle` locks, avoiding deadlocks.
pub fn spawn_event_receiver(
    tracker: &TaskTracker,
    shutdown_token: CancellationToken,
    mut rx: mpsc::Receiver<PlayerEvent>,
    sink: Arc<dyn EventSink>,
) {
    tracker.spawn(async move {
        loop {
            tokio::select! {
                biased;
                () = shutdown_token.cancelled() => break,
                maybe_event = rx.recv() => match maybe_event {
                    Some(event) => sink.handle(event),
                    None => break,
                },
            }
        }
        log::info!("Media control event receiver stopped");
    });
}

/// Hand a command to the player without blocking the OS thread it arrived on.
fn forward(tx: &mpsc::Sender<PlayerEvent>, event: PlayerEvent) {
    if let Err(e) = tx.try_send(event) {
        log::warn!("Dropped media control event due to full channel: {}", describe(&e));
    }
}

/// Publishes only local covers. Remote artwork is downloaded through the app's capped fetch
/// before its cache path replaces the queue's URL.
#[cfg(windows)]
fn cover_url(artwork_path: &str) -> Option<String> {
    if !std::path::Path::new(artwork_path).is_absolute() {
        return None;
    }
    // WinRT's file loader rejects forward slashes even in an absolute Windows path.
    let artwork_path = artwork_path.replace('/', std::path::MAIN_SEPARATOR_STR);
    Some(format!("file://{artwork_path}"))
}

#[cfg(not(windows))]
fn cover_url(artwork_path: &str) -> String {
    format!("file://{artwork_path}")
}

#[cfg(all(test, windows))]
#[path = "tests/cover_url_tests.rs"]
mod tests;
