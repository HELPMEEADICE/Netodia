use std::sync::Arc;
use std::sync::MutexGuard;
use std::sync::atomic::{AtomicU8, Ordering};

use super::event_sink::PlayerSinks;
use super::queue::QueueState;
use super::types::PlaybackStatus;
use netodia_core::entities::track::TrackSummary;

mod action;
mod persistence;
mod summary_sync;
mod transport;
mod view_model;

pub use action::PlayerAction;
pub use persistence::restore_queue;
pub use summary_sync::{any_tracked, sync_current_track_if_in, sync_track_summaries};
pub use transport::{ReleaseDecision, play_track_inner, resume_from_stopped, stop_end_of_queue};
pub use view_model::{PlayerViewModel, PlayerViewModelLight, PositionTick, QueueViewModel};

/// Maximum volume level (percent) stored in `PlayerState` and reachable from the
/// UI. Playback amplitude tops out at unity gain (see [`volume_to_amplitude`]),
/// so this is the true ceiling — there is no boost band above it.
pub const MAX_VOLUME: u32 = 100;
/// Minimum playback speed multiplier.
pub const MIN_SPEED: f64 = 0.25;
/// Maximum playback speed multiplier. Capped at 2×: speed is a ratio on the
/// deck's converter, which is naive resampling and shifts pitch with it, so
/// beyond 2× the audio degrades into chipmunk territory with little practical
/// use for music.
pub const MAX_SPEED: f64 = 2.0;

/// Single source of truth for converting a stored volume level (percent) plus a mute flag into
/// the linear amplitude the audio backend and OS media controls (MPRIS) both expect.
pub fn volume_to_amplitude(volume: u32, is_muted: bool) -> f64 {
    if is_muted { 0.0 } else { f64::from(volume) / 100.0 }
}

/// [`volume_to_amplitude`]'s way back, for an amplitude set outside the player: an OS media panel,
/// or the device's own volume control.
///
/// Clamped, because a host may send a value outside `[0, 1]`, and rounded rather than truncated,
/// so the quietest step is reachable from half a percent rather than a full one.
#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "clamped to [0, 1] before the scale, so the rounded value fits a u32"
)]
pub fn amplitude_to_volume(amplitude: f64) -> u32 {
    (amplitude.clamp(0.0, 1.0) * 100.0).round() as u32
}

pub struct PlayerState {
    pub status: PlaybackStatus,
    /// Track currently on the deck, if any.
    pub source: Option<Arc<TrackSummary>>,
    pub position_ms: u64,
    pub duration_ms: u64,
    pub volume: u32,
    pub is_muted: bool,
    pub pre_mute_volume: u32,
    pub playback_speed: f64,
    pub gapless_enabled: bool,
    /// Sleep-timer "End of current track" mode: when armed, the playback
    /// monitor pauses at the next end-of-stream boundary instead of advancing
    /// the queue. Session-only (never persisted). Set via
    /// `library::playback::player_set_pause_at_track_end`; surfaced to
    /// the UI as `sleep_at_track_end` on the light `ViewModel` so the overflow
    /// menu's sleep row auto-clears once the monitor fires and disarms it.
    pub pause_after_current_track: bool,
    pub queue: QueueState,
}

impl Default for PlayerState {
    fn default() -> Self {
        Self {
            status: PlaybackStatus::Stopped,
            source: None,
            position_ms: 0,
            duration_ms: 0,
            volume: 100,
            is_muted: false,
            pre_mute_volume: 100,
            playback_speed: 1.0,
            gapless_enabled: true,
            pause_after_current_track: false,
            queue: QueueState::default(),
        }
    }
}

/// Wrapper around `Mutex<PlayerState>` with an atomic playback status mirror.
/// The `AtomicU8` allows the playback monitor to skip lock acquisition
/// when the player is paused/stopped (the common idle case).
pub struct PlayerStateHandle {
    mutex: std::sync::Mutex<PlayerState>,
    /// Mirror of `PlayerState::status` as a `u8`, updated after every state change.
    pub status_atomic: AtomicU8,
    /// Serializes the *side-effect* phase across tasks. `with_state_emit` makes a
    /// single state mutation atomic, but the `execute_actions` that follows runs
    /// on whatever tokio worker the caller happens to be on. Without this,
    /// two batches (e.g. the monitor's EOS-advance and a UI `Stop`) can interleave
    /// their side effects on separate workers and leave state and backend
    /// disagreeing. `emit_and_execute` holds this across *both* the mutation and
    /// the execution so mutation order equals side-effect order. Held only across
    /// synchronous work (never an `.await`), so a blocking mutex is correct.
    exec_lock: std::sync::Mutex<()>,
}

impl Default for PlayerStateHandle {
    fn default() -> Self {
        Self {
            mutex: std::sync::Mutex::new(PlayerState::default()),
            status_atomic: AtomicU8::new(PlaybackStatus::Stopped as u8),
            exec_lock: std::sync::Mutex::new(()),
        }
    }
}

impl PlayerStateHandle {
    /// Acquire the execution lock, recovering from poison rather than panicking
    /// (mirrors [`lock_state`] / `PlaybackEngine::lock_decks`). The guarded unit
    /// carries no data — poison only means a prior holder panicked mid-batch, and
    /// the guard exists purely to serialize the next batch.
    pub fn lock_exec(&self) -> MutexGuard<'_, ()> {
        self.exec_lock.lock().unwrap_or_else(|poisoned| {
            log::error!("PlayerState exec lock was poisoned, recovering");
            poisoned.into_inner()
        })
    }
}

impl PlayerState {
    /// The track on the deck, if any.
    pub fn current_track(&self) -> Option<&Arc<TrackSummary>> {
        self.source.as_ref()
    }

    /// The track on the deck, for the two writers that mutate it in place.
    pub fn current_track_mut(&mut self) -> Option<&mut Arc<TrackSummary>> {
        self.source.as_mut()
    }
}

/// Lock the player state, recovering from mutex poison rather than panicking.
/// A poisoned mutex means a thread panicked while holding the lock — the state
/// may be inconsistent, but crashing the entire app is worse for a media player.
pub fn lock_state(handle: &PlayerStateHandle) -> MutexGuard<'_, PlayerState> {
    handle.mutex.lock().unwrap_or_else(|poisoned| {
        log::error!("PlayerState mutex was poisoned, recovering");
        poisoned.into_inner()
    })
}

/// Lock state, run mutation, build `ViewModel`, publish on the watch channels.
/// Makes forgetting `ViewModel` emission structurally impossible.
///
/// Always publishes a lightweight `ViewModel` (no queue data) on `sinks.view_model`.
/// Publishes a queue `ViewModel` on `sinks.queue` only when the queue version changed.
/// Synchronizes OS media controls (MPRIS / SMTC) when a sink is registered.
pub fn with_state_emit<F, R>(state: &PlayerStateHandle, sinks: &PlayerSinks, f: F) -> R
where
    F: FnOnce(&mut PlayerState) -> R,
{
    let mut guard = lock_state(state);
    let queue_version_before = guard.queue.version;
    let result = f(&mut guard);
    let status = guard.status;
    // Sync atomic mirror for lock-free status checks (e.g., playback monitor)
    state.status_atomic.store(status as u8, Ordering::Relaxed);
    let vm_light = guard.to_view_model_light();
    let queue_vm = if guard.queue.version == queue_version_before {
        None
    } else {
        Some(guard.to_queue_view_model())
    };
    drop(guard);

    // `mc.sync` borrows `vm_light`, so it runs before the send moves it.
    if let Some(qvm) = queue_vm {
        let _ = sinks.queue.send(Some(qvm));
    }
    if let Some(mc) = sinks.media_controls.as_ref() {
        mc.sync(&vm_light, status);
    }
    let _ = sinks.view_model.send(Some(vm_light));

    result
}

#[cfg(test)]
#[path = "../tests/state_tests.rs"]
mod tests;
