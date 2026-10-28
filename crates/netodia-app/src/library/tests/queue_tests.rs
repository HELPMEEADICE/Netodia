use std::sync::Arc;

use netodia_core::entities::track::TrackSummary;
use netodia_core::error::AppError;
use netodia_engine::player::engine::state::{PlayerState, lock_state};

use super::*;
use crate::state::fixtures::test_sinks;

fn make_summary(id: i64) -> Arc<TrackSummary> {
    Arc::new(TrackSummary {
        id,
        file_path: format!("/music/{id}.mp3"),
        file_name: format!("{id}.mp3"),
        title: format!("Track {id}"),
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

#[test]
fn shuffle_inline_empty_queue_noop() {
    let mut state = PlayerState::default();
    shuffle_inline(&mut state);
    assert!(!state.queue.shuffle_enabled);
}

#[test]
fn shuffle_inline_single_track() {
    let mut state = PlayerState::default();
    state.queue.add_tracks(vec![make_summary(1)]);
    state.queue.current_index = Some(0);

    shuffle_inline(&mut state);

    assert!(state.queue.shuffle_enabled);
    assert_eq!(state.queue.get_current().map(|t| t.id), Some(1));
}

#[test]
fn shuffle_inline_keeps_current_at_front() {
    let mut state = PlayerState::default();
    let tracks: Vec<_> = (1..=10).map(make_summary).collect();
    state.queue.add_tracks(tracks);
    state.queue.current_index = Some(5); // Track 6 is current

    shuffle_inline(&mut state);

    assert_eq!(state.queue.get_current().map(|t| t.id), Some(6));
}

#[test]
fn shuffle_inline_enables_shuffle_flag() {
    let mut state = PlayerState::default();
    state.queue.add_tracks(vec![make_summary(1), make_summary(2), make_summary(3)]);
    state.queue.current_index = Some(0);

    assert!(!state.queue.shuffle_enabled);
    shuffle_inline(&mut state);
    assert!(state.queue.shuffle_enabled);
}

#[test]
fn shuffle_inline_all_indices_present() -> Result<(), AppError> {
    let mut state = PlayerState::default();
    let count: i64 = 20;
    let tracks: Vec<_> = (1..=count).map(make_summary).collect();
    state.queue.add_tracks(tracks);
    state.queue.current_index = Some(0);

    shuffle_inline(&mut state);

    let play_order = state.queue.tracks_in_play_order();
    let expected_len =
        usize::try_from(count).map_err(|_| AppError::Validation("count negative".into()))?;
    assert_eq!(play_order.len(), expected_len);

    let mut ids: Vec<i64> = play_order.iter().map(|t| t.id).collect();
    ids.sort_unstable();
    let expected: Vec<i64> = (1..=count).collect();
    assert_eq!(ids, expected);
    Ok(())
}

#[test]
fn shuffle_unshuffle_roundtrip() {
    let mut state = PlayerState::default();
    let tracks: Vec<_> = (1..=5).map(make_summary).collect();
    let original_ids: Vec<i64> = tracks.iter().map(|t| t.id).collect();
    state.queue.add_tracks(tracks);
    state.queue.current_index = Some(0);

    shuffle_inline(&mut state);
    assert!(state.queue.shuffle_enabled);

    state.queue.unshuffle();
    assert!(!state.queue.shuffle_enabled);

    let restored: Vec<i64> = state.queue.tracks_in_play_order().iter().map(|t| t.id).collect();
    assert_eq!(restored, original_ids);
}

// --- The transport doors ---------------------------------------------------
//
// Driven through the workers under `queue_{set_shuffle,toggle_shuffle,cycle_repeat,set_repeat}`
// rather than by rebuilding their bodies over a bare `PlayerState`, which is what the tests these
// replaced did: none of the three ever called the door it was named after, so an instrumented run
// reported all three doors never executed while all three tests passed. What the doors add over
// the queue methods `netodia-engine` already pins is the branch they pick and the value they hand
// `persist_{shuffle,repeat}`, so that is what these assert.

fn seated_queue(count: i64) -> (PlayerStateHandle, PlayerSinks) {
    let player_state = PlayerStateHandle::default();
    let sinks = test_sinks();
    with_state_emit(&player_state, &sinks, |s| {
        s.queue.add_tracks((1..=count).map(make_summary).collect());
        s.queue.current_index = Some(0);
    });
    (player_state, sinks)
}

fn queue_version(player_state: &PlayerStateHandle) -> u64 {
    lock_state(player_state).queue.version
}

/// `queue.version` is the witness rather than the play order: re-shuffling five tracks lands on
/// the same permutation often enough that an order comparison would pass at random, and the
/// version is what `with_state_emit` gates the queue re-emit on anyway.
#[test]
fn a_shuffle_asked_for_twice_reorders_once() {
    let (player_state, sinks) = seated_queue(5);
    set_shuffle(&player_state, &sinks, true);
    let after_first = queue_version(&player_state);

    set_shuffle(&player_state, &sinks, true);

    assert_eq!(
        queue_version(&player_state),
        after_first,
        "the Shuffle pill is pressed twice by a caller that already asked for it, and a second \
         reorder throws away the position the listener was at"
    );
}

/// The one input where the request and the outcome disagree, and the reason the door answers with
/// the state: `persist_shuffle` takes this value, so a version returning `enabled` writes a
/// shuffle into `settings.json` that the next launch restores over a queue nothing reordered.
#[test]
fn shuffling_an_empty_queue_answers_that_nothing_was_shuffled() {
    let (player_state, sinks) = seated_queue(0);

    let settled = set_shuffle(&player_state, &sinks, true);

    assert!(!settled, "an empty queue has no order to shuffle, so the request cannot be granted");
}

/// Pins the door's `false` arm. The shuffled order is arranged by hand rather than by asking for
/// one, so nothing here depends on the RNG having moved anything: a version clearing the flag
/// without calling `unshuffle` leaves the queue reversed, and gets caught every run instead of
/// once in a hundred and twenty.
#[test]
fn turning_shuffle_off_puts_the_queue_back_in_the_order_it_was_added() {
    let (player_state, sinks) = seated_queue(5);
    with_state_emit(&player_state, &sinks, |s| {
        s.queue.play_order.reverse();
        s.queue.shuffle_enabled = true;
    });

    set_shuffle(&player_state, &sinks, false);

    let state = lock_state(&player_state);
    assert!(!state.queue.shuffle_enabled, "the request was `false`");
    let restored: Vec<i64> = state.queue.tracks_in_play_order().iter().map(|t| t.id).collect();
    assert_eq!(restored, (1..=5).collect::<Vec<i64>>());
}

#[test]
fn the_toggle_flips_whichever_way_shuffle_is_currently_pointing() {
    let (player_state, sinks) = seated_queue(5);

    assert!(toggle_shuffle(&player_state, &sinks), "a queue in order shuffles");
    assert!(!toggle_shuffle(&player_state, &sinks), "a shuffled queue goes back in order");
}

/// The answer is what `persist_repeat` writes, so reporting the mode it left rather than the one
/// it landed on brings the next launch back one press behind. `cycle_repeat_mode`'s own sequence
/// is `netodia-engine`'s claim and is pinned there.
#[test]
fn a_repeat_press_answers_with_the_mode_it_landed_on() {
    let (player_state, sinks) = seated_queue(3);

    let announced = cycle_repeat(&player_state, &sinks);

    assert_eq!(announced, RepeatMode::All, "one press off `Off` lands on `All`");
    assert_eq!(announced, lock_state(&player_state).queue.repeat_mode);
}

/// The answer to `set_repeat` from `start`, over a queue already holding that mode.
fn repeat_set_from(start: RepeatMode, requested: RepeatMode) -> RepeatMode {
    let (player_state, sinks) = seated_queue(3);
    with_state_emit(&player_state, &sinks, |s| s.queue.repeat_mode = start);

    set_repeat(&player_state, &sinks, requested)
}

/// An OS panel names the mode it wants, `playerctl loop Track` among them. Each row pairs a start
/// with a request that one press from it would not reach, so a door that stepped instead of
/// setting fails every row rather than the one that happens to coincide.
#[test]
fn a_repeat_request_lands_on_the_mode_it_names_rather_than_the_next_one() {
    assert_eq!(
        repeat_set_from(RepeatMode::Off, RepeatMode::One),
        RepeatMode::One,
        "a press off `Off` would land on `All`"
    );
    assert_eq!(
        repeat_set_from(RepeatMode::One, RepeatMode::All),
        RepeatMode::All,
        "a press off `One` would land on `Off`"
    );
    assert_eq!(
        repeat_set_from(RepeatMode::All, RepeatMode::Off),
        RepeatMode::Off,
        "a press off `All` would land on `One`"
    );
}

// --- Queue restore ---------------------------------------------------------

use netodia_engine::player::engine::types::PersistableQueue;
use netodia_store::database::queries::fixtures::insert_test_track;
use tempfile::TempDir;

use crate::services::settings::read_settings;
use crate::state::fixtures::seeded_root_with;

fn persisted(track_ids: Vec<i64>, current_index: i32) -> PersistedPlayback {
    PersistedPlayback {
        queue: PersistableQueue { track_ids, current_index },
        playlist_tracks: Vec::new(),
        playlist_current_index: -1,
        current_position_ms: None,
    }
}

fn plan_of(persisted: Option<PersistedPlayback>, summaries: Vec<Arc<TrackSummary>>) -> RestorePlan {
    RestorePlan { persisted, summaries, repeat_mode: RepeatMode::Off, shuffle_enabled: false }
}

/// A data directory with `settings.json` already written, since `mutate_settings_with` reads
/// before it writes and a `Paths` alone points at nothing.
fn seeded_paths(shuffle_enabled: bool) -> Result<(TempDir, Arc<Paths>), AppError> {
    let (tmp, paths) = seeded_root_with(|s| {
        s.queue.shuffle_enabled = shuffle_enabled;
        s.queue.repeat_mode = RepeatMode::All;
    })?;
    Ok((tmp, Arc::new(paths)))
}

#[test]
fn a_saved_queue_restores_as_a_queue() {
    let mut state = PlayerState::default();
    let actions = apply_restore(
        &mut state,
        plan_of(Some(persisted(vec![1, 2], 0)), vec![make_summary(1), make_summary(2)]),
    );

    assert!(actions.is_empty());
    assert_eq!(state.queue.tracks.len(), 2);
    assert_eq!(state.current_track().map(|t| t.id), Some(1));
}

#[test]
fn a_first_launch_with_no_file_restores_nothing() {
    let mut state = PlayerState::default();

    let actions = apply_restore(&mut state, plan_of(None, vec![]));

    assert!(actions.is_empty());
    assert!(state.queue.tracks.is_empty());
    assert!(state.current_track().is_none());
}

#[test]
fn a_missing_queue_file_restores_nothing() -> Result<(), AppError> {
    let tmp = TempDir::new()?;
    let paths = Paths::rooted_at(tmp.path().to_path_buf());

    assert!(load_persisted_playback(&paths).is_none());
    Ok(())
}

/// Best-effort by design: a truncated write or a hand-edited file is a session's playback lost,
/// never a boot that fails.
#[test]
fn an_unparseable_queue_file_restores_nothing() -> Result<(), AppError> {
    let tmp = TempDir::new()?;
    let paths = Paths::rooted_at(tmp.path().to_path_buf());
    std::fs::write(&paths.queue_path, b"{ not json")?;

    assert!(load_persisted_playback(&paths).is_none());
    Ok(())
}

/// The case that actually happens: a folder removed between sessions, so some persisted ids have
/// no row any more. What comes back is what survived.
#[tokio::test]
async fn ids_whose_rows_are_gone_drop_out_of_the_restore() -> Result<(), AppError> {
    let (_tmp, paths) = seeded_paths(false)?;
    let db = DbPool::test_pool().await?;
    queries::folder::insert_folder(&db, "/music", true).await?;
    let kept = insert_test_track(&db, "/music/kept.mp3", "Kept", "Artist", "Album", "Rock").await?;

    let plan = plan_restore(&db, &paths, Some(persisted(vec![kept, 9_999], 0))).await?;

    assert_eq!(plan.summaries.iter().map(|s| s.id).collect::<Vec<_>>(), vec![kept]);
    Ok(())
}

/// Restoring a queue keeps both playback preferences and preserves the saved current track.
#[tokio::test]
async fn a_restored_queue_keeps_playback_preferences() -> Result<(), AppError> {
    let (_tmp, paths) = seeded_paths(true)?;
    let db = DbPool::test_pool().await?;
    queries::folder::insert_folder(&db, "/music", true).await?;
    let id = insert_test_track(&db, "/music/a.mp3", "A", "Artist", "Album", "Rock").await?;

    let plan = plan_restore(&db, &paths, Some(persisted(vec![id], 0))).await?;

    assert!(plan.shuffle_enabled);
    assert!(read_settings(&paths)?.queue.shuffle_enabled);
    assert_eq!(plan.repeat_mode, RepeatMode::All);

    let mut state = PlayerState::default();
    apply_restore(&mut state, plan);
    assert!(state.queue.shuffle_enabled);
    assert_eq!(state.queue.repeat_mode, RepeatMode::All);
    assert_eq!(state.current_track().map(|track| track.id), Some(id));
    state.queue.unshuffle();
    assert!(!state.queue.shuffle_enabled);
    assert_eq!(state.current_track().map(|track| track.id), Some(id));
    Ok(())
}

/// With nothing restored there is no unknown original order to protect, so the user's setting
/// stands.
#[tokio::test]
async fn a_restore_with_no_tracks_leaves_the_shuffle_setting_alone() -> Result<(), AppError> {
    let (_tmp, paths) = seeded_paths(true)?;
    let db = DbPool::test_pool().await?;

    let plan = plan_restore(&db, &paths, None).await?;

    assert!(plan.shuffle_enabled);
    assert!(read_settings(&paths)?.queue.shuffle_enabled);
    Ok(())
}

// --- what a drop and a "Play Next" owe their input order ---

/// A `TrackSummary` with a title of its own, since [`make_summary`] ties one to the id and the
/// ordering tests need the two to vary apart.
fn titled(id: i64, title: &str) -> Arc<TrackSummary> {
    let mut summary = (*make_summary(id)).clone();
    summary.title = title.to_owned();
    Arc::new(summary)
}

/// Files from outside arrive in visual selection order on KDE and GNOME, not alphabetical, and
/// `natord` is what puts "Track 2" ahead of "Track 10".
#[test]
fn a_dropped_batch_is_ordered_the_way_a_list_is() {
    let mut batch = vec![titled(1, "Track 10"), titled(2, "Track 9"), titled(3, "Track 1")];

    sort_for_queue(&mut batch);

    assert_eq!(
        batch.iter().map(|t| t.title.as_str()).collect::<Vec<_>>(),
        ["Track 1", "Track 9", "Track 10"],
        "a byte compare puts 10 ahead of 9, which is every numbered album out of order"
    );
}

/// The sort is stable, so a batch the tags cannot tell apart keeps the order the file manager
/// handed it over in rather than an arbitrary one.
#[test]
fn equal_titles_keep_the_order_the_drop_handed_over() {
    let mut batch = vec![titled(7, "Untitled"), titled(3, "Untitled"), titled(5, "Untitled")];

    sort_for_queue(&mut batch);

    assert_eq!(batch.iter().map(|t| t.id).collect::<Vec<_>>(), [7, 3, 5]);
}

/// "Play Next" on a multi-row selection has to land in the order the rows were listed. Every
/// `insert_next` goes to `current_index + 1`, so the batch is walked backwards to come out
/// forwards, and nothing else in the queue moves.
#[tokio::test]
async fn play_next_lands_a_batch_in_input_order_behind_the_current_track() -> Result<(), AppError> {
    let db = DbPool::test_pool().await?;
    queries::folder::insert_folder(&db, "/music", true).await?;
    let mut picked = Vec::new();
    for n in 1..=3 {
        let title = format!("Pick {n}");
        picked.push(
            insert_test_track(&db, &format!("/music/pick{n}.mp3"), &title, "A", "B", "Rock")
                .await?,
        );
    }

    let player_state = PlayerStateHandle::default();
    let sinks = test_sinks();
    with_state_emit(&player_state, &sinks, |s| {
        s.queue.add_tracks(vec![make_summary(100), make_summary(200)]);
        s.queue.current_index = Some(0);
    });

    play_next_many(&db, &player_state, &sinks, &picked).await?;

    let state = lock_state(&player_state);
    let mut expected = vec![100];
    expected.extend(picked.iter().copied());
    expected.push(200);
    assert_eq!(
        state.queue.tracks_in_play_order().iter().map(|t| t.id).collect::<Vec<_>>(),
        expected,
        "the selection lands behind what is playing, in the order it was picked"
    );
    Ok(())
}

/// The context menu can fire on an empty selection, and the guard is what keeps that off the
/// database and off the view-model channel.
#[tokio::test]
async fn play_next_with_nothing_picked_does_not_reach_the_queue() -> Result<(), AppError> {
    let db = DbPool::test_pool().await?;
    let player_state = PlayerStateHandle::default();
    let sinks = test_sinks();
    let mut published = sinks.view_model.subscribe();
    published.borrow_and_update();

    play_next_many(&db, &player_state, &sinks, &[]).await?;

    assert!(!published.has_changed().unwrap_or(true), "nothing was picked, so nothing changed");
    assert!(lock_state(&player_state).queue.tracks.is_empty());
    Ok(())
}

// --- Opening a file from outside the app ------------------------------------
//
// The Rust end of the file-association handoff, and the only half of it nothing pinned. `%F` on
// the four `.desktop` sources, `wix/main.wxs`'s `FileAssociations`, the single-instance claim and
// `boot::tasks::serve_file_opens`' backlog are all held somewhere; what they hand to was not.
//
// The queue order survives the round trip because `get_track_summaries_by_ids` re-orders its rows
// to match the ids it was given, so what `open_as_queue` sorts is what ends up on screen.

use netodia_artwork::media::image::artwork::new_cover_cache;
use netodia_core::entities::tags::{FieldEdit, TagEdit};

use crate::state::fixtures::TestPlayback;

/// Stage the silence fixture under `file_name` carrying `title`, and spell its path the way a
/// file manager would. The title has to go in the file: this path imports rather than inserting,
/// so the tag is the only place the sort can read a name from.
fn opened_file(dir: &std::path::Path, file_name: &str, title: &str) -> Result<String, AppError> {
    let dest = dir.join(file_name);
    std::fs::copy(
        std::path::PathBuf::from(netodia_testkit::ASSETS_DIR).join("silence.mp3"),
        &dest,
    )?;
    let edit = TagEdit { title: FieldEdit::Set(title.to_owned()), ..TagEdit::default() };
    netodia_store::media::ingest::tag_writer::apply_to_file(&dest, &edit, None)?;
    Ok(dest.to_string_lossy().into_owned())
}

fn queued_titles(player_state: &PlayerStateHandle) -> Vec<String> {
    lock_state(player_state)
        .queue
        .tracks_in_play_order()
        .iter()
        .map(|track| track.title.clone())
        .collect()
}

/// The order the user sees is the sorted one, not the order the file manager handed over and not
/// the order the ids came back in. `natord` is what makes the three distinguishable: dropped in
/// reverse, "Track 10" sorts after "Track 2" where a plain string compare puts it first.
#[tokio::test]
async fn opened_files_reach_the_queue_in_natural_title_order() -> Result<(), AppError> {
    let fx = TestPlayback::empty().await?;
    let dir = fx.tmp.path();
    let opened = vec![
        opened_file(dir, "c.mp3", "Track 10")?,
        opened_file(dir, "b.mp3", "Track 2")?,
        opened_file(dir, "a.mp3", "Track 1")?,
    ];

    open_as_queue(&fx.ctx, &new_cover_cache(), &opened).await?;

    assert_eq!(queued_titles(&fx.ctx.player_state), ["Track 1", "Track 2", "Track 10"]);
    Ok(())
}

/// The sibling above pins the list; this pins which of it plays. A queue in the right order with
/// `current_index` somewhere else in it is the same bug from the listener's side, and only this
/// notices. (`Some(0)` and `None` are the same call here, `resolve_start_slot` falling back to
/// the head, so that is not what this is about.)
#[tokio::test]
async fn opening_a_batch_starts_at_the_first_track_in_order() -> Result<(), AppError> {
    let fx = TestPlayback::empty().await?;
    let dir = fx.tmp.path();
    let opened = vec![opened_file(dir, "b.mp3", "Second")?, opened_file(dir, "a.mp3", "First")?];

    open_as_queue(&fx.ctx, &new_cover_cache(), &opened).await?;

    assert_eq!(
        lock_state(&fx.ctx.player_state).queue.get_current().map(|track| track.title.clone()),
        Some("First".to_owned())
    );
    Ok(())
}

/// Opening replaces; importing appends. The two differ in nothing else, and collapsing them
/// costs the listener the queue they were already playing.
#[tokio::test]
async fn opening_replaces_the_queue_where_importing_appends_to_it() -> Result<(), AppError> {
    let fx = TestPlayback::empty().await?;
    let dir = fx.tmp.path();
    let seated = vec![opened_file(dir, "seated.mp3", "Already Playing")?];
    let arriving = vec![opened_file(dir, "arriving.mp3", "Just Opened")?];

    append_imported(&fx.ctx, &new_cover_cache(), &seated).await?;
    append_imported(&fx.ctx, &new_cover_cache(), &arriving).await?;
    let appended = queued_titles(&fx.ctx.player_state);

    open_as_queue(&fx.ctx, &new_cover_cache(), &arriving).await?;

    assert_eq!(appended, ["Already Playing", "Just Opened"], "the import kept what was there");
    assert_eq!(queued_titles(&fx.ctx.player_state), ["Just Opened"], "the open did not");
    Ok(())
}

/// A batch where nothing could be read is an error rather than an empty queue: the user
/// double-clicked something and is owed an answer, and the caller has no other way to tell that
/// the queue they were listening to is still the right one.
#[tokio::test]
async fn opening_files_that_cannot_be_read_is_an_error_rather_than_an_empty_queue()
-> Result<(), AppError> {
    let fx = TestPlayback::empty().await?;
    let missing = fx.tmp.path().join("never-existed.mp3").to_string_lossy().into_owned();

    let refused = open_as_queue(&fx.ctx, &new_cover_cache(), &[missing]).await;

    assert!(matches!(refused, Err(AppError::Queue(_))), "got {refused:?}");
    assert!(lock_state(&fx.ctx.player_state).queue.tracks.is_empty());
    Ok(())
}

fn playback_mode_after_press(
    player_state: &PlayerStateHandle,
    sinks: &PlayerSinks,
) -> (RepeatMode, bool) {
    cycle_playback_mode(player_state, sinks);
    let state = lock_state(player_state);
    (state.queue.repeat_mode, state.queue.shuffle_enabled)
}

#[test]
fn playback_mode_cycles_through_all_four_modes_and_wraps() {
    let (player_state, sinks) = seated_queue(5);

    let modes = [
        playback_mode_after_press(&player_state, &sinks),
        playback_mode_after_press(&player_state, &sinks),
        playback_mode_after_press(&player_state, &sinks),
        playback_mode_after_press(&player_state, &sinks),
        playback_mode_after_press(&player_state, &sinks),
    ];

    assert_eq!(
        modes,
        [
            (RepeatMode::All, false),
            (RepeatMode::One, false),
            (RepeatMode::Off, true),
            (RepeatMode::Off, false),
            (RepeatMode::All, false),
        ]
    );
}

#[test]
fn playback_mode_entering_shuffle_preserves_the_current_track() {
    let (player_state, sinks) = seated_queue(5);
    with_state_emit(&player_state, &sinks, |s| s.queue.current_index = Some(2));
    set_repeat(&player_state, &sinks, RepeatMode::One);

    cycle_playback_mode(&player_state, &sinks);

    assert_eq!(lock_state(&player_state).queue.get_current().map(|track| track.id), Some(3));
}

#[test]
fn playback_mode_leaving_shuffle_restores_order_and_preserves_the_current_track() {
    let (player_state, sinks) = seated_queue(5);
    with_state_emit(&player_state, &sinks, |s| {
        s.queue.play_order.reverse();
        s.queue.current_index = Some(1);
        s.queue.shuffle_enabled = true;
    });

    cycle_playback_mode(&player_state, &sinks);

    let state = lock_state(&player_state);
    let order: Vec<i64> = state.queue.tracks_in_play_order().iter().map(|t| t.id).collect();
    assert_eq!(order, vec![1, 2, 3, 4, 5]);
    assert_eq!(state.queue.get_current().map(|track| track.id), Some(4));
}

#[test]
fn playback_mode_leaves_external_shuffle_repeat_combination_in_sequential_mode() {
    let (player_state, sinks) = seated_queue(5);
    set_shuffle(&player_state, &sinks, true);
    set_repeat(&player_state, &sinks, RepeatMode::One);

    let mode = playback_mode_after_press(&player_state, &sinks);

    assert_eq!(mode, (RepeatMode::Off, false));
}

#[test]
fn playback_mode_can_select_shuffle_before_tracks_are_added() {
    let (player_state, sinks) = seated_queue(0);
    set_repeat(&player_state, &sinks, RepeatMode::One);

    let mode = playback_mode_after_press(&player_state, &sinks);

    assert_eq!(mode, (RepeatMode::Off, true));
}

#[test]
fn playback_mode_can_shuffle_a_single_track() {
    let (player_state, sinks) = seated_queue(1);
    set_repeat(&player_state, &sinks, RepeatMode::One);

    let mode = playback_mode_after_press(&player_state, &sinks);

    assert_eq!(mode, (RepeatMode::Off, true));
    assert_eq!(lock_state(&player_state).queue.get_current().map(|track| track.id), Some(1));
}

#[tokio::test]
async fn playback_mode_first_launch_is_sequential() -> Result<(), AppError> {
    let (_tmp, paths) = seeded_root_with(|_| {})?;
    let db = DbPool::test_pool().await?;

    let plan = plan_restore(&db, &Arc::new(paths), None).await?;

    assert_eq!(plan.repeat_mode, RepeatMode::Off);
    assert!(!plan.shuffle_enabled);
    Ok(())
}

async fn saved_playback_mode_after_press(
    repeat: RepeatMode,
    shuffle: bool,
    track_count: i64,
) -> Result<(RepeatMode, bool), AppError> {
    let (_tmp, paths) = seeded_root_with(|_| {})?;
    let paths = Arc::new(paths);
    let db = DbPool::test_pool().await?;
    let (player_state, sinks) = seated_queue(track_count);
    set_repeat(&player_state, &sinks, repeat);
    set_shuffle(&player_state, &sinks, shuffle);

    cycle_playback_mode(&player_state, &sinks);
    write_playback_mode(&paths, &player_state)?;
    let plan = plan_restore(&db, &paths, None).await?;
    let mut restarted = PlayerState::default();
    apply_restore(&mut restarted, plan);
    Ok((restarted.queue.repeat_mode, restarted.queue.shuffle_enabled))
}

#[tokio::test]
async fn playback_mode_remembers_each_selection_after_restart() -> Result<(), AppError> {
    assert_eq!(
        saved_playback_mode_after_press(RepeatMode::Off, false, 3).await?,
        (RepeatMode::All, false)
    );
    assert_eq!(
        saved_playback_mode_after_press(RepeatMode::All, false, 3).await?,
        (RepeatMode::One, false)
    );
    assert_eq!(
        saved_playback_mode_after_press(RepeatMode::One, false, 3).await?,
        (RepeatMode::Off, true)
    );
    assert_eq!(
        saved_playback_mode_after_press(RepeatMode::Off, true, 3).await?,
        (RepeatMode::Off, false)
    );
    Ok(())
}

#[tokio::test]
async fn playback_mode_remembers_shuffle_even_without_tracks() -> Result<(), AppError> {
    assert_eq!(
        saved_playback_mode_after_press(RepeatMode::One, false, 0).await?,
        (RepeatMode::Off, true)
    );
    Ok(())
}
