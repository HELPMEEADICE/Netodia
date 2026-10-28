//! Account-scoped roaming with bounded history and refill before the queue runs out.

use std::collections::HashSet;
use std::time::Duration;

use netodia_core::entities::cloud::RoamingMode;
use netodia_core::error::AppResult;
use netodia_engine::player::engine::queue::QueueState;
use netodia_engine::player::engine::state::{lock_state, with_state_emit};
use netodia_engine::player::engine::types::{PlaybackStatus, RepeatMode};
use tokio_util::sync::CancellationToken;

use crate::state::AppState;

const HISTORY_LIMIT: usize = 20;
const REFILL_REMAINING: usize = 2;

pub struct RoamingRequest {
    pub mode: RoamingMode,
    pub seed: Option<i64>,
    pub cancellation: CancellationToken,
    pub queue_generation: u64,
    pub playback_generation: u64,
}

/// Stops on account changes, shutdown, an explicit stop, or another playlist taking over.
pub async fn run(
    state: &AppState,
    request: RoamingRequest,
    on_started: impl FnOnce(),
) -> AppResult<()> {
    let account = state.cloud.account_tasks_token();
    tokio::select! {
        () = request.cancellation.cancelled() => Ok(()),
        () = account.cancelled() => Ok(()),
        () = state.shutdown_token.cancelled() => Ok(()),
        result = run_inner(state, &request, on_started) => result,
    }
}

async fn run_inner(
    state: &AppState,
    request: &RoamingRequest,
    on_started: impl FnOnce(),
) -> AppResult<()> {
    if state.cloud_playback.queue_generation() != request.queue_generation
        || !state.engine.is_playback_request_current(request.playback_generation)
    {
        return Ok(());
    }
    let songs = state.cloud.roaming_songs(request.mode, request.seed).await?;
    if state.cloud_playback.queue_generation() != request.queue_generation
        || !state.engine.is_playback_request_current(request.playback_generation)
    {
        return Ok(());
    }
    let result = state
        .cloud_playback
        .play_roaming_queue(songs, request.queue_generation, request.playback_generation)
        .await?;
    if !result.started {
        return Ok(());
    }
    let generation = request.queue_generation;
    if state.cloud_playback.queue_generation() != generation {
        return Ok(());
    }
    with_state_emit(&state.player_state, &state.sinks, |player| {
        player.queue.unshuffle();
        player.queue.set_repeat_mode(RepeatMode::Off);
    });
    on_started();

    let mut poll = tokio::time::interval(Duration::from_secs(2));
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut last_empty_track = None;
    loop {
        poll.tick().await;
        let (seed, current_id) = {
            let player = lock_state(&state.player_state);
            if state.cloud_playback.queue_generation() != generation
                || (player.status == PlaybackStatus::Stopped
                    && !state.cloud_playback.is_queue_track_loading())
                || player.source.as_ref().is_none_or(|track| !track.file_path.is_empty())
            {
                return Ok(());
            }
            let Some(index) = player.queue.current_index else { return Ok(()) };
            if player.queue.len().saturating_sub(index + 1) > REFILL_REMAINING {
                continue;
            }
            let current_id = player.source.as_ref().map(|track| track.id);
            if last_empty_track.is_some() && last_empty_track == current_id {
                continue;
            }
            (player.queue.tracks_in_play_order().last().map(|track| track.id), current_id)
        };
        let songs = state.cloud.roaming_songs(request.mode, seed).await?;
        if state.cloud_playback.queue_generation() != generation {
            return Ok(());
        }
        let mut seen: HashSet<i64> = {
            let player = lock_state(&state.player_state);
            player
                .queue
                .tracks_in_play_order()
                .iter()
                .skip(player.queue.current_index.unwrap_or(0))
                .map(|track| track.id)
                .collect()
        };
        let ids: Vec<i64> =
            songs.into_iter().filter_map(|song| seen.insert(song.id).then_some(song.id)).collect();
        if ids.is_empty() {
            // A short FM batch can be unchanged until the current song advances.
            last_empty_track = current_id;
            continue;
        }
        last_empty_track = None;
        state.cloud_playback.enqueue_roaming(&ids, generation).await?;
        with_state_emit(&state.player_state, &state.sinks, |player| {
            if state.cloud_playback.queue_generation() != generation {
                return;
            }
            trim_history(&mut player.queue);
        });
    }
}

fn trim_history(queue: &mut QueueState) {
    let Some(current_index) = queue.current_index else {
        return;
    };
    let keep_from = current_index.saturating_sub(HISTORY_LIMIT);
    if keep_from == 0 {
        return;
    }
    // Rebuild by position: the same song can legitimately appear again later in the queue.
    let retained = queue.tracks_in_play_order().into_iter().skip(keep_from).collect();
    queue.clear();
    queue.add_tracks(retained);
    let _ = queue.skip_to_index(current_index - keep_from);
}

#[cfg(test)]
#[path = "tests/cloud_roaming_tests.rs"]
mod tests;
