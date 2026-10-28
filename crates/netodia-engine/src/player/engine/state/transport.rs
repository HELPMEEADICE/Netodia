//! The track transport: every builder a play, pause, skip, seek or end-of-stream goes through, and
//! the one writer of what "now playing this track" means.

use std::sync::Arc;

use super::volume_to_amplitude;
use super::{MAX_SPEED, MAX_VOLUME, MIN_SPEED, PlayerAction, PlayerState};
use crate::player::engine::types::PlaybackStatus;
use netodia_core::entities::track::TrackSummary;
use netodia_playback::player::playback::crossfade::CrossfadeDecision;
use netodia_playback::player::playback::replaygain::TrackReplayGain;

impl PlayerState {
    /// This state's volume as a backend amplitude, through the [`volume_to_amplitude`] the MPRIS
    /// path shares.
    pub fn effective_volume(&self) -> f64 {
        volume_to_amplitude(self.volume, self.is_muted)
    }

    /// Build actions for play/resume command.
    ///
    pub fn build_play_actions(&mut self) -> Vec<PlayerAction> {
        if self.status == PlaybackStatus::Paused {
            self.status = PlaybackStatus::Playing;
            vec![PlayerAction::Resume]
        } else {
            resume_from_stopped(self)
        }
    }

    /// Build actions for a play command over a paused track its deck no longer holds, which is
    /// what a pause long enough to give an exclusive device back leaves. Nothing is there to
    /// resume, so the track starts again from its position, reclaiming the device on the way.
    ///
    /// The deck is the backend's to answer, so `library::playback` picks this over
    /// [`Self::build_play_actions`] with the state lock held.
    pub fn build_replay_actions(&mut self) -> Vec<PlayerAction> {
        if self.status != PlaybackStatus::Paused {
            return vec![];
        }
        replay_current_track(self).unwrap_or_default()
    }

    /// Build actions for pause command. `fade_ms` is the pause-fade length when
    /// that setting is on, else `0` — same contract as [`Self::build_stop_actions`].
    ///
    pub fn build_pause_actions(&mut self, fade_ms: u64) -> Vec<PlayerAction> {
        if self.status == PlaybackStatus::Playing {
            self.status = PlaybackStatus::Paused;
            vec![PlayerAction::Pause { fade_ms }]
        } else {
            vec![]
        }
    }

    /// Take a long-paused track off the deck so the exclusive device it holds goes back to the
    /// system, leaving it paused where the deck stopped for [`Self::build_replay_actions`] to
    /// start it again from.
    ///
    /// The monitor decided this outside the emit lock, so what it
    /// saw is checked again: still paused on the same track at the same position, a seek in the
    /// gap having moved only the last of those.
    pub fn build_release_actions(&mut self, decision: ReleaseDecision) -> Vec<PlayerAction> {
        let same_track = self.current_track().map(|track| track.id) == Some(decision.track_id);
        if self.status != PlaybackStatus::Paused
            || !same_track
            || self.position_ms != decision.position_ms
        {
            return vec![];
        }
        self.position_ms = decision.resume_ms;
        vec![PlayerAction::Stop { fade_ms: 0 }]
    }

    /// Build actions for a stop: back to the top of a track that stays seated, so play starts it
    /// over. A pause is what keeps the position. `fade_ms` is the pause-fade length when that
    /// setting is on, else `0`.
    ///
    pub fn build_stop_actions(&mut self, fade_ms: u64) -> Vec<PlayerAction> {
        self.status = PlaybackStatus::Stopped;
        self.position_ms = 0;
        vec![PlayerAction::Stop { fade_ms }]
    }

    /// The seek action for whatever track is on the deck, or nothing where none is.
    ///
    /// The track rides along because the backend rebuilds the source to move it; see
    /// [`PlayerAction::Seek`].
    fn seek_action(&self, position_ms: u64) -> Option<PlayerAction> {
        let track = self.source.as_ref()?;
        Some(PlayerAction::Seek {
            position_ms,
            file_path: track.file_path.clone(),
            replaygain: track.as_ref().into(),
        })
    }

    /// Move the position, and build the action that moves the deck with it.
    ///
    /// The position moves either way. Nothing seated is nothing for the backend to rebuild, but
    /// this is reachable before anything has played — where the action the old in-place seek emitted was a no-op by the time it landed
    /// on an empty deck.
    fn build_move_to_actions(&mut self, position_ms: u64) -> Vec<PlayerAction> {
        let seek = self.seek_action(position_ms);
        self.position_ms = position_ms;
        seek.into_iter().collect()
    }

    /// Build actions for seek command.
    pub fn build_seek_actions(&mut self, position_ms: u64) -> Vec<PlayerAction> {
        self.build_move_to_actions(position_ms)
    }

    /// Build actions for next-track command.
    pub fn build_next_actions(&mut self) -> Vec<PlayerAction> {
        let mut actions = vec![];
        let was_paused = self.status == PlaybackStatus::Paused;

        if let Some(track) = self.current_track()
            && self.duration_ms > 0
            && self.position_ms < self.duration_ms / 2
        {
            actions.push(PlayerAction::UpdateSkipCount(track.id));
        }

        if let Some(track) = self.queue.advance_skip().cloned() {
            actions.extend(play_track_inner(self, track, None));
            self.restore_paused(was_paused, &mut actions);
        } else {
            actions.extend(stop_end_of_queue(self));
        }

        actions
    }

    /// Re-pause after a track change that was made while paused.
    ///
    /// `fade_ms: 0` is load-bearing: the `PlayMedia` this follows has just
    /// *started* the deck, and this only restores the paused state. A fade here
    /// would ramp the incoming track down from full volume instead of pausing it,
    /// so its first quarter-second would be audible — and its decoder would be
    /// that far in on resume.
    fn restore_paused(&mut self, was_paused: bool, actions: &mut Vec<PlayerAction>) {
        if was_paused {
            self.status = PlaybackStatus::Paused;
            actions.push(PlayerAction::Pause { fade_ms: 0 });
        }
    }

    /// Step back in the queue regardless of the current playback position.
    pub fn build_previous_actions(&mut self) -> Vec<PlayerAction> {
        let was_paused = self.status == PlaybackStatus::Paused;

        if let Some(track) = self.queue.previous().cloned() {
            let mut actions = play_track_inner(self, track, None);
            self.restore_paused(was_paused, &mut actions);
            actions
        } else {
            self.build_move_to_actions(0)
        }
    }

    /// Build actions for set-volume command.
    pub fn build_set_volume_actions(&mut self, level: u32) -> Vec<PlayerAction> {
        self.volume = level.min(MAX_VOLUME);
        self.is_muted = false;
        vec![PlayerAction::SetVolume(self.effective_volume())]
    }

    /// Build actions for toggle-mute command.
    pub fn build_toggle_mute_actions(&mut self) -> Vec<PlayerAction> {
        let new_muted = !self.is_muted;
        if new_muted {
            self.pre_mute_volume = self.volume;
        }
        self.is_muted = new_muted;
        vec![PlayerAction::SetVolume(self.effective_volume())]
    }

    /// Build actions when the current source drains to end-of-stream (the
    /// playback monitor's `EndOfStream` branch). Normally advances the queue
    /// (or stops at the end), but when the sleep-timer's "End of current track"
    /// mode is armed it disarms the flag and stops instead of advancing —
    /// leaving `current_track` at position 0 for replay-from-start. Always
    /// counts a play for the track that just ended.
    pub fn build_end_of_stream_actions(&mut self) -> Vec<PlayerAction> {
        let mut actions = Vec::with_capacity(4);

        if let Some(track) = self.current_track() {
            actions.push(PlayerAction::UpdatePlayCount(track.id));
        }

        if self.pause_after_current_track {
            self.pause_after_current_track = false;
            actions.extend(stop_end_of_queue(self));
            return actions;
        }

        if let Some(track) = self.queue.advance().cloned() {
            actions.extend(play_track_inner(self, track, None));
        } else {
            actions.extend(stop_end_of_queue(self));
        }

        actions
    }

    /// Build actions when the playback monitor decides the current track should
    /// start overlapping the next one. Mirrors `build_end_of_stream_actions`,
    /// but the outgoing track stays audible for `fade_ms` while the incoming
    /// one ramps up on the other deck.
    ///
    /// State advances at fade *start*, so Now-Playing switches to the incoming
    /// track as the overlap begins.
    /// Returns an empty vec (no crossfade) when the queue has moved on and
    /// there is no longer a next track, or when the decision has gone stale.
    ///
    /// `decision` carries the state the monitor was looking at when it chose to
    /// crossfade. It makes that choice under the `PlayerState` lock but only
    /// reaches here after acquiring `exec_lock`, so any other control op — pause,
    /// stop, next, previous, picking a track, seeking — can complete in between.
    /// Re-verifying here is the same discipline as the `queue.advance()` below:
    ///
    /// - **status** — without it, forcing `Playing` would resurrect playback the
    ///   user just paused. `BeginCrossfade` calls `Player::play()`, so it really
    ///   would be audible.
    /// - **track id** — without it, `advance()` would skip straight past the
    ///   track they just picked.
    /// - **position** — the one the other two miss. A seek keeps both the status
    ///   and the id and moves only the position, so a backward scrub inside the
    ///   fade window would otherwise fade out and skip the track the user just
    ///   scrubbed *into*. The monitor writes `position_ms` itself immediately
    ///   before deciding, so in this window the only other writers are
    ///   [`build_seek_actions`](Self::build_seek_actions), [`play_track_inner`]
    ///   and [`build_previous_actions`](Self::build_previous_actions) — exactly
    ///   the ops that must abort. Equality therefore also covers the *same* track
    ///   being restarted (which resets the position to 0).
    pub fn build_crossfade_actions(&mut self, decision: CrossfadeDecision) -> Vec<PlayerAction> {
        let mut actions = Vec::with_capacity(2);

        let Some(outgoing_id) = self.current_track().map(|t| t.id) else {
            return actions;
        };
        if self.status != PlaybackStatus::Playing
            || Some(outgoing_id) != decision.track_id
            || self.position_ms != decision.position_ms
        {
            return actions;
        }

        // Re-read the queue under the emit lock rather than trusting the
        // monitor's earlier `peek_next` — a skip could have landed in between.
        let Some(track) = self.queue.advance().cloned() else {
            return actions;
        };

        // The outgoing track counts as played the moment it starts fading. Same
        // accounting as `build_end_of_stream_actions`, just a few seconds early —
        // and only once `advance()` has confirmed somewhere to go.
        actions.push(PlayerAction::UpdatePlayCount(outgoing_id));

        // Same "the state now points at this track" step `play_track_inner`
        // takes — only the action it ends in differs. (Its `status = Playing` is
        // a no-op here; the guard above already proved it.)
        let start = begin_track(self, track, None);

        actions.push(PlayerAction::BeginCrossfade {
            file_path: start.file_path,
            replaygain: start.replaygain,
            fade_ms: decision.fade_ms,
            volume: start.volume,
            speed: start.speed,
        });
        actions
    }

    /// Build actions for set-playback-speed command.
    pub fn build_set_speed_actions(&mut self, speed: f64) -> Vec<PlayerAction> {
        let speed = speed.clamp(MIN_SPEED, MAX_SPEED);
        self.playback_speed = speed;
        vec![PlayerAction::SetSpeed(speed)]
    }
}

/// What the monitor saw when it decided a pause had held the device long enough, for
/// [`PlayerState::build_release_actions`] to check again under the emit lock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReleaseDecision {
    pub track_id: i64,
    /// The state's own position, which only a seek moves while paused.
    pub position_ms: u64,
    /// Where the deck stopped pulling, read before it is cleared: what a resume would have carried
    /// on from, everything before it having played out by now. The state's position is the last
    /// playing tick's and trails it, so play starts the track again from here instead.
    pub resume_ms: u64,
}

/// Everything a start action needs about the track the state now points at.
/// Produced by [`begin_track`], which is the single writer of the
/// "`current_track` + duration + position" trio.
struct TrackStart {
    file_path: String,
    replaygain: TrackReplayGain,
    volume: f64,
    speed: f64,
    /// The resume position, clamped and normalised — `None` means "from the top".
    start_position_ms: Option<u64>,
}

/// Point `state` at `track`: status Playing, duration and position from the
/// track, `current_track` replaced. Shared by [`play_track_inner`] (which turns
/// it into a `PlayMedia`) and [`PlayerState::build_crossfade_actions`] (a
/// `BeginCrossfade`), so the two can't drift on what "now playing this" means.
fn begin_track(
    state: &mut PlayerState,
    track: Arc<TrackSummary>,
    start_position_ms: Option<u64>,
) -> TrackStart {
    state.status = PlaybackStatus::Playing;
    state.duration_ms = u64::try_from(track.duration_ms.max(0)).unwrap_or(0);
    // Clamp to 500ms before end to avoid immediate EOS detection by the playback monitor.
    let max_resume_pos = state.duration_ms.saturating_sub(500);
    let clamped_pos = start_position_ms.map(|p| p.min(max_resume_pos)).filter(|&p| p > 0);
    state.position_ms = clamped_pos.unwrap_or(0);

    let start = TrackStart {
        file_path: track.file_path.clone(),
        replaygain: track.as_ref().into(),
        volume: state.effective_volume(),
        speed: state.playback_speed,
        start_position_ms: clamped_pos,
    };
    state.source = Some(track);
    start
}

/// Core playback logic — reused by commands, bus handler, position poller.
/// Returns `Vec<PlayerAction>` for the caller to execute after releasing the state lock.
/// `start_position_ms` — if `Some`, seeks to that position after starting playback (used for resume).
pub fn play_track_inner(
    state: &mut PlayerState,
    track: Arc<TrackSummary>,
    start_position_ms: Option<u64>,
) -> Vec<PlayerAction> {
    let start = begin_track(state, track, start_position_ms);

    // Gapless preload is staged late (by the playback monitor) when the
    // current track approaches its end — see `spawn_playback_monitor`. That
    // way mid-track repeat-mode / queue changes are reflected in what gets
    // preloaded, instead of being clobbered by a source already staged on the deck.
    vec![PlayerAction::PlayMedia {
        file_path: start.file_path,
        volume: start.volume,
        speed: start.speed,
        start_position_ms: start.start_position_ms,
        replaygain: start.replaygain,
    }]
}

/// Stop at the end of the queue. Never faded: the track has already run out of audio, so there is
/// nothing left to fade, and a deferred clear would only delay the silence.
pub fn stop_end_of_queue(state: &mut PlayerState) -> Vec<PlayerAction> {
    state.build_stop_actions(0)
}

/// Resume playback from a Stopped state. Replays the current track from the saved position.
/// Returns empty vec if status is not Stopped, or if neither the deck nor the queue holds a track.
///
pub fn resume_from_stopped(state: &mut PlayerState) -> Vec<PlayerAction> {
    if state.status != PlaybackStatus::Stopped {
        return vec![];
    }
    if let Some(actions) = replay_current_track(state) {
        return actions;
    }
    match state.queue.get_current().cloned() {
        Some(track) => play_track_inner(state, track, None),
        None => vec![],
    }
}

/// Start the track the state points at again from its position, or `None` where it points at
/// none. A stop and a paused track taken off the deck both come back this way.
fn replay_current_track(state: &mut PlayerState) -> Option<Vec<PlayerAction>> {
    let track = state.current_track().cloned()?;
    let resume_pos = (state.position_ms > 0).then_some(state.position_ms);
    Some(play_track_inner(state, track, resume_pos))
}
