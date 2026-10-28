//! The transport doors, driven as the callbacks drive them.
//!
//! Everything here goes through a shipped `pub fn` over a [`TestPlayback`], whose engine sits on a
//! device-free mixer. What used to be here instead was a private `toggle()` retyping
//! `player_toggle_play_pause`'s branch and two tests retyping `player_play_tracks`' seeding, each
//! written because the door could not be called and each free to drift from the door it stood in
//! for. The state machine underneath is `netodia-engine`'s and its builders are pinned in that
//! crate's `state_tests.rs`; what is left here is the layer above them.

use super::*;
use crate::services::settings::{SettingsData, read_settings, write_settings};
use crate::state::fixtures::TestPlayback;
use netodia_engine::player::engine::state::{MAX_SPEED, MIN_SPEED, PlayerState};

fn make_summary(id: i64, duration_ms: i64) -> Arc<TrackSummary> {
    Arc::new(TrackSummary {
        id,
        file_path: format!("/music/{id}.mp3"),
        file_name: format!("{id}.mp3"),
        title: format!("Track {id}"),
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

/// Mutate the fixture's state through the shipped emit, so a test starts from a state the machine
/// built rather than from fields poked into it.
fn seat<R>(fx: &TestPlayback, f: impl FnOnce(&mut PlayerState) -> R) -> R {
    with_state_emit(&fx.ctx.player_state, &fx.ctx.sinks, f)
}

/// A fixture already playing `count` staged files from the top, started the way the header Play
/// pill starts one: no row picked, so the head is the fallback rather than a choice.
async fn playing(count: usize) -> Result<(TestPlayback, Vec<i64>), AppError> {
    let fx = TestPlayback::empty().await?;
    let ids = fx.stage_playable(count).await?;
    player_play_tracks(&fx.ctx, ids.clone(), None).await?;
    Ok((fx, ids))
}

// --- resolve_start_slot ---

#[test]
fn start_slot_follows_the_picked_track() {
    let ids = vec![1, 2, 3];
    let summaries: Vec<_> = (1..=3).map(|i| make_summary(i, 180_000)).collect();
    assert_eq!(resolve_start_slot(&ids, &summaries, Some(2)), Some(2));
}

/// The regression this exists for: a row deleted between the view's fetch and
/// the click drops out of `summaries`, shifting every slot behind it. Resolving
/// by index would start on the neighbour.
#[test]
fn start_slot_survives_a_track_missing_from_the_fetch() {
    let ids = vec![1, 2, 3, 4, 5];
    let summaries: Vec<_> = [1, 3, 4, 5].into_iter().map(|i| make_summary(i, 180_000)).collect();

    // The user clicked track 4, which the displayed list holds at index 3.
    // Track 2 vanished before the fetch, so slot 3 is now track 5 — taking
    // the index at face value would start playback one track late.
    let start = resolve_start_slot(&ids, &summaries, Some(3));
    assert_eq!(start, Some(2));
    assert_eq!(start.and_then(|i| summaries.get(i)).map(|t| t.id), Some(4));
}

#[test]
fn start_slot_is_none_when_the_picked_track_is_gone() {
    let ids = vec![1, 2, 3];
    let summaries: Vec<_> = [1, 3].into_iter().map(|i| make_summary(i, 180_000)).collect();
    assert_eq!(resolve_start_slot(&ids, &summaries, Some(1)), None);
}

#[test]
fn start_slot_is_none_for_an_out_of_range_index() {
    let ids = vec![1, 2, 3];
    let summaries: Vec<_> = (1..=3).map(|i| make_summary(i, 180_000)).collect();
    assert_eq!(resolve_start_slot(&ids, &summaries, Some(100)), None);
}

#[test]
fn start_slot_is_none_without_an_index() {
    let ids = vec![1, 2, 3];
    let summaries: Vec<_> = (1..=3).map(|i| make_summary(i, 180_000)).collect();
    assert_eq!(resolve_start_slot(&ids, &summaries, None), None);
}

// --- player_play_tracks ---

#[tokio::test]
async fn the_queue_becomes_the_list_the_user_picked_from() -> Result<(), AppError> {
    let (fx, ids) = playing(3).await?;
    player_play_tracks(&fx.ctx, ids.clone(), Some(2)).await?;

    let state = lock_state(&fx.ctx.player_state);
    assert_eq!(
        state.queue.tracks.iter().map(|t| t.id).collect::<Vec<_>>(),
        ids,
        "the queue is the list that was on screen, in the order it was on screen"
    );
    assert_eq!(state.queue.current_index, Some(2));
    assert_eq!(state.current_track().map(|t| t.id), ids.get(2).copied());
    assert_eq!(state.status, PlaybackStatus::Playing);
    Ok(())
}

/// The warn arm for a row that went away between the view's fetch and the click. Starting at the
/// head is a fallback rather than a failure: the user asked for this list.
#[tokio::test]
async fn a_pick_whose_row_is_gone_starts_at_the_head() -> Result<(), AppError> {
    let fx = TestPlayback::empty().await?;
    let ids = fx.stage_playable(2).await?;
    let with_a_hole = vec![ids[0], 9_999, ids[1]];

    player_play_tracks(&fx.ctx, with_a_hole, Some(1)).await?;

    let state = lock_state(&fx.ctx.player_state);
    assert_eq!(state.queue.tracks.len(), 2, "the id with no row drops out");
    assert_eq!(state.current_track().map(|t| t.id), ids.first().copied());
    Ok(())
}

/// The other warn arm, and a different fault: the caller's own index is past its own list.
#[tokio::test]
async fn an_index_past_the_ids_handed_in_starts_at_the_head() -> Result<(), AppError> {
    let fx = TestPlayback::empty().await?;
    let ids = fx.stage_playable(3).await?;

    player_play_tracks(&fx.ctx, ids.clone(), Some(99)).await?;

    let state = lock_state(&fx.ctx.player_state);
    assert_eq!(state.current_track().map(|t| t.id), ids.first().copied());
    Ok(())
}

/// The refusal lands before the emit, which is what keeps a mis-click from clearing the queue the
/// user was listening to.
#[tokio::test]
async fn no_valid_ids_is_refused_without_touching_the_queue() -> Result<(), AppError> {
    let (fx, ids) = playing(2).await?;

    let refused = player_play_tracks(&fx.ctx, vec![9_999], None).await;

    assert!(matches!(refused, Err(AppError::Queue(_))));
    let state = lock_state(&fx.ctx.player_state);
    assert_eq!(state.queue.tracks.iter().map(|t| t.id).collect::<Vec<_>>(), ids);
    assert_eq!(state.status, PlaybackStatus::Playing, "and it is still playing");
    Ok(())
}

/// With shuffle already on, the rest of the list is shuffled *behind* the picked track. A freshly
/// seeded `play_order` is the identity permutation, so without this the shuffle button would stay
/// lit while playback walked the album straight through.
#[tokio::test]
async fn shuffle_already_on_anchors_the_picked_track() -> Result<(), AppError> {
    let fx = TestPlayback::empty().await?;
    let ids = fx.stage_playable(8).await?;
    seat(&fx, |s| s.queue.shuffle_enabled = true);

    player_play_tracks(&fx.ctx, ids.clone(), Some(5)).await?;

    let state = lock_state(&fx.ctx.player_state);
    assert_eq!(state.current_track().map(|t| t.id), ids.get(5).copied());
    let mut queued: Vec<i64> = state.queue.tracks_in_play_order().iter().map(|t| t.id).collect();
    queued.sort_unstable();
    let mut expected = ids;
    expected.sort_unstable();
    assert_eq!(queued, expected, "every track is still queued, exactly once");
    Ok(())
}

// --- the transport doors ---

/// One walk through the transport, because each door is a one-line forward onto a builder
/// `state_tests.rs` already pins. What is worth a test here is the wiring: that each reaches the
/// builder it names, and that the queue they share moves the way the user asked.
#[tokio::test]
async fn each_transport_door_reaches_the_builder_it_names() -> Result<(), AppError> {
    let (fx, ids) = playing(3).await?;

    player_pause(&fx.ctx)?;
    assert_eq!(lock_state(&fx.ctx.player_state).status, PlaybackStatus::Paused);

    player_play(&fx.ctx)?;
    assert_eq!(lock_state(&fx.ctx.player_state).status, PlaybackStatus::Playing);

    player_next(&fx.ctx)?;
    assert_eq!(lock_state(&fx.ctx.player_state).current_track().map(|t| t.id), ids.get(1).copied());

    player_seek(&fx.ctx, 45_000)?;
    assert_eq!(lock_state(&fx.ctx.player_state).position_ms, 45_000);

    player_set_playback_speed(&fx.ctx, 1.5)?;
    assert!((lock_state(&fx.ctx.player_state).playback_speed - 1.5).abs() < f64::EPSILON);

    // Previous steps back even after seeking well into the current track.
    player_previous(&fx.ctx)?;
    {
        let state = lock_state(&fx.ctx.player_state);
        assert_eq!(state.position_ms, 0);
        assert_eq!(state.current_track().map(|t| t.id), ids.first().copied());
    }

    player_stop(&fx.ctx)?;
    let state = lock_state(&fx.ctx.player_state);
    assert_eq!(state.status, PlaybackStatus::Stopped);
    assert!(
        state.current_track().is_some(),
        "a user stop keeps the track so play can start it over"
    );
    Ok(())
}

#[tokio::test]
async fn the_toggle_takes_its_branch_from_the_status() -> Result<(), AppError> {
    let (fx, _ids) = playing(2).await?;

    player_toggle_play_pause(&fx.ctx)?;
    assert_eq!(lock_state(&fx.ctx.player_state).status, PlaybackStatus::Paused);

    player_toggle_play_pause(&fx.ctx)?;
    assert_eq!(lock_state(&fx.ctx.player_state).status, PlaybackStatus::Playing);

    player_stop(&fx.ctx)?;
    player_toggle_play_pause(&fx.ctx)?;
    assert_eq!(
        lock_state(&fx.ctx.player_state).status,
        PlaybackStatus::Playing,
        "a stop keeps the track, so the toggle starts it again"
    );
    Ok(())
}

#[tokio::test]
async fn the_toggle_does_nothing_with_nothing_to_play() -> Result<(), AppError> {
    let fx = TestPlayback::empty().await?;

    player_toggle_play_pause(&fx.ctx)?;

    let state = lock_state(&fx.ctx.player_state);
    assert_eq!(state.status, PlaybackStatus::Stopped);
    assert!(state.current_track().is_none());
    Ok(())
}

/// A transport door a press goes through.
type Door = fn(&PlaybackContext) -> Result<(), AppError>;

/// A long pause gives an exclusive device back by taking the track off its deck and leaving it
/// paused, the `stop` here standing in for that release. A resume would carry on over a deck
/// holding nothing and play silence, so both doors start the track again instead.
#[tokio::test]
async fn play_and_the_toggle_start_a_paused_track_again_once_its_deck_holds_nothing()
-> Result<(), AppError> {
    let doors: [(&str, Door); 2] = [("play", player_play), ("toggle", player_toggle_play_pause)];
    for (door, press) in doors {
        let (fx, _ids) = playing(1).await?;
        player_pause(&fx.ctx)?;
        fx.ctx.engine.stop();

        press(&fx.ctx)?;

        let status = lock_state(&fx.ctx.player_state).status;
        assert_eq!(
            (fx.ctx.engine.holds_source(), status),
            (true, PlaybackStatus::Playing),
            "{door}"
        );
    }
    Ok(())
}

/// The short-circuit is only reachable through the door: it reads the state and returns before
/// `with_state_emit`, so a slider firing the value it already holds costs no publish. Both halves
/// of the guard are here, and the second is the one that bites: the same volume *while muted* has
/// to go through, because passing it is how the user unmutes.
#[tokio::test]
async fn setting_a_volume_already_held_publishes_nothing_unless_it_is_muted() -> Result<(), AppError>
{
    let fx = TestPlayback::empty().await?;
    let mut published = fx.ctx.sinks.view_model.subscribe();
    published.borrow_and_update();

    let held = lock_state(&fx.ctx.player_state).volume;
    player_set_volume(&fx.ctx, held)?;
    assert!(
        !published.has_changed().unwrap_or(true),
        "the value it already holds must not wake every view-model subscriber"
    );

    player_set_volume(&fx.ctx, held - 30)?;
    assert!(published.has_changed().unwrap_or(false), "a real change publishes");
    published.borrow_and_update();

    seat(&fx, PlayerState::build_toggle_mute_actions);
    published.borrow_and_update();

    player_set_volume(&fx.ctx, held - 30)?;

    assert!(published.has_changed().unwrap_or(false), "the same volume while muted is an unmute");
    let state = lock_state(&fx.ctx.player_state);
    assert!(!state.is_muted);
    assert_eq!(state.volume, held - 30);
    Ok(())
}

/// With no device control carrying it, the volume is a gain on the samples like the speed is, so
/// the reset takes both to where they change nothing and hands back the volume for the settings
/// write, which persists whatever it is told.
#[tokio::test]
async fn the_bit_perfect_reset_takes_a_software_volume_and_the_speed_to_unity()
-> Result<(), AppError> {
    let fx = TestPlayback::empty().await?;
    player_set_volume(&fx.ctx, 40)?;
    player_set_playback_speed(&fx.ctx, 1.5)?;
    seat(&fx, PlayerState::build_toggle_mute_actions);

    let left = player_make_bit_perfect(&fx.ctx);

    let state = lock_state(&fx.ctx.player_state);
    let landed = (left, state.volume, state.is_muted, state.playback_speed.to_bits());
    assert_eq!(
        landed,
        (MAX_VOLUME, MAX_VOLUME, false, 1.0_f64.to_bits()),
        "(handed back, volume, muted, speed bits)"
    );
    Ok(())
}

// --- session flags ---

#[tokio::test]
async fn the_sleep_timer_can_be_set_for_the_current_track() -> Result<(), AppError> {
    let (fx, _ids) = playing(1).await?;
    player_set_pause_at_track_end(&fx.ctx, true)?;
    assert!(lock_state(&fx.ctx.player_state).pause_after_current_track);
    Ok(())
}

#[tokio::test]
async fn the_gapless_flag_round_trips_through_its_door() -> Result<(), AppError> {
    let fx = TestPlayback::empty().await?;

    player_set_gapless(&fx.ctx, true)?;
    assert!(lock_state(&fx.ctx.player_state).gapless_enabled);

    player_set_gapless(&fx.ctx, false)?;
    assert!(!lock_state(&fx.ctx.player_state).gapless_enabled);
    Ok(())
}

// --- settings write-through ---

/// A `settings.json` in the fixture's root for a write-through to read. Missing, it is answered
/// with `SettingsData::default()`, which reads the environment the sibling suites mutate.
fn seed_settings(fx: &TestPlayback) -> Result<(), AppError> {
    write_settings(&fx.ctx.paths, &netodia_testkit::reading_env(SettingsData::default))
}

/// The mute the OS media key, the tray and the transport share has to outlive the session, so the
/// toggle writes through rather than only publishing.
#[tokio::test]
async fn toggling_mute_writes_through_to_settings() -> Result<(), AppError> {
    let fx = TestPlayback::empty().await?;
    seed_settings(&fx)?;

    player_toggle_mute(&fx.ctx).await?;
    assert!(read_settings(&fx.ctx.paths)?.playback.is_muted);

    player_toggle_mute(&fx.ctx).await?;
    assert!(!read_settings(&fx.ctx.paths)?.playback.is_muted);
    Ok(())
}

/// The slider fires this on every release, most of which change nothing. On a profile with no
/// `settings.json` yet, a skipped write is a file that never appears.
#[tokio::test]
async fn a_commit_that_would_change_nothing_writes_nothing() -> Result<(), AppError> {
    let fx = TestPlayback::empty().await?;

    commit_player_settings(&fx.ctx).await?;

    assert!(!fx.ctx.paths.settings_path.exists(), "nothing differed, so nothing was written");
    Ok(())
}

/// The slider and the media panels commit through this one door, and the player and the file
/// clamp each on their own, so both bounds are where the two could part and the next launch
/// restore a speed the session never played at.
#[tokio::test]
async fn a_committed_speed_lands_on_the_player_and_in_settings_alike() -> Result<(), AppError> {
    let rows = [(1.5, 1.5), (MAX_SPEED * 2.0, MAX_SPEED), (MIN_SPEED / 2.0, MIN_SPEED)];
    for (asked, landed) in rows {
        let fx = TestPlayback::empty().await?;
        seed_settings(&fx)?;

        player_set_playback_speed_committed(&fx.ctx, asked).await?;

        let played = lock_state(&fx.ctx.player_state).playback_speed;
        let saved = read_settings(&fx.ctx.paths)?.playback.playback_speed;
        assert_eq!(
            (played.to_bits(), saved.to_bits()),
            (landed.to_bits(), landed.to_bits()),
            "asked for {asked}: (player, settings)"
        );
    }
    Ok(())
}

/// The write is awaited rather than left to the blocking pool, so a file that won't take it is
/// reported through the caller's own log line, the slider's or the media panel's.
#[tokio::test]
async fn a_speed_the_settings_file_wont_take_is_reported_to_the_caller() -> Result<(), AppError> {
    let fx = TestPlayback::empty().await?;
    // A directory where the file goes, which no read or write can take for one.
    std::fs::create_dir(&fx.ctx.paths.settings_path)?;

    let committed = player_set_playback_speed_committed(&fx.ctx, 1.5).await;

    assert!(matches!(committed, Err(AppError::Io(_))), "got {committed:?}");
    Ok(())
}

// --- the crossfade cell ---

/// Five one-line forwarders onto one cell, which is exactly the shape a copy-paste crosses wires
/// in. Driving them from a resting cell to five distinct values is what would catch it.
#[tokio::test]
async fn every_crossfade_door_reaches_the_setting_it_names() -> Result<(), AppError> {
    let fx = TestPlayback::empty().await?;

    player_set_crossfade_enabled(&fx.ctx, true);
    player_set_crossfade_duration_ms(&fx.ctx, 4_000);
    player_set_crossfade_manual(&fx.ctx, true);
    player_set_crossfade_skip_same_album(&fx.ctx, true);
    player_set_crossfade_fade_on_pause(&fx.ctx, true);

    let settings = fx.ctx.engine.crossfade_settings();
    assert!(settings.enabled);
    assert_eq!(settings.duration_ms, 4_000);
    assert!(settings.manual);
    assert!(settings.skip_same_album);
    assert!(settings.fade_on_pause);
    Ok(())
}
