use std::collections::HashSet;
use std::sync::Arc;

use super::*;
use netodia_core::entities::track::TrackSummary;
use netodia_core::error::AppError;

fn make_summary(id: i64, title: &str, duration_ms: i64) -> Arc<TrackSummary> {
    Arc::new(TrackSummary {
        id,
        file_path: format!("/music/{id}.mp3"),
        file_name: format!("{id}.mp3"),
        title: title.to_owned(),
        artist: None,
        album: None,
        duration_ms,
        artwork_path: None,
        track_number: None,
        disc_number: None,
        last_position: 0,
        is_favorite: false,
        rating: 0,
        replaygain_track_gain: None,
        replaygain_track_peak: None,
        replaygain_album_gain: None,
        replaygain_album_peak: None,
    })
}

fn make_cloud_summary(id: i64, title: &str) -> Arc<TrackSummary> {
    Arc::new(TrackSummary {
        id,
        file_path: String::new(),
        file_name: format!("{id}.mp3"),
        title: title.to_owned(),
        artist: None,
        album: None,
        duration_ms: 180_000,
        artwork_path: None,
        track_number: None,
        disc_number: None,
        last_position: 0,
        is_favorite: false,
        rating: 0,
        replaygain_track_gain: None,
        replaygain_track_peak: None,
        replaygain_album_gain: None,
        replaygain_album_peak: None,
    })
}

fn make_queue(count: i64) -> QueueState {
    let mut q = QueueState::default();
    let tracks: Vec<Arc<TrackSummary>> =
        (1..=count).map(|i| make_summary(i, &format!("Track {i}"), 180_000)).collect();
    q.add_tracks(tracks);
    q
}

// ── add_tracks ──────────────────────────────────────────────────────

#[test]
fn add_tracks_to_empty_queue() {
    let mut q = QueueState::default();
    let tracks = vec![make_summary(1, "A", 100), make_summary(2, "B", 200)];
    q.add_tracks(tracks);

    assert_eq!(q.tracks.len(), 2);
    assert_eq!(q.play_order, vec![0, 1]);
    assert_eq!(q.original_order, vec![0, 1]);
}

#[test]
fn add_tracks_appends_to_existing() {
    let mut q = make_queue(2);
    let more = vec![make_summary(3, "C", 300)];
    q.add_tracks(more);

    assert_eq!(q.tracks.len(), 3);
    assert_eq!(q.play_order, vec![0, 1, 2]);
    assert_eq!(q.original_order, vec![0, 1, 2]);
}

#[test]
fn add_tracks_increments_version() {
    let mut q = QueueState::default();
    let v0 = q.version;
    q.add_tracks(vec![make_summary(1, "A", 100)]);
    assert!(q.version > v0);
}

// ── insert_next ─────────────────────────────────────────────────────

#[test]
fn insert_next_after_current() {
    let mut q = make_queue(3);
    q.current_index = Some(1);
    q.insert_next(make_summary(10, "Inserted", 100));

    // Should insert at position 2 in play_order
    assert_eq!(q.play_order[2], 3); // new track index
    assert_eq!(q.tracks[3].title, "Inserted");
}

#[test]
fn insert_next_when_no_current() {
    let mut q = make_queue(2);
    q.current_index = None;
    q.insert_next(make_summary(10, "Inserted", 100));

    // Should insert at position 0
    assert_eq!(q.play_order[0], 2); // new track index
}

#[test]
fn insert_next_clamps_to_end() -> Result<(), AppError> {
    let mut q = make_queue(2);
    q.current_index = Some(10); // way past end
    q.insert_next(make_summary(10, "Inserted", 100));

    // Should clamp insert position to play_order.len()
    let last =
        q.play_order.last().ok_or_else(|| AppError::Validation("play_order empty".into()))?;
    assert_eq!(*last, 2);
    Ok(())
}

// ── remove_at ───────────────────────────────────────────────────────

#[test]
fn remove_at_out_of_bounds() {
    let mut q = make_queue(2);
    assert!(!q.remove_at(5));
}

#[test]
fn remove_at_adjusts_current_when_before() {
    let mut q = make_queue(3);
    q.current_index = Some(2);
    let removed = Arc::downgrade(&q.tracks[0]);
    q.remove_at(0);
    assert_eq!(q.current_index, Some(1));
    assert_eq!(q.get_current().map(|track| track.id), Some(3));
    assert!(removed.upgrade().is_none());
}

#[test]
fn remove_at_no_change_when_after() {
    let mut q = make_queue(3);
    q.current_index = Some(0);
    q.remove_at(2);
    assert_eq!(q.current_index, Some(0));
}

#[test]
fn remove_at_current_clamps_to_len() {
    let mut q = make_queue(2);
    q.current_index = Some(1); // last item
    q.remove_at(1);
    // Queue has 1 item left, current_index should clamp to 0
    assert_eq!(q.current_index, Some(0));
}

#[test]
fn remove_at_last_item_sets_minus_one() {
    let mut q = make_queue(1);
    q.current_index = Some(0);
    q.remove_at(0);
    assert_eq!(q.current_index, None);
    assert!(q.play_order.is_empty());
}

#[test]
fn remove_at_increments_version() {
    let mut q = make_queue(2);
    let v0 = q.version;
    q.remove_at(0);
    assert!(q.version > v0);
}

// ── remove_batch ────────────────────────────────────────────────────

#[test]
fn remove_batch_multiple() {
    let mut q = make_queue(4);
    q.add_tracks(vec![make_summary(1, "Duplicate", 180_000)]);
    q.apply_shuffle_order(&[3, 0, 2, 1, 4]);
    q.current_index = Some(3);
    let removed = [Arc::downgrade(&q.tracks[0]), Arc::downgrade(&q.tracks[1])];

    q.remove_batch(&[1, 3, 99]);

    assert!(removed.iter().all(|track| track.upgrade().is_none()));
    assert_eq!(q.to_persistable().track_ids, [4, 3, 1]);
    assert_eq!(q.get_current().map(|track| track.title.as_str()), Some("Duplicate"));
    q.unshuffle();
    assert_eq!(q.to_persistable().track_ids, [3, 4, 1]);
}

#[test]
fn remove_batch_deduplicates() {
    let mut q = make_queue(3);
    q.remove_batch(&[1, 1, 1]);
    assert_eq!(q.play_order.len(), 2);
}

#[test]
fn remove_batch_empty_is_noop() {
    let mut q = make_queue(3);
    let v0 = q.version;
    q.remove_batch(&[]);
    assert_eq!(q.version, v0);
    assert_eq!(q.play_order.len(), 3);
}

// ── move_track ──────────────────────────────────────────────────────

#[test]
fn move_track_out_of_bounds() {
    let mut q = make_queue(3);
    assert!(!q.move_track(0, 10));
    assert!(!q.move_track(10, 0));
}

#[test]
fn move_track_forward_adjusts_current() {
    let mut q = make_queue(4);
    q.current_index = Some(1);
    // Move track at 0 to position 2 (moving past current)
    q.move_track(0, 2);
    // from < current && to >= current → current -= 1
    assert_eq!(q.current_index, Some(0));
}

#[test]
fn move_track_backward_adjusts_current() {
    let mut q = make_queue(4);
    q.current_index = Some(1);
    // Move track at 3 to position 0 (moving before current)
    q.move_track(3, 0);
    // from > current && to <= current → current += 1
    assert_eq!(q.current_index, Some(2));
}

#[test]
fn move_current_track() {
    let mut q = make_queue(4);
    q.current_index = Some(1);
    q.move_track(1, 3);
    assert_eq!(q.current_index, Some(3));
}

// ── clear ───────────────────────────────────────────────────────────

#[test]
fn clear_resets_all() {
    let mut q = make_queue(3);
    q.current_index = Some(1);
    q.clear();

    assert!(q.tracks.is_empty());
    assert!(q.play_order.is_empty());
    assert!(q.original_order.is_empty());
    assert_eq!(q.current_index, None);
}

// ── skip_to_index ───────────────────────────────────────────────────

#[test]
fn skip_to_index_valid() {
    let mut q = make_queue(3);
    let track = q.skip_to_index(2);
    assert!(track.is_some());
    assert_eq!(q.current_index, Some(2));
}

#[test]
fn skip_to_index_out_of_bounds() {
    let mut q = make_queue(3);
    assert!(q.skip_to_index(10).is_none());
}

// ── advance ─────────────────────────────────────────────────────────

#[test]
fn advance_off_stops_at_end() {
    let mut q = make_queue(2);
    q.current_index = Some(1);
    q.repeat_mode = RepeatMode::Off;
    assert!(q.advance().is_none());
}

#[test]
fn advance_all_wraps() {
    let mut q = make_queue(3);
    q.current_index = Some(2);
    q.repeat_mode = RepeatMode::All;
    let track = q.advance();
    assert!(track.is_some());
    assert_eq!(q.current_index, Some(0));
}

#[test]
fn advance_one_repeats_current() {
    let mut q = make_queue(3);
    q.current_index = Some(1);
    q.repeat_mode = RepeatMode::One;
    let track = q.advance();
    assert!(track.is_some());
    assert_eq!(q.current_index, Some(1));
}

#[test]
fn advance_empty_queue() {
    let mut q = QueueState::default();
    assert!(q.advance().is_none());
}

// ── advance_skip ────────────────────────────────────────────────────

#[test]
fn advance_skip_one_advances_within_queue() {
    let mut q = make_queue(3);
    q.current_index = Some(1);
    q.repeat_mode = RepeatMode::One;
    let track = q.advance_skip();
    assert!(track.is_some());
    assert_eq!(q.current_index, Some(2));
}

#[test]
fn advance_skip_one_wraps_at_end() {
    let mut q = make_queue(3);
    q.current_index = Some(2);
    q.repeat_mode = RepeatMode::One;
    let track = q.advance_skip();
    assert!(track.is_some());
    assert_eq!(q.current_index, Some(0));
}

#[test]
fn advance_skip_all_wraps() {
    let mut q = make_queue(3);
    q.current_index = Some(2);
    q.repeat_mode = RepeatMode::All;
    let track = q.advance_skip();
    assert!(track.is_some());
    assert_eq!(q.current_index, Some(0));
}

#[test]
fn advance_skip_off_stops_at_end() {
    let mut q = make_queue(2);
    q.current_index = Some(1);
    q.repeat_mode = RepeatMode::Off;
    assert!(q.advance_skip().is_none());
}

#[test]
fn failure_advance_leaves_repeat_one_for_the_next_candidate() {
    let mut q = make_queue(3);
    q.repeat_mode = RepeatMode::One;
    q.current_index = Some(0);

    assert_eq!(q.advance_after_failure().map(|track| track.id), Some(2));
    assert_eq!(q.advance_after_failure().map(|track| track.id), Some(3));
    assert_eq!(q.advance_after_failure().map(|track| track.id), Some(1));
}

// ── set_repeat_mode ─────────────────────────────────────────────────

/// A panel re-sends the mode it already shows, and a bump there re-publishes the whole queue to
/// every subscriber for nothing.
#[test]
fn setting_the_repeat_mode_already_in_force_leaves_the_version_alone() {
    let mut q = make_queue(3);
    q.repeat_mode = RepeatMode::All;
    let before = q.version;

    q.set_repeat_mode(RepeatMode::All);

    assert_eq!(q.version, before);
}

/// `with_state_emit` gates the queue re-emit on the version, so a move that skipped the bump would
/// leave the transport bar's repeat button showing the mode it just left.
#[test]
fn a_new_repeat_mode_bumps_the_version() {
    let mut q = make_queue(3);
    let before = q.version;

    q.set_repeat_mode(RepeatMode::One);

    assert_ne!(q.version, before);
}

// ── previous ────────────────────────────────────────────────────────

#[test]
fn previous_goes_back() {
    let mut q = make_queue(3);
    q.current_index = Some(2);
    let track = q.previous();
    assert!(track.is_some());
    assert_eq!(q.current_index, Some(1));
}

#[test]
fn previous_all_wraps_to_last() {
    let mut q = make_queue(3);
    q.current_index = Some(0);
    q.repeat_mode = RepeatMode::All;
    let track = q.previous();
    assert!(track.is_some());
    assert_eq!(q.current_index, Some(2));
}

#[test]
fn previous_one_wraps_to_last() {
    let mut q = make_queue(3);
    q.current_index = Some(0);
    q.repeat_mode = RepeatMode::One;
    let track = q.previous();
    assert!(track.is_some());
    assert_eq!(q.current_index, Some(2));
}

#[test]
fn previous_off_stays_at_zero() {
    let mut q = make_queue(3);
    q.current_index = Some(0);
    q.repeat_mode = RepeatMode::Off;
    let track = q.previous();
    assert!(track.is_some());
    assert_eq!(q.current_index, Some(0));
}

// ── peek_next ───────────────────────────────────────────────────────

#[test]
fn peek_next_does_not_mutate() {
    let mut q = make_queue(3);
    q.current_index = Some(0);
    let version_before = q.version;
    let _ = q.peek_next();
    assert_eq!(q.current_index, Some(0));
    assert_eq!(q.version, version_before);
}

#[test]
fn peek_next_off_at_end_returns_none() {
    let mut q = make_queue(2);
    q.current_index = Some(1);
    q.repeat_mode = RepeatMode::Off;
    assert!(q.peek_next().is_none());
}

#[test]
fn peek_next_all_at_end_returns_first() -> Result<(), AppError> {
    let mut q = make_queue(3);
    q.current_index = Some(2);
    q.repeat_mode = RepeatMode::All;
    let track = q.peek_next().ok_or_else(|| AppError::Validation("peek_next None".into()))?;
    assert_eq!(track.id, 1);
    Ok(())
}

#[test]
fn peek_next_one_returns_current() -> Result<(), AppError> {
    let mut q = make_queue(3);
    q.current_index = Some(1);
    q.repeat_mode = RepeatMode::One;
    let track = q.peek_next().ok_or_else(|| AppError::Validation("peek_next None".into()))?;
    assert_eq!(track.id, 2); // tracks[play_order[1]] = tracks[1], id=2
    Ok(())
}

// ── shuffle ─────────────────────────────────────────────────────────

#[test]
fn begin_shuffle_returns_len_and_current() {
    let mut q = make_queue(5);
    q.current_index = Some(2);
    let (len, current) = q.begin_shuffle();
    assert_eq!(len, 5);
    assert_eq!(current, Some(2));
}

#[test]
fn begin_shuffle_no_current() {
    let q = make_queue(3);
    // current_index is None by default after add_tracks (we didn't set it).
    let (len, current) = q.begin_shuffle();
    assert_eq!(len, 3);
    assert_eq!(current, None);
}

#[test]
fn apply_shuffle_order_reorders() {
    let mut q = make_queue(3);
    q.current_index = Some(1);
    // Shuffle: put current (1) first, then 2, then 0
    q.apply_shuffle_order(&[1, 2, 0]);

    assert_eq!(q.play_order, vec![1, 2, 0]);
    assert_eq!(q.current_index, Some(0));
    assert!(q.shuffle_enabled);
}

#[test]
fn unshuffle_restores_original_order() {
    let mut q = make_queue(3);
    q.current_index = Some(1);
    // Shuffle
    q.apply_shuffle_order(&[1, 2, 0]);
    // Current track (id=2) is now at play_order index 0
    q.unshuffle();

    assert_eq!(q.play_order, vec![0, 1, 2]);
    assert!(!q.shuffle_enabled);
    // Current track (id=2) should be at original position 1
    assert_eq!(q.current_index, Some(1));
}

// ── cycle_repeat_mode ───────────────────────────────────────────────

#[test]
fn cycle_repeat_mode_sequence() {
    let mut q = QueueState::default();
    assert_eq!(q.repeat_mode, RepeatMode::Off);
    q.cycle_repeat_mode();
    assert_eq!(q.repeat_mode, RepeatMode::All);
    q.cycle_repeat_mode();
    assert_eq!(q.repeat_mode, RepeatMode::One);
    q.cycle_repeat_mode();
    assert_eq!(q.repeat_mode, RepeatMode::Off);
}

// ── to_persistable ──────────────────────────────────────────────────

#[test]
fn to_persistable_maps_track_ids() {
    let mut q = make_queue(3);
    q.current_index = Some(1);

    let p = q.to_persistable();
    assert_eq!(p.track_ids, vec![1, 2, 3]);
    assert_eq!(p.current_index, 1);
}

// ── tracks_in_play_order ────────────────────────────────────────────

#[test]
fn tracks_in_play_order_returns_correct_sequence() {
    let mut q = make_queue(3);
    // Reverse the play order
    q.play_order = vec![2, 0, 1];
    let tracks = q.tracks_in_play_order();
    assert_eq!(tracks.len(), 3);
    assert_eq!(tracks[0].id, 3);
    assert_eq!(tracks[1].id, 1);
    assert_eq!(tracks[2].id, 2);
}

// ── get_current ─────────────────────────────────────────────────────

#[test]
fn get_current_returns_none_when_empty() {
    let q = QueueState::default();
    assert!(q.get_current().is_none());
}

#[test]
fn get_current_returns_none_for_negative_index() {
    let q = make_queue(3);
    // current_index is None by default
    assert!(q.get_current().is_none());
}

// ── prune_missing ───────────────────────────────────────────────────

#[test]
fn prune_missing_empty_set_is_noop() {
    let mut q = make_queue(3);
    q.current_index = Some(1);
    let v0 = q.version;
    let outcome = q.prune_missing(&HashSet::new());

    assert_eq!(outcome.removed, 0);
    assert!(!outcome.current_was_removed);
    assert_eq!(q.tracks.len(), 3);
    assert_eq!(q.current_index, Some(1));
    assert_eq!(q.version, v0);
}

#[test]
fn prune_missing_no_matches_is_noop() {
    let mut q = make_queue(3);
    let v0 = q.version;
    let outcome = q.prune_missing(&HashSet::from([99, 100]));

    assert_eq!(outcome.removed, 0);
    assert_eq!(q.tracks.len(), 3);
    assert_eq!(q.version, v0);
}

#[test]
fn pruning_a_local_row_does_not_remove_a_cloud_track_with_the_same_id() {
    let mut q = QueueState::default();
    q.add_tracks(vec![make_summary(7, "Local", 100), make_cloud_summary(7, "Cloud")]);
    q.current_index = Some(1);

    let outcome = q.prune_missing_local(&HashSet::from([7]));

    assert_eq!(outcome.removed, 1);
    assert!(!outcome.current_was_removed);
    assert_eq!(q.tracks.len(), 1);
    assert!(q.tracks[0].file_path.is_empty());
    assert_eq!(q.get_current().map(|track| track.title.as_str()), Some("Cloud"));
}

#[test]
fn prune_missing_keeps_current_when_other_removed() -> Result<(), AppError> {
    // tracks 1..=5, current = play_order[2] = track id 3. Prune ids {1, 5}.
    // Expected: surviving tracks [2, 3, 4], current_index remapped to 1
    // (still pointing at id 3), `current_was_removed = false`.
    let mut q = make_queue(5);
    q.current_index = Some(2);

    let outcome = q.prune_missing(&HashSet::from([1, 5]));

    assert_eq!(outcome.removed, 2);
    assert!(!outcome.current_was_removed);
    assert_eq!(q.tracks.iter().map(|t| t.id).collect::<Vec<_>>(), vec![2, 3, 4]);
    assert_eq!(q.play_order, vec![0, 1, 2]);
    assert_eq!(q.original_order, vec![0, 1, 2]);
    let current = q
        .get_current()
        .ok_or_else(|| AppError::Validation("get_current None after prune".into()))?;
    assert_eq!(current.id, 3);
    Ok(())
}

#[test]
fn prune_missing_advances_when_current_removed() -> Result<(), AppError> {
    // tracks 1..=3, current = play_order[1] = id 2. Prune {2}.
    // Expected: surviving tracks [1, 3], current_index = 1 (next survivor,
    // which is id 3), `current_was_removed = true`.
    let mut q = make_queue(3);
    q.current_index = Some(1);

    let outcome = q.prune_missing(&HashSet::from([2]));

    assert!(outcome.current_was_removed);
    assert_eq!(outcome.removed, 1);
    assert_eq!(q.tracks.iter().map(|t| t.id).collect::<Vec<_>>(), vec![1, 3]);
    let current = q
        .get_current()
        .ok_or_else(|| AppError::Validation("get_current None after prune".into()))?;
    assert_eq!(current.id, 3);
    Ok(())
}

#[test]
fn prune_missing_clamps_to_last_survivor_when_tail_removed() -> Result<(), AppError> {
    // tracks 1..=4, current = 3 (id 4). Prune {3, 4}.
    // Expected: surviving [1, 2], no surviving slot at or after old current,
    // so we clamp to the last survivor (index 1, id 2). current_was_removed
    // is true.
    let mut q = make_queue(4);
    q.current_index = Some(3);

    let outcome = q.prune_missing(&HashSet::from([3, 4]));

    assert!(outcome.current_was_removed);
    assert_eq!(q.current_index, Some(1));
    let current = q
        .get_current()
        .ok_or_else(|| AppError::Validation("get_current None after prune".into()))?;
    assert_eq!(current.id, 2);
    Ok(())
}

#[test]
fn prune_missing_everything_empties_queue() {
    let mut q = make_queue(3);
    q.current_index = Some(0);

    let outcome = q.prune_missing(&HashSet::from([1, 2, 3]));

    assert_eq!(outcome.removed, 3);
    assert!(outcome.current_was_removed);
    assert!(q.tracks.is_empty());
    assert!(q.play_order.is_empty());
    assert!(q.original_order.is_empty());
    assert_eq!(q.current_index, None);
}

#[test]
fn prune_missing_remaps_shuffled_order() {
    // tracks 1..=4 with a shuffled play_order. Pruning id 1 must drop the
    // play_order entry that pointed at it and remap every remaining entry
    // to the new tracks-vec position.
    let mut q = make_queue(4);
    // Hand-craft a shuffle: [t3, t1, t4, t2] in play order.
    q.play_order = vec![2, 0, 3, 1];
    q.shuffle_enabled = true;
    q.current_index = Some(0); // playing t3

    let outcome = q.prune_missing(&HashSet::from([1]));

    assert_eq!(outcome.removed, 1);
    assert!(!outcome.current_was_removed);
    // tracks after prune: [t2, t3, t4] → t3 at new idx 1, t4 at new idx 2, t2 at new idx 0.
    // Old play_order [2,0,3,1] -> drop entry 1 (was t1), remap remaining:
    //   2→1 (t3), 3→2 (t4), 1→0 (t2). New: [1, 2, 0].
    assert_eq!(q.play_order, vec![1, 2, 0]);
    // original_order had [0,1,2,3] → drop t1 (idx 0), remap survivors:
    //   1→0 (t2), 2→1 (t3), 3→2 (t4). New: [0, 1, 2].
    assert_eq!(q.original_order, vec![0, 1, 2]);
}

// ── get_current ─────────────────────────────────────────────────────

#[test]
fn get_current_returns_track() -> Result<(), AppError> {
    let mut q = make_queue(3);
    q.current_index = Some(1);
    let track = q.get_current().ok_or_else(|| AppError::Validation("get_current None".into()))?;
    assert_eq!(track.id, 2);
    Ok(())
}
