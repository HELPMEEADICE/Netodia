use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use super::actions::emit_and_execute;
use super::backend::{PlaybackCheck, PlaybackEngine};
use super::event_sink::PlayerSinks;
use super::signal_path::{SignalPath, Transport};
use super::state::{
    PlayerAction, PlayerState, PlayerStateHandle, PositionTick, ReleaseDecision, lock_state,
};
use super::types::{PersistedPlayback, PlaybackStatus};
use netodia_playback::player::playback::crossfade;
use netodia_playback::player::playback::replaygain::TrackReplayGain;

/// How often the monitor wakes: tight enough that gapless preload triggers and
/// end-of-stream detection stay responsive, loose enough not to spin.
const POLL_INTERVAL_MS: u64 = 500;

/// The crossfade trigger window is `[MIN_FADE_MS, configured duration]`, and the
/// monitor only samples it once per poll. At the shortest configurable duration
/// it must still be **at least one poll wide**, or a tick can step straight over
/// it (e.g. 700 ms remaining → 200 ms remaining) and no crossfade fires. That is
/// not a benign miss: `crossfade_eligible` has already suppressed the gapless
/// preload by then, so the transition degrades to a decode-and-start hard cut —
/// an audible gap, worse than the gapless behaviour crossfade replaced.
const _: () = assert!(
    crossfade::MIN_CROSSFADE_MS as u64 >= crossfade::MIN_FADE_MS + POLL_INTERVAL_MS,
    "the shortest crossfade must leave a trigger window at least one poll wide"
);

/// How many ms before the end of the current track we stage the next gapless
/// source. Generous enough that the file can decode and queue before EOS even
/// on a slow disk, tight enough that mid-track repeat-mode / queue changes
/// have a chance to influence what gets preloaded.
const PRELOAD_LEAD_MS: u64 = 1500;

/// How much playback a `SIGKILL` may cost: the queue-and-position snapshot lands this often
/// while something is playing, and `main.rs`'s shutdown hook writes the authoritative one on
/// a clean exit.
///
/// Measured in playback rather than wall time: a poll that skips its tick returns
/// `Poll::Skipped`, which the counter never sees.
const SAVE_INTERVAL_MS: u64 = 30_000;

/// Polls spanning [`SAVE_INTERVAL_MS`]. Derived rather than spelled, so the interval its own
/// comment argues cannot drift from the count the loop actually applies.
const SAVE_EVERY_N_TICKS: u64 = SAVE_INTERVAL_MS / POLL_INTERVAL_MS;

const _: () = assert!(
    SAVE_EVERY_N_TICKS > 0,
    "the save cadence must span at least one poll, or its modulus is zero"
);

/// How long a pause may keep an exclusive device from everything else before the monitor gives
/// it back, where the user asked for that. Long enough that a short break keeps the claim, and
/// with it a resume that needs no reopen. The Output card's row spells the figure in its words.
const RELEASE_AFTER_PAUSE_MS: u64 = 5 * 60 * 1000;

/// Polls spanning [`RELEASE_AFTER_PAUSE_MS`], derived for the reason [`SAVE_EVERY_N_TICKS`] is.
const RELEASE_AFTER_N_TICKS: u64 = RELEASE_AFTER_PAUSE_MS / POLL_INTERVAL_MS;

/// Rate limiter on the position publish, admitting one tick per whole second.
///
/// The monitor wakes at [`POLL_INTERVAL_MS`] so the crossfade and gapless windows stay tight,
/// but the now-playing bar renders seconds and a slider thumb on a long track moves invisibly
/// between two 500 ms samples. Publishing at the poll rate makes the UI rebuild text content
/// twice per visible change, so the tick nobody can see is the one worth dropping.
///
/// A type rather than a local, because the seed is the half worth holding and a bare `&mut u64`
/// leaves it at whichever call site constructs one.
struct SecondGate(u64);

impl Default for SecondGate {
    /// Past any real position, so the first tick of a freshly started track publishes instead
    /// of waiting for its first second boundary.
    fn default() -> Self {
        Self(u64::MAX)
    }
}

impl SecondGate {
    /// Whether `position_ms` lands in a second the UI has not been shown, recording it either
    /// way. A seek backwards admits: what matters is that the second moved, not which way.
    fn admits(&mut self, position_ms: u64) -> bool {
        let second = position_ms / 1000;
        let moved = second != self.0;
        self.0 = second;
        moved
    }
}

/// The polls a pause has held an exclusive device for.
///
/// It starts over once it answers, so a release the state machine turned down, a seek having
/// landed in the gap, is asked for again a full period later rather than never. One that went
/// through parks the output, which stops the count before it gets that far.
#[derive(Default)]
struct PauseWatch(u64);

impl PauseWatch {
    /// Whether this poll ends a hold long enough to give the device back. Anything but a held
    /// pause starts the count over, so a play in between never lets two pauses add up.
    fn due(&mut self, holding: bool) -> bool {
        if !holding {
            self.0 = 0;
            return false;
        }
        self.0 += 1;
        if self.0 < RELEASE_AFTER_N_TICKS {
            return false;
        }
        self.0 = 0;
        true
    }
}

/// What the monitor read off the audio backend before taking the `PlayerState`
/// lock. Gathered first, deliberately: querying the backend under the state lock
/// would nest the decks mutex inside it.
#[derive(Copy, Clone)]
pub struct BackendSnapshot {
    /// Where the ear is, which is what the tick publishes and the state keeps.
    pub position_ms: u64,
    /// Where the deck has been pulled to, a device's worth ahead of the ear. What the crossfade
    /// and the gapless stage are timed against, since both act on what is pulled next.
    pub pulled_ms: u64,
    pub already_preloaded: bool,
    pub crossfading: bool,
    pub xf: crossfade::CrossfadeSettings,
    /// The output has to reopen for the track queued next, which no crossfade can cross.
    pub next_needs_reopen: bool,
}

/// What one `Playing` tick decided: the position to publish, and at most one of
/// a crossfade or a gapless preload — never both, since they are two ways to
/// make the *same* transition.
pub struct PlayingTick {
    pub tick: PositionTick,
    /// Path + baked `ReplayGain` of the track to stage behind the current one.
    pub late_preload: Option<(String, TrackReplayGain)>,
    pub crossfade: Option<crossfade::CrossfadeDecision>,
}

/// The whole of a `Playing` tick's decision, as a pure function of the state and
/// what the backend reported. `None` means the tick is void — playback moved off
/// `Playing` between the backend reads and the lock, so the caller skips it.
///
/// Split out of the monitor loop so the crossfade-vs-gapless gate can be tested
/// directly, without a running audio backend.
pub fn evaluate_playing_tick(
    state: &mut PlayerState,
    backend: BackendSnapshot,
) -> Option<PlayingTick> {
    let BackendSnapshot {
        position_ms,
        pulled_ms,
        already_preloaded,
        crossfading,
        xf,
        next_needs_reopen,
    } = backend;

    if state.status != PlaybackStatus::Playing {
        return None;
    }
    state.position_ms = position_ms;
    let tick = PositionTick { position_ms, duration_ms: state.duration_ms };

    let next = state.queue.peek_next();
    // The native engine can stage local decoders directly, while a cloud track needs an async
    // request before its source can be installed. Let external transitions reach EndOfStream so
    // the app-owned playback handler can resolve them without preloading an empty path.
    let external_transition = state.current_track().is_some_and(|track| track.file_path.is_empty())
        || next.is_some_and(|track| track.file_path.is_empty());
    let same_album = match (state.current_track(), next) {
        (Some(cur), Some(nxt)) => crossfade::same_album(cur, nxt),
        _ => false,
    };
    // Timing-INDEPENDENT: does this transition belong to the crossfade path at
    // all? The gapless preload below is gated on its negation, and it must not
    // depend on the position — a crossfade shorter than PRELOAD_LEAD_MS would
    // otherwise let the preload fire first, set `gapless_pending`, and
    // permanently block the crossfade via its own gate.
    //
    // A next track the output reopens for still reaches the preload below, which refuses it, so
    // the transition ends at `EndOfStream` either way.
    let eligible = crossfade::crossfade_eligible(
        xf,
        state.pause_after_current_track || next_needs_reopen || external_transition,
        next.is_some(),
        same_album,
    );

    // Carry the state this decision was made against. The caller holds the
    // `PlayerState` lock now but takes `exec_lock` only later, so a pause / stop
    // / seek / manual track change can land in between; `build_crossfade_actions`
    // re-verifies the whole snapshot under the emit lock and bails if any of it
    // moved.
    let crossfade = crossfade::should_crossfade(
        eligible,
        already_preloaded,
        crossfading,
        pulled_ms,
        state.duration_ms,
        xf.duration_ms,
    )
    .map(|fade_ms| crossfade::CrossfadeDecision {
        fade_ms,
        track_id: state.current_track().map(|t| t.id),
        position_ms: state.position_ms,
    });

    // Late gapless preload: stage the next track only when the current one is
    // within PRELOAD_LEAD_MS of ending. Doing it late lets mid-track repeat-mode
    // / queue mutations decide what plays next (an eager preload would lock in a
    // stale choice the moment the current track started).
    let late_preload = if state.gapless_enabled
        && !external_transition
        && !already_preloaded
        // A crossfade runs the next track on the *other* deck. A gapless source
        // would sit on this one, behind the outgoing track, and inherit its fade
        // cell.
        && !eligible
        && !crossfading
        // Sleep-timer "End of current track": suppress the gapless preload so the
        // current track drains to `EndOfStream` (not `GaplessTransition`, which
        // would already be playing the next track) — that's the only boundary
        // `build_end_of_stream_actions`' pause-at-track-end gate can catch.
        && !state.pause_after_current_track
        && state.duration_ms > 0
        // A deck reads 0 until it has been pulled for the source just started on
        // it, and a track shorter than the lead would read its whole length as
        // remaining and stage a preload on the spot.
        && pulled_ms > 0
        && pulled_ms <= state.duration_ms
        && state.duration_ms.saturating_sub(pulled_ms) < PRELOAD_LEAD_MS
    {
        // Capture the next track's baked ReplayGain alongside its path — it must
        // travel with *its own* source (the preloaded track has different tags
        // than the playing one), so the gain is baked per source, not shared.
        state.queue.peek_next().map(|t| (t.file_path.clone(), t.as_ref().into()))
    } else {
        None
    };

    Some(PlayingTick { tick, late_preload, crossfade })
}

/// Whether the output has to reopen for the track queued next, which no crossfade can cross.
///
/// The first ask about a track opens its file, so it is asked only while crossfade is on. That ask
/// usually lands on the track's first tick, well ahead of any crossfade window, and the engine
/// answers the rest from what it found. The file is opened with the state lock released.
fn next_needs_reopen(
    engine: &PlaybackEngine,
    player_state: &PlayerStateHandle,
    xf: crossfade::CrossfadeSettings,
) -> bool {
    if !xf.enabled {
        return false;
    }
    let next_path = {
        let state = lock_state(player_state);
        state.queue.peek_next().map(|track| track.file_path.clone())
    };
    next_path.is_some_and(|path| engine.reopens_for(&path))
}

/// What the monitor knows at a save point, for whoever owns the writing.
pub struct PlaybackSnapshot {
    /// The playing track and how far into it, `None` when nothing is.
    pub track: Option<(i64, u64)>,
    pub playback: PersistedPlayback,
}

/// How the monitor hands a snapshot over.
///
/// A sink rather than a `DbPool` and a `Paths`: what the engine knows is where playback got to,
/// and *where that is written down* is a question one layer up. Awaited inline at the call site,
/// so an in-flight save still completes before shutdown wins the next select.
pub type SnapshotSink =
    Arc<dyn Fn(PlaybackSnapshot) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;

/// All long-lived handles the playback monitor needs to operate. Bundled
/// so `spawn_playback_monitor` doesn't accumulate a long argument list as
/// the monitor's responsibilities grow.
pub struct PlaybackMonitorContext {
    pub shutdown_token: CancellationToken,
    pub player_state: Arc<PlayerStateHandle>,
    pub engine: Arc<PlaybackEngine>,
    pub sinks: Arc<PlayerSinks>,
    pub position_tx: watch::Sender<Option<PositionTick>>,
    pub signal_path_tx: watch::Sender<Option<SignalPath>>,
    pub save: SnapshotSink,
}

/// Spawns a single background task that handles position polling,
/// gapless transition detection, and end-of-stream detection.
pub fn spawn_playback_monitor(tracker: &TaskTracker, ctx: PlaybackMonitorContext) {
    let shutdown_token = ctx.shutdown_token.clone();
    let mut monitor = Monitor::new(ctx);
    tracker.spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_millis(POLL_INTERVAL_MS));
        loop {
            tokio::select! {
                biased;
                () = shutdown_token.cancelled() => {
                    log::info!("Playback monitor stopped");
                    break;
                }
                _ = interval.tick() => {}
            }
            if monitor.poll() == Poll::Counted {
                monitor.count_toward_save().await;
            }
        }
    });
}

/// Whether a poll counts toward the save cadence, which counts playback rather than wall time.
#[derive(PartialEq, Eq)]
enum Poll {
    Counted,
    Skipped,
}

/// The monitor's handles, and what it carries from one poll to the next.
struct Monitor {
    player_state: Arc<PlayerStateHandle>,
    engine: Arc<PlaybackEngine>,
    sinks: Arc<PlayerSinks>,
    position_tx: watch::Sender<Option<PositionTick>>,
    signal_path_tx: watch::Sender<Option<SignalPath>>,
    save: SnapshotSink,
    save_tick_counter: u64,
    publish: SecondGate,
    pause_watch: PauseWatch,
}

impl Monitor {
    fn new(ctx: PlaybackMonitorContext) -> Self {
        Self {
            player_state: ctx.player_state,
            engine: ctx.engine,
            sinks: ctx.sinks,
            position_tx: ctx.position_tx,
            signal_path_tx: ctx.signal_path_tx,
            save: ctx.save,
            save_tick_counter: 0,
            publish: SecondGate::default(),
            pause_watch: PauseWatch::default(),
        }
    }

    fn poll(&mut self) -> Poll {
        // Every read below takes the decks lock, which a reopen holds across a device open, and
        // waiting for it would block a runtime worker as long. The next tick reads the new stream.
        if self.engine.output_reopening() {
            return Poll::Skipped;
        }

        // Ahead of the not-playing short circuit below, because a stop is what retires the
        // most sources at once and it lands on exactly the ticks that circuit skips.
        self.engine.collect_spent();

        // Quick check: skip tick when not playing (lock-free via atomic mirror)
        let status = self.player_state.status_atomic.load(std::sync::atomic::Ordering::Relaxed);
        let paused = status == PlaybackStatus::Paused as u8;
        let release_due = self.pause_watch.due(paused && self.engine.holds_releasable_claim());

        if status != PlaybackStatus::Playing as u8 {
            if release_due {
                release_paused_track(&self.engine, &self.player_state, &self.sinks);
            }
            // A pause keeps the last path: nothing pulls the source, so it could not be
            // re-read anyway. One whose device went back has none left to describe.
            if status == PlaybackStatus::Stopped as u8 || (paused && self.engine.output_parked()) {
                publish_signal_path(&self.signal_path_tx, None);
            }
            return Poll::Skipped;
        }

        // A cloud seek suspends output while retaining the user's playing intent. Do not
        // publish the old clock or advance a drained source while its replacement loads.
        if self.engine.is_paused() {
            return Poll::Skipped;
        }

        // Single lock acquisition to avoid TOCTOU between gapless and EOS checks
        match self.engine.check_playback_state() {
            PlaybackCheck::GaplessTransition => {
                self.advance_gapless();
                Poll::Counted
            }
            PlaybackCheck::EndOfStream => self.end_of_stream(),
            PlaybackCheck::Playing => self.playing(),
        }
    }

    fn advance_gapless(&self) {
        emit_and_execute(&*self.engine, &self.player_state, &self.sinks, |state| {
            let mut actions = Vec::with_capacity(2);

            // Update play count for the track that just finished
            if let Some(track) = state.current_track() {
                actions.push(PlayerAction::UpdatePlayCount(track.id));
            }

            // Advance queue — update state only (the deck is already playing).
            // The next gapless preload is staged later, by the `Playing`
            // branch, when this new current track approaches its own end.
            if let Some(track) = state.queue.advance().cloned() {
                state.position_ms = 0;
                state.duration_ms = u64::try_from(track.duration_ms.max(0)).unwrap_or(0);
                state.source = Some(track);
            }

            actions
        });
    }

    fn end_of_stream(&self) -> Poll {
        // Advance the queue (or, if the sleep-timer's "End of current
        // track" mode is armed, disarm it and stop instead). See
        // `PlayerState::build_end_of_stream_actions`.
        let mut advanced = false;
        emit_and_execute(&*self.engine, &self.player_state, &self.sinks, |state| {
            // A seek or track switch may have won the execution lock after the EOS snapshot.
            if state.status != PlaybackStatus::Playing
                || self.engine.is_paused()
                || self.engine.holds_source()
            {
                return Vec::new();
            }
            advanced = true;
            state.build_end_of_stream_actions()
        });
        if advanced { Poll::Counted } else { Poll::Skipped }
    }

    /// Normal tick: update position with lightweight event.
    fn playing(&mut self) -> Poll {
        let backend = self.read_backend();
        let (decided, transport) = {
            let mut state = lock_state(&self.player_state);
            let transport = Transport {
                volume: state.volume,
                muted: state.is_muted,
                speed: state.playback_speed,
                crossfading: backend.crossfading,
            };
            (evaluate_playing_tick(&mut state, backend), transport)
        };
        publish_signal_path(&self.signal_path_tx, self.engine.signal_path(transport));
        let Some(PlayingTick { tick, late_preload, crossfade }) = decided else {
            return Poll::Skipped;
        };

        if self.publish.admits(tick.position_ms) {
            let _ = self.position_tx.send(Some(tick.clone()));
        }
        if let Some(mc) = self.sinks.media_controls.as_ref() {
            mc.update_position(tick.position_ms);
        }
        if let Some(decision) = crossfade {
            // Advance the queue and start the incoming track on the
            // idle deck in one serialized step. `emit_and_execute`
            // re-reads the queue *and* re-verifies the status, the
            // current track and the position under the exec lock, so
            // anything that landed since the decision above can't be
            // clobbered.
            emit_and_execute(&*self.engine, &self.player_state, &self.sinks, |state| {
                state.build_crossfade_actions(decision)
            });
        } else if let Some((path, rg)) = late_preload {
            self.engine.preload_gapless(Some(&path), rg);
        }
        Poll::Counted
    }

    /// Queried BEFORE locking `PlayerState` to avoid a nested lock — `evaluate_playing_tick`
    /// takes this as an input.
    fn read_backend(&self) -> BackendSnapshot {
        let xf = self.engine.crossfade_settings();
        // Ahead of the positions, which a first open of the next file would leave stale.
        let next_needs_reopen = next_needs_reopen(&self.engine, &self.player_state, xf);
        BackendSnapshot {
            position_ms: self.engine.query_heard_position(),
            pulled_ms: self.engine.query_position(),
            already_preloaded: self.engine.is_gapless_preloaded(),
            crossfading: self.engine.is_crossfading(),
            xf,
            next_needs_reopen,
        }
    }

    /// Snapshots under the state lock and awaits the sink inline — the monitor task
    /// itself is tracked by `tracker`, so an in-flight save completes before shutdown
    /// wins on the next select.
    async fn count_toward_save(&mut self) {
        self.save_tick_counter = (self.save_tick_counter + 1) % SAVE_EVERY_N_TICKS;
        if self.save_tick_counter != 0 {
            return;
        }
        let snapshot = {
            let state = lock_state(&self.player_state);
            PlaybackSnapshot {
                track: state
                    .current_track()
                    .filter(|track| !track.file_path.is_empty())
                    .map(|track| (track.id, state.position_ms)),
                playback: state.to_persisted(),
            }
        };
        (self.save)(snapshot).await;
    }
}

/// Take a long-paused track off the deck so its exclusive device goes back, leaving it paused
/// where the deck stopped.
///
/// The deck is read under the emit, in the lock order's own direction, and only once it has acted
/// on what it was sent. A seek the emit waited out has sent its swap without the deck having taken
/// it, so the deck still reads its old position, which the release would write back over the
/// seek's. A deck still catching up turns the release down, as a seek in the gap does.
fn release_paused_track(
    engine: &PlaybackEngine,
    player_state: &PlayerStateHandle,
    sinks: &PlayerSinks,
) {
    let seen = {
        let state = lock_state(player_state);
        state.current_track().map(|track| (track.id, state.position_ms))
    };
    let Some((track_id, position_ms)) = seen else {
        return;
    };
    emit_and_execute(engine, player_state, sinks, |state| {
        let Some(resume_ms) = engine.query_settled_position(position_ms) else {
            return Vec::new();
        };
        state.build_release_actions(ReleaseDecision { track_id, position_ms, resume_ms })
    });
}

/// Publish `path` only when it differs, so the panel repaints on a change rather than per tick.
fn publish_signal_path(tx: &watch::Sender<Option<SignalPath>>, path: Option<SignalPath>) {
    tx.send_if_modified(|current| {
        if *current == path {
            return false;
        }
        *current = path;
        true
    });
}

#[cfg(test)]
#[path = "tests/handlers_tests.rs"]
mod tests;
