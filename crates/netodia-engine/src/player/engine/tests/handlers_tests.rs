//! Tests for the playback monitor's per-tick decision.
//!
//! `evaluate_playing_tick` is the whole of the `Playing` branch: it decides
//! whether this tick starts a crossfade, stages a gapless preload, or does
//! neither. The two are mutually exclusive for a given transition — a crossfade
//! runs the next track on the *other* deck, while a gapless source would sit on
//! this one, behind the outgoing track, and inherit its fade cell.

use std::num::NonZero;
use std::path::Path;
use std::sync::Arc;

use super::*;
use crate::player::engine::state::PlayerViewModelLight;
use netodia_audio::player::source::audio::{Sample, Shape};
use netodia_core::entities::track::TrackSummary;
use netodia_core::error::AppError;
use netodia_playback::player::playback::crossfade::CrossfadeSettings;
use netodia_playback::player::playback::decks::DECK_COUNT;
use netodia_playback::player::playback::output::mixer;
use netodia_testkit::ASSETS_DIR;
use tokio::sync::watch;

fn track(id: i64, album: Option<&str>) -> Arc<TrackSummary> {
    Arc::new(TrackSummary {
        id,
        file_path: format!("/music/{id}.mp3"),
        file_name: format!("{id}.mp3"),
        title: format!("Track {id}"),
        artist: None,
        album: album.map(str::to_owned),
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

/// Two queued tracks on different albums, playing the first, gapless on.
fn playing_state() -> PlayerState {
    let mut state = PlayerState::default();
    state.queue.add_tracks(vec![track(1, Some("A")), track(2, Some("B"))]);
    state.queue.current_index = Some(0);
    state.status = PlaybackStatus::Playing;
    state.source = Some(track(1, Some("A")));
    state.duration_ms = 180_000;
    state.gapless_enabled = true;
    state
}

fn crossfade_off() -> CrossfadeSettings {
    CrossfadeSettings {
        enabled: false,
        duration_ms: 2_000,
        manual: false,
        skip_same_album: true,
        fade_on_pause: false,
    }
}

fn crossfade_on(duration_ms: u32) -> CrossfadeSettings {
    CrossfadeSettings { enabled: true, duration_ms, ..crossfade_off() }
}

/// A backend with no device lead, so the ear and the pull read alike.
fn backend(position_ms: u64, xf: CrossfadeSettings) -> BackendSnapshot {
    BackendSnapshot {
        position_ms,
        pulled_ms: position_ms,
        already_preloaded: false,
        crossfading: false,
        xf,
        next_needs_reopen: false,
    }
}

/// A backend whose device still holds what was pulled past `heard_ms`.
fn backend_behind(heard_ms: u64, pulled_ms: u64, xf: CrossfadeSettings) -> BackendSnapshot {
    BackendSnapshot { pulled_ms, ..backend(heard_ms, xf) }
}

fn seated(
    state: PlayerState,
) -> (PlayerStateHandle, PlayerSinks, watch::Receiver<Option<PlayerViewModelLight>>) {
    let (view_model, published) = watch::channel(None);
    let (queue, _) = watch::channel(None);
    let sinks = PlayerSinks {
        view_model,
        queue,
        media_controls: None,
        external_track_playback: std::sync::RwLock::new(None),
    };
    let handle = PlayerStateHandle::default();
    crate::player::engine::state::with_state_emit(&handle, &sinks, |live| *live = state);
    (handle, sinks, published)
}

/// The position at which `remaining_ms` of the 180 s fixture track are left.
fn at_remaining(remaining_ms: u64) -> u64 {
    180_000 - remaining_ms
}

/// Run one tick, asserting it isn't void. `None` (playback moved off `Playing`)
/// is a separate case with its own test.
fn tick(state: &mut PlayerState, backend: BackendSnapshot) -> Option<PlayingTick> {
    let out = evaluate_playing_tick(state, backend);
    assert!(out.is_some(), "a tick on a playing state must not be void");
    out
}

/// The path of the track this tick staged behind the current one, if any.
fn staged(t: Option<&PlayingTick>) -> Option<&str> {
    t.and_then(|t| t.late_preload.as_ref()).map(|(path, _rg)| path.as_str())
}

/// The fade length this tick decided on, if it decided to crossfade.
fn fade_ms(t: Option<&PlayingTick>) -> Option<u64> {
    t.and_then(|t| t.crossfade.as_ref()).map(|d| d.fade_ms)
}

// --- the tick itself -------------------------------------------------------

#[test]
fn a_tick_off_playing_is_void() {
    let mut state = playing_state();
    state.status = PlaybackStatus::Paused;

    assert!(evaluate_playing_tick(&mut state, backend(1_000, crossfade_off())).is_none());
}

/// The backend is read before the state lock, so a stop can land between that read and this.
/// The position it read must not go back over the stop's zero, or play would carry on from it
/// rather than start the track over.
#[test]
fn a_tick_read_before_a_stop_leaves_the_track_at_the_top() {
    let mut state = playing_state();
    state.position_ms = 30_000;
    state.build_stop_actions(0);

    evaluate_playing_tick(&mut state, backend(30_500, crossfade_off()));

    assert_eq!(state.position_ms, 0);
}

#[test]
fn a_normal_tick_publishes_the_position_and_does_nothing_else() {
    let mut state = playing_state();

    let t = tick(&mut state, backend(30_000, crossfade_off()));

    assert_eq!(t.as_ref().map(|t| t.tick.position_ms), Some(30_000));
    assert_eq!(t.as_ref().map(|t| t.tick.duration_ms), Some(180_000));
    assert_eq!(state.position_ms, 30_000);
    assert_eq!(fade_ms(t.as_ref()), None);
    assert_eq!(staged(t.as_ref()), None, "far from the end, nothing to stage");
}

/// Everything published, persisted or reported outward is where the ear is, which is a device's
/// lead behind what the voices have pulled.
#[test]
fn the_tick_publishes_what_the_ear_hears_not_what_was_pulled() {
    let mut state = playing_state();

    let t = tick(&mut state, backend_behind(30_000, 30_080, crossfade_off()));

    assert_eq!(t.as_ref().map(|t| t.tick.position_ms), Some(30_000));
    assert_eq!(state.position_ms, 30_000);
}

/// The preload stages what the voice pulls next, so it times against the pulled clock: an ear still
/// outside the lead window must not hold it back once the pull is inside.
#[test]
fn the_late_preload_times_against_the_pulled_clock() {
    let rows = [
        ("pulled inside the window", PRELOAD_LEAD_MS + 100, PRELOAD_LEAD_MS - 100, true),
        ("pulled outside it", PRELOAD_LEAD_MS - 100, PRELOAD_LEAD_MS + 100, false),
    ];
    for (what, heard_left, pulled_left, stages) in rows {
        let mut state = playing_state();
        let backend =
            backend_behind(at_remaining(heard_left), at_remaining(pulled_left), crossfade_off());

        let t = tick(&mut state, backend);

        assert_eq!(staged(t.as_ref()).is_some(), stages, "{what}");
    }
}

/// The ramp is laid over what the voice pulls next, so it has to land on the track's real end as
/// pulled, not on where the ear still is.
#[test]
fn the_crossfade_times_against_the_pulled_clock() {
    let mut state = playing_state();
    let xf = crossfade_on(2_000);

    let t = tick(&mut state, backend_behind(at_remaining(2_100), at_remaining(1_900), xf));

    assert_eq!(fade_ms(t.as_ref()), Some(1_900));
}

// --- gapless preload -------------------------------------------------------

#[test]
fn the_gapless_preload_stages_the_next_track_near_the_end() {
    let mut state = playing_state();

    let t = tick(&mut state, backend(at_remaining(1_000), crossfade_off()));

    assert_eq!(
        staged(t.as_ref()),
        Some("/music/2.mp3"),
        "within PRELOAD_LEAD_MS of the end, the next track must be staged"
    );
    assert_eq!(fade_ms(t.as_ref()), None, "crossfade is off");
}

#[test]
fn cloud_queue_transitions_wait_for_the_external_source_resolver() {
    let mut state = playing_state();
    for track in &mut state.queue.tracks {
        Arc::make_mut(track).file_path.clear();
    }
    let mut current = (*track(1, Some("A"))).clone();
    current.file_path.clear();
    state.source = Some(Arc::new(current));

    let decision = tick(&mut state, backend(at_remaining(1_000), crossfade_on(3_000)));

    assert_eq!(staged(decision.as_ref()), None, "an empty cloud URL cannot be preloaded as a file");
    assert_eq!(fade_ms(decision.as_ref()), None, "crossfade waits for an async cloud decoder");
}

#[test]
fn pause_at_track_end_suppresses_the_gapless_preload() {
    let mut state = playing_state();
    // The sleep timer's "End of current track": the track must drain to
    // `EndOfStream`, not `GaplessTransition` (which would already be playing the
    // next one) — that's the only boundary the pause gate can catch.
    state.pause_after_current_track = true;

    let t = tick(&mut state, backend(at_remaining(1_000), crossfade_off()));

    assert_eq!(staged(t.as_ref()), None);
    assert_eq!(fade_ms(t.as_ref()), None);
}

#[test]
fn a_zero_position_read_never_stages_a_preload() {
    let mut state = playing_state();

    // A deck reads 0 until it has been pulled for its new source, which would
    // otherwise read as a whole track's worth of remaining time.
    let t = tick(&mut state, backend(0, crossfade_off()));

    assert_eq!(staged(t.as_ref()), None);
}

// --- crossfade vs gapless: the mutual exclusion -----------------------------

#[test]
fn an_eligible_crossfade_suppresses_the_gapless_preload() {
    let mut state = playing_state();

    // Inside the preload window, but this transition belongs to the crossfade
    // path — so the preload must not fire, whatever the timing says.
    let t = tick(&mut state, backend(at_remaining(1_400), crossfade_on(2_000)));

    assert_eq!(
        staged(t.as_ref()),
        None,
        "a gapless source would sit on the outgoing deck and inherit its fade cell"
    );
}

#[test]
fn a_crossfade_shorter_than_the_preload_lead_still_fires() {
    let mut state = playing_state();
    let xf = crossfade_on(crossfade::MIN_CROSSFADE_MS);

    // The regression this guards: the preload window (1.5 s) is *wider* than a
    // 1 s crossfade. Gating the preload on the timing-dependent predicate would
    // let it fire first here, set `gapless_pending`, and then permanently block
    // the crossfade via its own `!gapless_pending` gate.
    let early = tick(&mut state, backend(at_remaining(1_400), xf));
    assert_eq!(staged(early.as_ref()), None, "the preload must stay suppressed");
    assert_eq!(fade_ms(early.as_ref()), None, "too early for a 1 s fade");

    let t = tick(&mut state, backend(at_remaining(900), xf));

    // The ramp lands on the real track end, not on the configured duration.
    assert_eq!(fade_ms(t.as_ref()), Some(900));
    assert_eq!(
        t.as_ref().and_then(|t| t.crossfade.as_ref()?.track_id),
        Some(1),
        "the decision carries the OUTGOING track's id"
    );
    assert_eq!(
        t.as_ref().and_then(|t| t.crossfade.as_ref()).map(|d| d.position_ms),
        Some(at_remaining(900))
    );
}

#[test]
fn a_same_album_transition_stays_gapless() {
    let mut state = playing_state();
    // Both tracks on album "A": the default `skip_same_album` keeps
    // continuous-mix albums seamless.
    state.queue.clear();
    state.queue.add_tracks(vec![track(1, Some("A")), track(2, Some("A"))]);
    state.queue.current_index = Some(0);

    let t = tick(&mut state, backend(at_remaining(1_000), crossfade_on(2_000)));

    assert_eq!(fade_ms(t.as_ref()), None, "same album, so no crossfade");
    assert_eq!(
        staged(t.as_ref()),
        Some("/music/2.mp3"),
        "and the gapless preload takes over instead"
    );
}

#[test]
fn a_crossfade_in_flight_blocks_both_paths() {
    let mut state = playing_state();
    let mut snap = backend(at_remaining(1_000), crossfade_on(2_000));
    snap.crossfading = true;

    let t = tick(&mut state, snap);

    assert_eq!(fade_ms(t.as_ref()), None, "one overlap at a time");
    assert_eq!(staged(t.as_ref()), None);
}

#[test]
fn a_staged_gapless_source_blocks_the_crossfade() {
    let mut state = playing_state();
    let mut snap = backend(at_remaining(1_500), crossfade_on(2_000));
    snap.already_preloaded = true;

    let t = tick(&mut state, snap);

    // The staged source shares the deck's fade cell, so it would inherit the
    // outgoing ramp. `begin_crossfade` debug-asserts this can't happen.
    assert_eq!(fade_ms(t.as_ref()), None);
    assert_eq!(staged(t.as_ref()), None, "already staged");
}

#[test]
fn the_last_track_neither_crossfades_nor_preloads() {
    let mut state = playing_state();
    state.queue.clear();
    state.queue.add_tracks(vec![track(1, Some("A"))]);
    state.queue.current_index = Some(0);

    let t = tick(&mut state, backend(at_remaining(1_000), crossfade_on(2_000)));

    assert_eq!(fade_ms(t.as_ref()), None);
    assert_eq!(staged(t.as_ref()), None);
}

/// No fade can cross a reopen, so a next track the output reopens for drains to its end instead,
/// and the tick still offers the preload, which the backend refuses for the same reason. Without
/// the reopen the crossfade takes the transition as before, and with crossfade off the reopen
/// changes nothing.
#[test]
fn a_next_track_the_output_reopens_for_is_never_crossfaded_into() {
    let rows = [
        ("crossfade on, a reopen ahead", crossfade_on(2_000), true, None, Some("/music/2.mp3")),
        ("crossfade on, no reopen", crossfade_on(2_000), false, Some(1_000), None),
        ("crossfade off, a reopen ahead", crossfade_off(), true, None, Some("/music/2.mp3")),
    ];
    for (what, xf, next_needs_reopen, expected_fade, expected_staged) in rows {
        let mut state = playing_state();
        let snapshot = BackendSnapshot { next_needs_reopen, ..backend(at_remaining(1_000), xf) };

        let t = tick(&mut state, snapshot);

        assert_eq!(
            (fade_ms(t.as_ref()), staged(t.as_ref())),
            (expected_fade, expected_staged),
            "{what}"
        );
    }
}

// --- a long pause giving an exclusive device back --------------------------

/// The polls up to and including the first that answers due, or `None` past a hold and a half.
fn polls_until_due(watch: &mut PauseWatch) -> Option<u64> {
    (1..=RELEASE_AFTER_N_TICKS * 3 / 2).find(|_| watch.due(true))
}

#[test]
fn a_held_pause_is_due_on_the_poll_that_completes_the_hold() {
    let mut watch = PauseWatch::default();

    let due_after = polls_until_due(&mut watch);

    assert_eq!(due_after, Some(RELEASE_AFTER_N_TICKS));
}

/// The state machine can turn a release down, a seek having landed in the gap, so the count starts
/// over once it answers and asks again a whole hold later rather than never.
#[test]
fn a_release_turned_down_is_asked_for_again_a_whole_hold_later() {
    let mut watch = PauseWatch::default();
    let _first = polls_until_due(&mut watch);

    let due_after = polls_until_due(&mut watch);

    assert_eq!(due_after, Some(RELEASE_AFTER_N_TICKS));
}

/// A play in between ends the hold, so two pauses either side of it never add up to one.
#[test]
fn a_poll_that_is_not_holding_starts_the_count_over() {
    let mut watch = PauseWatch::default();
    for _ in 1..RELEASE_AFTER_N_TICKS {
        watch.due(true);
    }
    watch.due(false);

    let due_after = polls_until_due(&mut watch);

    assert_eq!(due_after, Some(RELEASE_AFTER_N_TICKS));
}

/// A seek while paused moves the state at once and sends the deck its swap without waiting for it
/// to land. A release in that gap must not take the deck's old reading for where to resume.
#[tokio::test]
async fn a_release_never_writes_back_the_position_a_seek_moved_off() -> Result<(), AppError> {
    const SEEK_MS: u64 = 700;
    let fixture = Path::new(ASSETS_DIR).join("silence.wav").to_string_lossy().into_owned();
    let device = Shape {
        channels: NonZero::new(2).unwrap_or(NonZero::<u16>::MIN),
        rate: NonZero::new(44_100).unwrap_or(NonZero::<u32>::MIN),
    };
    let (mixer, mut pull) = mixer::pair(DECK_COUNT, device);
    let engine = PlaybackEngine::new(&mixer, tokio::runtime::Handle::current())?;
    engine.play_media(&fixture, 1.0, 1.0, None, TrackReplayGain::default())?;
    let mut block: Vec<Sample> = vec![0.0; 4_096];
    pull.fill(&mut block);
    engine.pause();
    engine.seek(&fixture, SEEK_MS, TrackReplayGain::default());
    let paused =
        PlayerState { status: PlaybackStatus::Paused, position_ms: SEEK_MS, ..playing_state() };
    let (handle, sinks, _published) = seated(paused);

    release_paused_track(&engine, &handle, &sinks);

    assert_eq!(lock_state(&handle).position_ms, SEEK_MS);
    Ok(())
}

/// The seed is the half of the rate limiter worth holding, and it is only assertable because
/// `SecondGate` carries it: a bare counter starting at `0` reads position 0 as already
/// published, and the now-playing bar shows nothing until the track crosses one second.
#[test]
fn the_first_tick_of_a_track_publishes_before_any_second_boundary() {
    assert!(SecondGate::default().admits(0));
}

#[test]
fn two_ticks_inside_one_second_publish_once() {
    let mut gate = SecondGate::default();
    assert!(gate.admits(4_000));
    assert!(!gate.admits(4_500), "the poll rate is twice what the UI can render");
}

#[test]
fn crossing_a_second_boundary_publishes_again() {
    let mut gate = SecondGate::default();
    assert!(gate.admits(4_500));
    assert!(gate.admits(5_000));
}

/// A seek lands wherever the user dropped the thumb, so the gate has to answer on the second
/// having *moved* rather than advanced. Comparing with `>` leaves the labels stale until
/// playback catches back up to where it was.
#[test]
fn a_seek_backwards_publishes_rather_than_waiting_for_the_old_second() {
    let mut gate = SecondGate::default();
    assert!(gate.admits(90_000));
    assert!(gate.admits(10_000));
}

/// The old source may already be drained when a seek suspends output. Its empty deck must not
/// advance the queue before the replacement source is ready.
#[tokio::test]
async fn a_suspended_seek_does_not_advance_the_queue_or_publish_the_old_clock()
-> Result<(), AppError> {
    let device = Shape {
        channels: NonZero::new(2).unwrap_or(NonZero::<u16>::MIN),
        rate: NonZero::new(44_100).unwrap_or(NonZero::<u32>::MIN),
    };
    let (mixer, _pull) = mixer::pair(DECK_COUNT, device);
    let engine = Arc::new(PlaybackEngine::new(&mixer, tokio::runtime::Handle::current())?);
    let mut state = playing_state();
    state.position_ms = 1_234;
    let (handle, sinks, _published) = seated(state);
    let handle = Arc::new(handle);
    let (position_tx, positions) = watch::channel(None);
    let (signal_path_tx, _) = watch::channel(None);
    engine.pause();
    let mut monitor = Monitor::new(PlaybackMonitorContext {
        shutdown_token: CancellationToken::new(),
        player_state: handle.clone(),
        engine,
        sinks: Arc::new(sinks),
        position_tx,
        signal_path_tx,
        save: Arc::new(|_| Box::pin(async {})),
    });

    assert!(monitor.poll() == Poll::Skipped);
    assert!(monitor.end_of_stream() == Poll::Skipped, "a pre-seek EOS snapshot must be refused");

    let state = lock_state(&handle);
    assert_eq!(state.queue.get_current().map(|track| track.id), Some(1));
    assert_eq!(state.position_ms, 1_234);
    assert!(positions.borrow().is_none());
    Ok(())
}
