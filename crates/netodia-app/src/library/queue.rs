use std::collections::HashMap;
use std::sync::Arc;

use crate::services::settings::{mutate_settings, read_settings};
use crate::state::{AppState, PlaybackContext};
use netodia_artwork::media::image::artwork::CoverCache;
use netodia_core::config::Paths;
use netodia_core::entities::track::TrackSummary;
use netodia_core::error::{AppError, AppResult};
use netodia_engine::player::engine::actions::emit_and_execute;
use netodia_engine::player::engine::event_sink::PlayerSinks;
use netodia_engine::player::engine::queue::current_index_to_i32;
use netodia_engine::player::engine::state::{
    PlayerAction, PlayerState, PlayerStateHandle, lock_state, play_track_inner, restore_queue,
    with_state_emit,
};
use netodia_engine::player::engine::types::{PersistableQueue, PersistedPlayback, RepeatMode};
use netodia_store::database::{DbPool, queries};

use super::import::{ImportFilesResult, import_and_summarize};
use super::playback::player_play_tracks;

/// Order a freshly-imported batch the way the rest of the app orders a list.
///
/// Files from outside — an OS drop's `text/uri-list`, a file manager's "Open
/// with" — arrive in visual selection order on KDE / GNOME, not alphabetical.
/// `natord` puts "Track 2" before "Track 10"; stable, so equal titles keep their
/// import order.
fn sort_for_queue(summaries: &mut [Arc<TrackSummary>]) {
    summaries.sort_by(|a, b| natord::compare(&a.title, &b.title));
}

pub async fn queue_import_files(
    state: &AppState,
    file_paths: Vec<String>,
) -> Result<ImportFilesResult, AppError> {
    append_imported(&state.playback_ctx(), &state.cover_cache, &file_paths).await
}

/// Import `file_paths` and append them to whatever is already queued.
async fn append_imported(
    ctx: &PlaybackContext,
    cover_cache: &CoverCache,
    file_paths: &[String],
) -> Result<ImportFilesResult, AppError> {
    let mut result =
        import_and_summarize(&ctx.db, &ctx.paths.artwork_dir, cover_cache, file_paths).await?;

    if !result.summaries.is_empty() {
        // Moved rather than cloned: the drag-drop caller discards the success
        // result, so an emptied `summaries` on it costs nothing.
        let mut summaries = std::mem::take(&mut result.summaries);
        sort_for_queue(&mut summaries);
        with_state_emit(&ctx.player_state, &ctx.sinks, |s| {
            s.queue.add_tracks(summaries);
        });
    }

    Ok(result)
}

/// The file-association entry point: import `file_paths`, make them the queue,
/// play from the top.
///
/// Replaces rather than appends — the queue you were listening to is not the
/// thing you just double-clicked. [`queue_import_files`] is the appending
/// sibling and the two differ in nothing else.
///
/// Ids come off the sorted summaries, not `ImportFilesResult::track_ids`, which
/// arrive partly out of a `HashMap` and would pick the first track by hash order.
pub async fn open_files(state: &AppState, file_paths: Vec<String>) -> AppResult<()> {
    open_as_queue(&state.playback_ctx(), &state.cover_cache, &file_paths).await?;

    // An opened file is usually new to the library, so every view re-fetches —
    // the same bump the drag-and-drop import does.
    state.library_changed.bump();
    Ok(())
}

/// Import `file_paths`, make them the queue, and play from the top.
async fn open_as_queue(
    ctx: &PlaybackContext,
    cover_cache: &CoverCache,
    file_paths: &[String],
) -> AppResult<()> {
    let mut result =
        import_and_summarize(&ctx.db, &ctx.paths.artwork_dir, cover_cache, file_paths).await?;

    if result.summaries.is_empty() {
        return Err(AppError::Queue(format!(
            "none of the {} opened file(s) could be read",
            file_paths.len()
        )));
    }

    sort_for_queue(&mut result.summaries);
    let track_ids: Vec<i64> = result.summaries.iter().map(|summary| summary.id).collect();

    log::debug!("queue: open {} track(s) from the command line", track_ids.len());
    player_play_tracks(ctx, track_ids, Some(0)).await
}

pub async fn queue_add_tracks(state: &AppState, track_ids: Vec<i64>) -> Result<(), AppError> {
    let summaries: Vec<Arc<TrackSummary>> =
        queries::track::get_track_summaries_by_ids(&state.db, &track_ids)
            .await?
            .into_iter()
            .map(Arc::new)
            .collect();

    log::debug!("queue: append {} track(s)", summaries.len());
    with_state_emit(&state.player_state, &state.sinks, |s| {
        s.queue.add_tracks(summaries);
    });
    Ok(())
}

/// The context menu's "Play Next": insert after `current_index` in input order,
/// so the queue reads `[current, id_0, …, id_n, …]`. One DB round-trip and one
/// `with_state_emit` for the batch.
pub async fn queue_play_next_many(state: &AppState, track_ids: Vec<i64>) -> Result<(), AppError> {
    play_next_many(&state.db, &state.player_state, &state.sinks, &track_ids).await
}

/// [`queue_play_next_many`]'s body, narrowed to what it reaches so the batch's ordering can be
/// driven off a bare pool.
async fn play_next_many(
    db: &DbPool,
    player_state: &PlayerStateHandle,
    sinks: &PlayerSinks,
    track_ids: &[i64],
) -> Result<(), AppError> {
    if track_ids.is_empty() {
        return Ok(());
    }
    let summaries: Vec<Arc<TrackSummary>> =
        queries::track::get_track_summaries_by_ids(db, track_ids)
            .await?
            .into_iter()
            .map(Arc::new)
            .collect();

    log::debug!("queue: play next, {} track(s)", summaries.len());
    with_state_emit(player_state, sinks, |s| {
        // `insert_next` always lands at `current_index + 1`, so walking the
        // input backwards is what produces forward order.
        for summary in summaries.into_iter().rev() {
            s.queue.insert_next(summary);
        }
    });
    Ok(())
}

pub fn queue_remove(state: &AppState, index: usize) -> Result<(), AppError> {
    log::debug!("queue: remove index {index}");
    with_state_emit(&state.player_state, &state.sinks, |s| {
        s.queue.remove_at(index);
    });
    Ok(())
}

pub fn queue_move(state: &AppState, from: usize, to: usize) -> Result<(), AppError> {
    // Once per drop, not per drag frame — Slint only emits on the drop gesture.
    log::debug!("queue: move {from} → {to}");
    with_state_emit(&state.player_state, &state.sinks, |s| {
        s.queue.move_track(from, to);
    });
    Ok(())
}

pub fn queue_remove_batch(state: &AppState, indices: &[usize]) -> Result<(), AppError> {
    log::debug!("queue: remove {} selected", indices.len());
    with_state_emit(&state.player_state, &state.sinks, |s| {
        s.queue.remove_batch(indices);
    });
    Ok(())
}

pub fn queue_clear(state: &AppState) -> Result<(), AppError> {
    // The one queue edit that can leave no other trace: with nothing playing it
    // emits no `PlayerAction`.
    log::debug!("queue: clear");
    with_state_emit(&state.player_state, &state.sinks, |s| {
        s.queue.clear();
    });
    Ok(())
}

pub fn queue_skip_to_index(state: &AppState, index: usize) -> Result<(), AppError> {
    // The `play` line this produces names the file; only this names why.
    log::debug!("queue: skip to index {index}");
    emit_and_execute(&*state.engine, &state.player_state, &state.sinks, |s| {
        let track = s.queue.skip_to_index(index).cloned();
        match track {
            Some(t) => play_track_inner(s, t, None),
            None => vec![],
        }
    });
    Ok(())
}

/// Drive shuffle to `enabled`, doing nothing when it is already there.
///
/// Comparison and flip share one `with_state_emit` closure, so a caller meaning
/// "on" can't be raced into "off" by the transport button landing between a read
/// and a write — the whole reason to reach for this over
/// read-then-`queue_toggle_shuffle`. The toggle is the transport's own path,
/// where flipping whatever is current *is* the intent.
pub fn queue_set_shuffle(state: &AppState, enabled: bool) -> Result<(), AppError> {
    let new_shuffle = set_shuffle(&state.player_state, &state.sinks, enabled);
    log::debug!("queue: shuffle {new_shuffle}");
    persist_shuffle(state);
    Ok(())
}

pub fn queue_toggle_shuffle(state: &AppState) -> Result<(), AppError> {
    let new_shuffle = toggle_shuffle(&state.player_state, &state.sinks);
    log::debug!("queue: shuffle {new_shuffle}");
    persist_shuffle(state);
    Ok(())
}

/// Cycle sequential → repeat all → repeat one → shuffle → sequential.
pub fn queue_cycle_playback_mode(state: &AppState) -> Result<(), AppError> {
    cycle_playback_mode(&state.player_state, &state.sinks);
    let player_state = Arc::clone(&state.player_state);
    state.persist_blocking("persist playback mode", move |paths| {
        write_playback_mode(paths, &player_state)
    });
    Ok(())
}

fn write_playback_mode(paths: &Paths, player_state: &PlayerStateHandle) -> AppResult<()> {
    mutate_settings(paths, |settings| {
        let player = lock_state(player_state);
        settings.queue.repeat_mode = player.queue.repeat_mode;
        settings.queue.shuffle_enabled = player.queue.shuffle_enabled;
    })
}

fn cycle_playback_mode(player_state: &PlayerStateHandle, sinks: &PlayerSinks) {
    with_state_emit(player_state, sinks, |s| {
        if s.queue.shuffle_enabled {
            s.queue.unshuffle();
            s.queue.set_repeat_mode(RepeatMode::Off);
        } else {
            match s.queue.repeat_mode {
                RepeatMode::Off => s.queue.set_repeat_mode(RepeatMode::All),
                RepeatMode::All => s.queue.set_repeat_mode(RepeatMode::One),
                RepeatMode::One => {
                    s.queue.set_repeat_mode(RepeatMode::Off);
                    shuffle_inline(s);
                    // Keep the preference even before tracks are added. Starting a new
                    // playlist already applies shuffle when this flag is enabled.
                    s.queue.shuffle_enabled = true;
                }
            }
        }
        log::debug!(
            "queue: playback mode → repeat {:?}, shuffle {}",
            s.queue.repeat_mode,
            s.queue.shuffle_enabled
        );
    });
}

pub fn queue_cycle_repeat(state: &AppState) -> Result<(), AppError> {
    let new_mode = cycle_repeat(&state.player_state, &state.sinks);
    log::debug!("queue: repeat → {new_mode:?}");
    persist_repeat(state);
    Ok(())
}

/// Drive repeat to `mode`, for a caller naming the mode rather than stepping through them.
pub fn queue_set_repeat(state: &AppState, mode: RepeatMode) -> Result<(), AppError> {
    let new_mode = set_repeat(&state.player_state, &state.sinks, mode);
    log::debug!("queue: repeat → {new_mode:?}");
    persist_repeat(state);
    Ok(())
}

/// Answers with the shuffle state it settled in, which is *not* the request: an empty queue
/// refuses to shuffle, so a caller persisting `enabled` writes a shuffle the next launch
/// restores over a queue that was never reordered.
fn set_shuffle(player_state: &PlayerStateHandle, sinks: &PlayerSinks, enabled: bool) -> bool {
    with_state_emit(player_state, sinks, |s| {
        if s.queue.shuffle_enabled == enabled {
            return s.queue.shuffle_enabled;
        }

        if enabled {
            shuffle_inline(s);
        } else {
            s.queue.unshuffle();
        }
        s.queue.shuffle_enabled
    })
}

/// Flip shuffle, answering with what it became.
fn toggle_shuffle(player_state: &PlayerStateHandle, sinks: &PlayerSinks) -> bool {
    with_state_emit(player_state, sinks, |s| {
        if s.queue.shuffle_enabled {
            s.queue.unshuffle();
        } else {
            shuffle_inline(s);
        }
        s.queue.shuffle_enabled
    })
}

/// Advance the repeat mode, answering with the one it landed on rather than the one it left.
fn cycle_repeat(player_state: &PlayerStateHandle, sinks: &PlayerSinks) -> RepeatMode {
    with_state_emit(player_state, sinks, |s| {
        s.queue.cycle_repeat_mode();
        s.queue.repeat_mode
    })
}

/// Set the repeat mode, answering with the one it landed on.
fn set_repeat(
    player_state: &PlayerStateHandle,
    sinks: &PlayerSinks,
    mode: RepeatMode,
) -> RepeatMode {
    with_state_emit(player_state, sinks, |s| {
        s.queue.set_repeat_mode(mode);
        s.queue.repeat_mode
    })
}

/// Fire-and-forget settings.json write for shuffle, on the blocking pool so the
/// `wire_sync!` spawn returns immediately. A failed write only logs: the
/// in-memory state already changed and persistence is best-effort.
fn persist_shuffle(state: &AppState) {
    let player_state = Arc::clone(&state.player_state);
    state.persist_blocking("persist shuffle_enabled", move |paths| {
        mutate_settings(paths, |s| {
            s.queue.shuffle_enabled = lock_state(&player_state).queue.shuffle_enabled;
        })
    });
}

fn persist_repeat(state: &AppState) {
    let player_state = Arc::clone(&state.player_state);
    state.persist_blocking("persist repeat_mode", move |paths| {
        mutate_settings(paths, |s| {
            s.queue.repeat_mode = lock_state(&player_state).queue.repeat_mode;
        })
    });
}

/// Read `queue.json`. Best-effort: a missing or unparseable file restores nothing.
fn load_persisted_playback(paths: &Paths) -> Option<PersistedPlayback> {
    if !paths.queue_path.exists() {
        return None;
    }
    let json = std::fs::read_to_string(&paths.queue_path).ok()?;
    serde_json::from_str(&json).ok()
}

/// Startup restore: what the last session was playing back into `PlayerState`, once, from
/// `boot::tasks`.
///
/// Gather then apply, because the two halves are testable at different costs: [`plan_restore`]
/// needs a pool and a data directory, and the apply restores the queue state.
pub async fn restore_persisted_playback(state: &AppState) -> AppResult<()> {
    let persisted = load_persisted_playback(&state.paths);
    let plan = plan_restore(&state.db, &state.paths, persisted).await?;
    emit_and_execute(&*state.engine, &state.player_state, &state.sinks, |s| apply_restore(s, plan));

    Ok(())
}

/// Everything a restart puts back, resolved before anything touches `PlayerState`.
struct RestorePlan {
    persisted: Option<PersistedPlayback>,
    /// The rows that still exist. Shorter than `persisted.queue.track_ids` when a folder went
    /// away between sessions, which is the ordinary case rather than the corrupt one.
    summaries: Vec<Arc<TrackSummary>>,
    repeat_mode: RepeatMode,
    shuffle_enabled: bool,
}

/// Resolve the cached playlist against the library and settle shuffle and repeat against
/// `settings.json`. Local tracks refresh from `SQLite` so moved files are honored; cloud entries
/// come directly from the disk snapshot because they have no local database row.
///
/// Playback preferences come from `settings.json`. The saved queue order becomes the
/// baseline for unshuffle because the earlier insertion order is not stored.
async fn plan_restore(
    db: &DbPool,
    paths: &Arc<Paths>,
    persisted: Option<PersistedPlayback>,
) -> AppResult<RestorePlan> {
    let (persisted, summaries) = resolve_persisted_playlist(db, persisted).await?;

    let paths = Arc::clone(paths);
    let (repeat_mode, shuffle_enabled) = tokio::task::spawn_blocking(move || -> AppResult<_> {
        let settings = read_settings(&paths)?;
        Ok((settings.queue.repeat_mode, settings.queue.shuffle_enabled))
    })
    .await
    .map_err(|e| AppError::io("restore_persisted_playback join", e))??;

    Ok(RestorePlan { persisted, summaries, repeat_mode, shuffle_enabled })
}

/// Resolve a saved queue while preserving its exact order and current row. Older `queue.json`
/// files have only local database ids, so they keep using the original restore path.
async fn resolve_persisted_playlist(
    db: &DbPool,
    persisted: Option<PersistedPlayback>,
) -> AppResult<(Option<PersistedPlayback>, Vec<Arc<TrackSummary>>)> {
    let Some(mut persisted) = persisted else {
        return Ok((None, Vec::new()));
    };
    if persisted.playlist_tracks.is_empty() {
        let summaries = queries::track::get_track_summaries_by_ids(db, &persisted.queue.track_ids)
            .await?
            .into_iter()
            .map(Arc::new)
            .collect();
        return Ok((Some(persisted), summaries));
    }

    let local_ids: Vec<i64> = persisted
        .playlist_tracks
        .iter()
        .filter(|track| !track.file_path.is_empty())
        .map(|track| track.id)
        .collect();
    let local_by_id: HashMap<i64, Arc<TrackSummary>> =
        queries::track::get_track_summaries_by_ids(db, &local_ids)
            .await?
            .into_iter()
            .map(|track| (track.id, Arc::new(track)))
            .collect();

    let mut summaries = Vec::with_capacity(persisted.playlist_tracks.len());
    let mut old_to_new = Vec::with_capacity(persisted.playlist_tracks.len());
    for track in &persisted.playlist_tracks {
        let resolved = if track.file_path.is_empty() {
            Some(Arc::new(track.clone()))
        } else {
            local_by_id.get(&track.id).cloned()
        };
        old_to_new.push(resolved.as_ref().map(|_| summaries.len()));
        if let Some(track) = resolved {
            summaries.push(track);
        }
    }

    let old_current = usize::try_from(persisted.playlist_current_index).ok();
    let selected_current = old_current.and_then(|index| old_to_new.get(index).copied().flatten());
    let remapped_current = selected_current
        .or_else(|| {
            old_current.and_then(|index| {
                old_to_new.iter().skip(index.saturating_add(1)).find_map(|index| *index)
            })
        })
        .or_else(|| old_current.and_then(|_| summaries.len().checked_sub(1)));

    if let (Some(index), Some(position_ms)) = (selected_current, persisted.current_position_ms) {
        let mut current = (*summaries[index]).clone();
        current.last_position = i64::try_from(position_ms).unwrap_or(i64::MAX);
        summaries[index] = Arc::new(current);
    }

    let track_ids: Vec<i64> = summaries.iter().map(|track| track.id).collect();
    let current_index = current_index_to_i32(remapped_current);
    persisted.queue = PersistableQueue { track_ids, current_index };
    persisted.playlist_current_index = current_index;
    Ok((Some(persisted), summaries))
}

/// Seat a [`RestorePlan`].
fn apply_restore(state: &mut PlayerState, plan: RestorePlan) -> Vec<PlayerAction> {
    let RestorePlan { persisted, summaries, repeat_mode, shuffle_enabled } = plan;

    if let Some(p) = persisted {
        restore_queue(state, summaries, &p.queue);
    }
    state.queue.repeat_mode = repeat_mode;
    state.queue.shuffle_enabled = shuffle_enabled;

    Vec::new()
}

/// Shuffle the queue's `play_order` in place using a thread-local RNG,
/// pinning the currently-playing track to position 0 so playback stays
/// continuous. Allocation-free — no intermediate index Vec.
fn shuffle_inline(state: &mut PlayerState) {
    let mut rng = rand::rng();
    state.queue.shuffle_play_order_in_place(&mut rng, /* anchor_to_current */ true);
}

#[cfg(test)]
#[path = "tests/queue_tests.rs"]
mod tests;
