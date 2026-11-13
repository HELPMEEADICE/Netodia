//! End-to-end crossfade check against the real audio chain, no audio device.
//!
//! `output::mixer::pair` builds the two halves with no device between them, so two real decks can
//! be driven and the summed output pulled by hand — exercising the Symphonia decoder, `EqSource`'s
//! fade stage, the visualizer tap, the rate converter and the unclamped sum for real.
//!
//! The fixtures are constant-amplitude DC WAVs, because two perfectly correlated
//! signals under a complementary *linear* crossfade sum to a constant: the mixed
//! output must hold steady at the source amplitude across the whole overlap and
//! never exceed it, which is exactly what keeps the unclamped mixer from
//! clipping.
//!
//! ⚠ Pulling the mixer *is* the audio thread here, and `clear` and `try_seek` block until that
//! thread services them, so a control op that makes one must run on a separate thread while this
//! one keeps pulling. See [`drive_until`].

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use netodia_engine::player::engine::backend::PlaybackEngine;
use netodia_playback::player::playback::decks::DECK_COUNT;
use netodia_playback::player::playback::output::mixer::{self, MixerPull};
use netodia_playback::player::playback::replaygain::TrackReplayGain;

mod common;
use common::shape;

const RATE: u32 = 44_100;
const CHANNELS: u16 = 2;
/// Half scale, so the clamp in `EqSource` can never mask a summing bug.
const AMPLITUDE: f32 = 0.5;
/// Long enough that the mixer's lockstep step is a small fraction of the ramp,
/// which is what `SKEW` below is derived from. Also the shipped default.
const FADE_MS: u64 = 2_000;

/// Frames discarded before asserting on steady-state amplitude.
///
/// A deck starts contributing on the first pull after its source is appended, so this is not
/// waiting for the pipeline to fill the way it was under rodio. What it does cover is the ramp's
/// own first steps and the decoder's first packet, neither of which is steady state.
const WARMUP_FRAMES: usize = (RATE as usize) / 10;

/// Fractional slack on the summed amplitude during an overlap, in parts per million.
///
/// What it absorbs is the two ramps sitting a lockstep step apart: they are armed together but each
/// advances as its own deck is pulled, so the deck already rendered for this step picks the arm up
/// on the next one. `output::mixer::LOCKSTEP_FRAMES` is that bound and the assertion below keeps
/// this clear of it — set it under and the suite goes flaky rather than red.
///
/// An integer so that assertion can derive from it rather than restate it, and `u16` because that is
/// the width a cast to `f32` cannot lose.
const SKEW_PPM: u16 = 1_500;

/// [`SKEW_PPM`] as the fraction the amplitude assertions actually compare against.
const SKEW: f32 = SKEW_PPM as f32 / 1_000_000.0;

/// Derived from [`SKEW_PPM`] rather than restating it: the pair of literals this used to compare
/// against was the same number spelled again, so retuning the slack left the guard on the old one.
const _: () = assert!(
    (mixer::LOCKSTEP_FRAMES as u64) * 1_000_000 < FADE_MS * (RATE as u64) / 1_000 * SKEW_PPM as u64,
    "the summed-amplitude slack no longer covers the mixer's lockstep step"
);

/// How long a control op gets to land while this thread pulls for it.
///
/// Wall clock, not a frame count: these waits turn on a blocked thread being
/// woken, and a frame budget measures the puller's throughput instead. Pulling
/// is cheap enough that two seconds of audio retires in tens of milliseconds,
/// so on a contended runner a frame budget expires before the thread it waits
/// for is ever scheduled. Wide because the only thing past it is a hang, and
/// safe to be wide only because [`pull_until`] pulls at a device's rate rather
/// than spinning the six-second fixtures dry.
const CONTROL_OP_BUDGET: Duration = Duration::from_secs(5);

/// How much audio one turn of [`pull_until`]'s wait takes, and the wall clock it takes it over.
///
/// The wait has to keep pulling, the op landing in the deck's callback and this thread being it.
/// But a free-running puller retires seconds of audio in the millisecond the other thread takes to
/// be scheduled, and a deck drained in that window is one `seek` declines to touch, which reads at
/// the assertion as a fade left stranded rather than as the fixture run dry. Holding the turns to
/// the rate a device would ask at is what keeps [`CONTROL_OP_BUDGET`] inside the fixtures.
const WAIT_TURN_MS: u64 = 1;

/// The rate the reopen cases move the output to: the other half of the pair a library of CD rips
/// on a 48 kHz device actually lives between.
const REOPENED_RATE: u32 = 48_000;

fn frames_for_ms(ms: u64) -> usize {
    frames_at(RATE, ms)
}

fn frames_at(rate: u32, ms: u64) -> usize {
    usize::try_from(u64::from(rate) * ms / 1_000).unwrap_or(0)
}

/// Write a 16-bit PCM WAV of constant amplitude. Hand-rolled — the project has
/// no WAV encoder dependency and the canonical header is 44 bytes.
/// `left` and `right` are separate so a fixture can tell its channels apart — the crossfade cases
/// want them equal, [`seeking_never_swaps_the_stereo_image`] wants them not to be.
fn write_dc_wav(path: &Path, seconds: u32, left: f32, right: f32) -> std::io::Result<()> {
    let frames = RATE * seconds;
    let data_len = frames * u32::from(CHANNELS) * 2;
    let mut buf = Vec::with_capacity(44 + data_len as usize);

    buf.extend_from_slice(b"RIFF");
    buf.extend_from_slice(&(36 + data_len).to_le_bytes());
    buf.extend_from_slice(b"WAVEfmt ");
    buf.extend_from_slice(&16u32.to_le_bytes()); // PCM fmt chunk size
    buf.extend_from_slice(&1u16.to_le_bytes()); // PCM
    buf.extend_from_slice(&CHANNELS.to_le_bytes());
    buf.extend_from_slice(&RATE.to_le_bytes());
    buf.extend_from_slice(&(RATE * u32::from(CHANNELS) * 2).to_le_bytes()); // byte rate
    buf.extend_from_slice(&(CHANNELS * 2).to_le_bytes()); // block align
    buf.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
    buf.extend_from_slice(b"data");
    buf.extend_from_slice(&data_len.to_le_bytes());

    let (left, right) = (pcm_sample(left), pcm_sample(right));
    for _ in 0..frames {
        buf.extend_from_slice(&left.to_le_bytes());
        buf.extend_from_slice(&right.to_le_bytes());
    }

    let mut f = std::fs::File::create(path)?;
    f.write_all(&buf)
}

#[allow(
    clippy::cast_possible_truncation,
    reason = "amplitude is a small constant well inside i16 range"
)]
fn pcm_sample(amplitude: f32) -> i16 {
    (amplitude * f32::from(i16::MAX)) as i16
}

/// Pull at the fixture's playback rate so long simulated blocks cannot outrun the decoder worker.
/// The same pace is sufficient after a device reshape: the assertions count frames, not time.
fn fill(src: &mut MixerPull, frames: usize) -> Vec<f32> {
    const BLOCK_FRAMES: usize = 4_096;
    let channels = usize::from(CHANNELS);
    let mut output = Vec::with_capacity(frames * channels);
    let start = Instant::now();
    for first in (0..frames).step_by(BLOCK_FRAMES) {
        let micros = u64::try_from(first).unwrap_or(0) * 1_000_000 / u64::from(RATE);
        let deadline = start + Duration::from_micros(micros);
        if let Some(wait) = deadline.checked_duration_since(Instant::now()) {
            std::thread::sleep(wait);
        }
        let count = (frames - first).min(BLOCK_FRAMES);
        output.extend(common::pull(src, count * channels));
    }
    output
}

/// Pull `frames` frames and return the per-frame amplitude of channel 0.
fn pull(src: &mut MixerPull, frames: usize) -> Vec<f32> {
    fill(src, frames)
        .chunks_exact(usize::from(CHANNELS))
        .map(|frame| {
            let (l, r) = (frame[0], frame[1]);
            // Both channels carry the same DC value, so they must track. Loose because two decks
            // can be a sample out of phase across a transition; exact per-frame gain coupling is
            // pinned against the raw `EqSource` by
            // `a_fade_advances_once_per_frame_not_once_per_sample`.
            assert!((l - r).abs() < 1e-3, "channels sheared: {l} vs {r}");
            l
        })
        .collect()
}

/// Pull `frames` frames as `(left, right)` pairs, for the one fixture whose channels differ.
///
/// [`pull`] collapses a frame to channel 0 after checking the two track, which is right for the DC
/// fixtures and blind to the two being swapped.
fn pull_stereo(src: &mut MixerPull, frames: usize) -> Vec<(f32, f32)> {
    fill(src, frames)
        .chunks_exact(usize::from(CHANNELS))
        .map(|frame| (frame[0], frame[1]))
        .collect()
}

/// Pull `frames` frames without asserting anything about them.
///
/// Whatever is landing across a transition is a transition by definition, so flush it through here
/// before asserting on what follows.
fn pull_lenient(src: &mut MixerPull, frames: usize) {
    let _ = fill(src, frames);
}

/// Turn the mixer until `done`, failing on [`CONTROL_OP_BUDGET`] rather than
/// hanging the suite.
///
/// `clear()` and `try_seek()` return only once the deck's own callback has serviced them, and that
/// callback is whoever pulls the mixer — in production the audio thread, here us. Pulls
/// *leniently*, whatever is landing being a transition by definition; a caller
/// asserts [`pull`]'s steady-state coupling after this returns, not during it.
fn pull_until(src: &mut MixerPull, what: &str, mut done: impl FnMut() -> bool) {
    let started = Instant::now();
    let mut next_turn = started;
    while !done() {
        assert!(started.elapsed() < CONTROL_OP_BUDGET, "{what}");
        if Instant::now() < next_turn {
            // The op is runnable once the pull has serviced it, so give it a core: on a
            // contended runner an unyielding puller is what it waits behind.
            std::thread::yield_now();
            continue;
        }
        pull_lenient(src, frames_for_ms(WAIT_TURN_MS));
        next_turn += Duration::from_millis(WAIT_TURN_MS);
    }
}

/// Run a blocking deck control op on another thread while this one keeps the
/// mixer turning, since the op is waiting on the pulling this thread does.
fn drive_until<F>(src: &mut MixerPull, op: F)
where
    F: FnOnce() + Send + 'static,
{
    let handle = std::thread::spawn(op);
    pull_until(src, "control op never completed — deadlock?", || handle.is_finished());
    assert!(handle.join().is_ok(), "control op panicked");
}

/// The load-bearing property: `MixerPull` sums its voices unclamped, so two
/// overlapping decks must never push it past the amplitude either carries alone
/// — what a complementary linear curve buys and an equal-power one would not.
/// Checked across the *whole* overlap, warmup included.
fn assert_no_clipping(samples: &[f32], what: &str) {
    let peak = samples.iter().fold(0.0_f32, |a, s| a.max(s.abs()));
    assert!(
        peak <= AMPLITUDE * (1.0 + SKEW),
        "{what}: mixer summed to {peak}, past the {AMPLITUDE} each deck carries alone — the crossfade curve is not complementary"
    );
}

/// Assert every sample sits within `tol` of `expected`. A per-sample bound, so
/// a transient excursion can't hide behind an average.
fn assert_holds_at(samples: &[f32], expected: f32, tol: f32, what: &str) {
    assert!(!samples.is_empty(), "{what}: nothing sampled");
    let worst = samples.iter().fold(0.0_f32, |a, s| a.max((s - expected).abs()));
    assert!(
        worst <= tol,
        "{what}: expected ~{expected}, worst deviation {worst} (first few: {:?})",
        &samples[..samples.len().min(4)]
    );
}

struct Fixture {
    _tmp: tempfile::TempDir,
    track_a: String,
    track_b: String,
}

fn fixture() -> std::io::Result<Fixture> {
    let tmp = tempfile::tempdir()?;
    let a: PathBuf = tmp.path().join("a.wav");
    let b: PathBuf = tmp.path().join("b.wav");
    write_dc_wav(&a, 6, AMPLITUDE, AMPLITUDE)?;
    write_dc_wav(&b, 6, AMPLITUDE, AMPLITUDE)?;
    Ok(Fixture {
        _tmp: tmp,
        track_a: a.to_string_lossy().into_owned(),
        track_b: b.to_string_lossy().into_owned(),
    })
}

/// `PlaybackEngine` + the puller its two decks feed.
///
/// The `Mixer` itself is dropped on the way out: the decks are reference-counted, so `Decks` keeps
/// the two it took and the puller keeps the voices behind them.
fn player() -> std::io::Result<(Arc<PlaybackEngine>, MixerPull)> {
    let (mixer, pull) = mixer::pair(DECK_COUNT, shape(CHANNELS, RATE));
    let player = PlaybackEngine::new(&mixer, tokio::runtime::Handle::current())
        .map_err(std::io::Error::other)?;
    Ok((Arc::new(player), pull))
}

fn start(engine: &PlaybackEngine, path: &str) {
    let r = engine.play_media(path, 1.0, 1.0, None, TrackReplayGain::default());
    assert!(r.is_ok(), "play_media failed: {:?}", r.err());
}

fn crossfade_into(engine: &PlaybackEngine, path: &str) {
    let r = engine.begin_crossfade(path, TrackReplayGain::default(), FADE_MS, 1.0, 1.0);
    assert!(r.is_ok(), "begin_crossfade failed: {:?}", r.err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn crossfade_overlaps_two_decks_without_ever_clipping_the_mixer() -> std::io::Result<()> {
    let fx = fixture()?;
    let (engine, mut mix) = player()?;
    start(&engine, &fx.track_a);

    // Deck A alone, at full amplitude.
    let solo = pull(&mut mix, WARMUP_FRAMES * 2);
    assert_holds_at(&solo[WARMUP_FRAMES..], AMPLITUDE, 1e-3, "deck A alone");
    assert!(!engine.is_crossfading());

    crossfade_into(&engine, &fx.track_b);
    assert!(engine.is_crossfading(), "the outgoing deck is still draining");

    // Two correlated DC signals under complementary linear ramps must sum to a
    // constant: never above, the mixer not clamping, and never meaningfully below.
    let during = pull(&mut mix, frames_for_ms(FADE_MS));
    assert_no_clipping(&during, "auto crossfade");
    assert_holds_at(&during[WARMUP_FRAMES..], AMPLITUDE, AMPLITUDE * SKEW, "mid-overlap sum");

    // Past the ramp the outgoing deck has ended itself, which is the backend's
    // signal that the overlap is over.
    let after = pull(&mut mix, WARMUP_FRAMES * 2);
    assert!(!engine.is_crossfading(), "outgoing deck must drain when its ramp lands");
    assert_holds_at(&after[WARMUP_FRAMES..], AMPLITUDE, 1e-3, "deck B alone");

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_visualizer_tap_reads_the_mix_not_the_two_decks_interleaved() -> std::io::Result<()> {
    /// A spectrum window's worth — 46 ms at this rate.
    const WINDOW: usize = 2_048;

    let fx = fixture()?;
    let (engine, mut mix) = player()?;
    // The backend's own spelling; the Now-Playing view reaches the same cell
    // through `visualizer()`, one line down.
    engine.set_visualizer_enabled(true);
    let viz = engine.visualizer();
    let mut window = vec![0.0; WINDOW];

    start(&engine, &fx.track_a);
    pull(&mut mix, WARMUP_FRAMES * 2);

    viz.snapshot(&mut window);
    assert_holds_at(&window, AMPLITUDE, 1e-3, "one deck playing");

    crossfade_into(&engine, &fx.track_b);
    // A quarter of the way in, so the two ramps are far apart.
    pull(&mut mix, frames_for_ms(FADE_MS / 4));
    viz.snapshot(&mut window);
    // The same property the mixer output above is pinned on, which is the point:
    // the tap must see what the mixer sees. Interleaved into one ring instead,
    // this window would alternate between the two ramps sample by sample and
    // average half the level — which a per-sample bound cannot hide.
    assert_holds_at(&window, AMPLITUDE, AMPLITUDE * SKEW, "mid-overlap tap");

    // The handover: the outgoing ramp lands, its source ends and releases the
    // ring, and the survivor carries the window alone.
    pull(&mut mix, frames_for_ms(FADE_MS) + WARMUP_FRAMES * 2);
    assert!(!engine.is_crossfading(), "outgoing deck must drain when its ramp lands");
    viz.snapshot(&mut window);
    assert_holds_at(&window, AMPLITUDE, 1e-3, "after the overlap");

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn seeking_mid_crossfade_drops_the_outgoing_deck_and_restores_unity() -> std::io::Result<()> {
    let fx = fixture()?;
    let (engine, mut mix) = player()?;
    start(&engine, &fx.track_a);
    let _ = pull(&mut mix, WARMUP_FRAMES);

    crossfade_into(&engine, &fx.track_b);

    // Land mid-overlap, where the incoming deck sits at ~half gain.
    let _ = pull(&mut mix, frames_for_ms(FADE_MS) / 2);

    // The abort clears the live outgoing deck, which blocks on the audio thread, and that is us.
    // The survivor is whatever `crossfade_into` brought in, so that is the file the seek rebuilds.
    let r = engine.clone();
    let survivor = fx.track_b.clone();
    drive_until(&mut mix, move || r.seek(&survivor, 0, TrackReplayGain::default()));
    assert!(!engine.is_crossfading(), "a seek must abort the crossfade");

    // The survivor ramps back to unity from its partial fade-in gain. Had the
    // abort left it stranded, this would sit low.
    let after = pull(&mut mix, WARMUP_FRAMES * 2);
    assert_holds_at(&after[WARMUP_FRAMES..], AMPLITUDE, 1e-3, "surviving deck after abort");

    Ok(())
}

/// A demuxer seek lands on a packet boundary, so the decoder resumes at the start of a frame —
/// but the deck's converter takes whole frames off the source and never re-syncs. If anything in
/// the chain hands back a different number of samples than it owed, every frame after that
/// straddles two source frames and the stereo image is swapped for the rest of the track.
///
/// Only the mixer can see it. Below the converter every sample is correct and in order, which is
/// why a unit test on either stage passes either way — the same argument `tests/stream_rate.rs`
/// makes for the resampler.
///
/// Distinct per-channel amplitudes are the whole fixture: the DC pair the other cases use carries
/// one value in both channels, where a swap is invisible. Several seeks rather than one for the
/// breadth — each lands on a different offset into its packet, and shearing the frame once is
/// enough to swap the image for the rest of the track.
fn assert_seeking_keeps_the_image(eq_on: bool) -> std::io::Result<()> {
    const LEFT: f32 = 0.5;
    const RIGHT: f32 = 0.25;
    const SEEKS: u64 = 8;
    /// Any peaking band leaves a DC fixture alone — an RBJ peaking EQ is unity at zero
    /// frequency whatever its gain — so this buys the active path without moving what is
    /// asserted.
    const BAND: usize = 5;

    let tmp = tempfile::tempdir()?;
    let wav = tmp.path().join("stereo.wav");
    write_dc_wav(&wav, 6, LEFT, RIGHT)?;
    let path = wav.to_string_lossy().into_owned();

    let (engine, mut mix) = player()?;
    if eq_on {
        engine.set_eq_enabled(true);
        engine.set_eq_band(BAND, 6.0);
    }
    start(&engine, &path);
    pull_lenient(&mut mix, WARMUP_FRAMES);

    for step in 0..SEEKS {
        // Spread across a frame's worth of sample offsets, so the landings are not all the same
        // parity. Well inside the six-second fixture.
        let position_ms = 500 + step * 37;
        let r = engine.clone();
        let seeked = path.clone();
        drive_until(&mut mix, move || r.seek(&seeked, position_ms, TrackReplayGain::default()));
        pull_lenient(&mut mix, WARMUP_FRAMES);

        for (left, right) in pull_stereo(&mut mix, WARMUP_FRAMES) {
            assert!(
                (left - LEFT).abs() < 1e-3 && (right - RIGHT).abs() < 1e-3,
                "seek to {position_ms} ms left the image swapped: {left} / {right}"
            );
        }
    }

    Ok(())
}

/// `FileDecoder::try_seek` carries the channel restoration rodio's own decoder did, and `EqSource`
/// keeps `frame_phase` rather than zeroing it, so the two agree on where in the frame playback
/// resumed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn seeking_never_swaps_the_stereo_image() -> std::io::Result<()> {
    assert_seeking_keeps_the_image(false)
}

/// The same, on the path that buffers a whole frame before handing any of it out. It keeps its own
/// count of how far through that frame it is, which is a second place a seek could leave the chain
/// a sample short — and the bypass case above cannot see it, buffering nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn seeking_never_swaps_the_stereo_image_through_the_eq() -> std::io::Result<()> {
    assert_seeking_keeps_the_image(true)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_manual_crossfade_also_holds_the_amplitude() -> std::io::Result<()> {
    // With `crossfade_manual` on, `play_media` fades rather than cuts. It clears
    // the *idle* target deck first, which doesn't block.
    let fx = fixture()?;
    let (engine, mut mix) = player()?;
    engine.set_crossfade_enabled(true);
    engine.set_crossfade_manual(true);
    engine.set_crossfade_duration_ms(u32::try_from(FADE_MS).unwrap_or(2_000));

    start(&engine, &fx.track_a);
    let solo = pull(&mut mix, WARMUP_FRAMES * 2);
    assert_holds_at(&solo[WARMUP_FRAMES..], AMPLITUDE, 1e-3, "deck A alone");

    // Now a "manual next" onto the other deck.
    let r = engine.clone();
    let path = fx.track_b.clone();
    drive_until(&mut mix, move || start(&r, &path));
    assert!(engine.is_crossfading(), "a manual track change must fade, not cut");

    let during = pull(&mut mix, frames_for_ms(FADE_MS));
    assert_no_clipping(&during, "manual crossfade");
    assert_holds_at(
        &during[WARMUP_FRAMES..],
        AMPLITUDE,
        AMPLITUDE * SKEW,
        "manual mid-overlap sum",
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_plain_play_is_transparent() -> std::io::Result<()> {
    // Crossfade off must leave the signal exactly as decoded — the fade cell is
    // idle, so `EqSource` keeps its bypass path. The mixer's own rate/channel
    // adapter sits in the way, so compare against the decoded PCM value rather
    // than bit-for-bit.
    let fx = fixture()?;
    let (engine, mut mix) = player()?;
    start(&engine, &fx.track_a);

    let out = pull(&mut mix, WARMUP_FRAMES * 2);
    let expected = f32::from(pcm_sample(AMPLITUDE)) / 32_768.0;
    assert_holds_at(&out[WARMUP_FRAMES..], expected, 1e-6, "bypass passthrough");
    assert!(!engine.is_crossfading());
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stopping_a_paused_deck_clears_it_immediately() -> std::io::Result<()> {
    // `player_stop` passes the pause-fade length whatever the player is doing,
    // so a stop routinely lands on an already-paused deck — which is never
    // pulled, so a ramp armed on it never advances. Deferring the clear behind
    // it would leave the decks loaded while the UI already reads Stopped.
    let fx = fixture()?;
    let (engine, mut mix) = player()?;
    start(&engine, &fx.track_a);
    let _ = pull(&mut mix, WARMUP_FRAMES);

    // `pause()` is the immediate one, so the deck is genuinely paused with no
    // deferred op pending — the state under test. Flush the boundary so what
    // follows runs against steady silence.
    engine.pause();
    pull_lenient(&mut mix, WARMUP_FRAMES);

    let stopper = Arc::clone(&engine);
    drive_until(&mut mix, move || {
        stopper.stop_with_fade(netodia_playback::player::playback::crossfade::PAUSE_FADE_MS);
    });

    assert_eq!(
        engine.check_playback_state(),
        netodia_engine::player::engine::backend::PlaybackCheck::EndOfStream,
        "the decks must be cleared by the time `stop_with_fade` returns, not \
         behind a deferred clear waiting on a ramp that can never advance"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_zero_length_pause_fade_silences_the_deck_at_once() -> std::io::Result<()> {
    // Next / previous pressed *while paused* emit `PlayMedia` — which starts the
    // deck — then `Pause { fade_ms: 0 }` purely to restore the paused state. A
    // fade there would ramp the freshly-started track down from full volume
    // rather than pausing it, making its first quarter-second audible.
    let fx = fixture()?;
    let (engine, mut mix) = player()?;
    // Fade-on-pause ON, so the length passed in is the only thing that can make
    // this immediate — the backend must not reach for the setting itself.
    engine.set_crossfade_fade_on_pause(true);
    start(&engine, &fx.track_a);
    let _ = pull(&mut mix, WARMUP_FRAMES);

    engine.pause_with_fade(0);
    // The pause is serviced on the next pull rather than at the call, so flush a
    // generous window first.
    pull_lenient(&mut mix, frames_for_ms(20));

    // Well inside a `PAUSE_FADE_MS` ramp, which would still be near full volume
    // across all of this — so silence here is the whole point.
    let after = pull(&mut mix, frames_for_ms(50));
    assert_holds_at(&after, 0.0, 1e-4, "a zero-length pause fade");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stopping_with_a_gapless_track_staged_clears_both_decks_at_once() -> std::io::Result<()> {
    // A staged gapless source shares its deck's fade cell, so a fade-out armed
    // there would be inherited the moment the outgoing source drained — starting
    // at full volume and audibly fading out. `can_fade_out` refuses while one is
    // staged, and the immediate stop takes the staged source with it.
    //
    // The alternative — arming the fade and clearing `gapless_pending` eagerly —
    // leaves the flag lying for the length of the ramp: a `play_media` landing
    // there reads "nothing staged", takes the manual-crossfade branch that
    // clears only the *idle* deck, and strands the staged source behind an
    // outgoing track armed to self-end.
    let fx = fixture()?;
    let (engine, mut mix) = player()?;
    engine.set_crossfade_fade_on_pause(true);
    start(&engine, &fx.track_a);
    let _ = pull(&mut mix, WARMUP_FRAMES);

    engine.preload_gapless(Some(&fx.track_b), TrackReplayGain::default());
    assert!(engine.is_gapless_preloaded(), "the next track must really be staged");

    let stopper = Arc::clone(&engine);
    drive_until(&mut mix, move || {
        stopper.stop_with_fade(netodia_playback::player::playback::crossfade::PAUSE_FADE_MS);
    });

    // The load-bearing one: `EndOfStream` needs the active deck genuinely
    // *empty*, so it holds only if the staged source went with the outgoing one.
    // Clearing `gapless_pending` alone leaves the deck two sources deep and
    // still reporting `Playing`.
    assert_eq!(
        engine.check_playback_state(),
        netodia_engine::player::engine::backend::PlaybackCheck::EndOfStream,
        "both decks — staged source included — must be cleared by the time \
         `stop_with_fade` returns, not behind a deferred clear"
    );
    assert!(!engine.is_gapless_preloaded(), "and the flag must agree");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pausing_with_a_gapless_track_staged_is_immediate() -> std::io::Result<()> {
    // Same gate as the stop above, and the same reason: a fade here holds the
    // deck at zero gain while the outgoing source drains into the staged one,
    // which then inherits the ramp and burns its own first quarter-second.
    let fx = fixture()?;
    let (engine, mut mix) = player()?;
    engine.set_crossfade_fade_on_pause(true);
    start(&engine, &fx.track_a);
    let _ = pull(&mut mix, WARMUP_FRAMES);

    engine.preload_gapless(Some(&fx.track_b), TrackReplayGain::default());
    assert!(engine.is_gapless_preloaded(), "the next track must really be staged");

    engine.pause_with_fade(netodia_playback::player::playback::crossfade::PAUSE_FADE_MS);
    pull_lenient(&mut mix, frames_for_ms(20));

    // A ramp would still be near full volume this early, so silence is the tell.
    let after = pull(&mut mix, frames_for_ms(50));
    assert_holds_at(&after, 0.0, 1e-4, "a pause with a gapless track staged");

    // A pause keeps the deck contents, so the staged source and the flag must
    // both survive it.
    assert!(engine.is_gapless_preloaded(), "a pause must not discard the staged source");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_deferred_clear_takes_a_late_preload_with_it() -> std::io::Result<()> {
    // `can_fade_out` only refuses a fade for a source staged *before* it looks. A
    // preload entering after `stop_with_fade`'s epoch bump snapshots the new
    // epoch, passes its own re-check and really does stage while the ramp is in
    // flight — the monitor calling `preload_gapless` off the `exec_lock` that
    // serializes every other control op. The deferred clear is `stop()`'s
    // deferred half, so it must take that source *and* the flag; leaving the
    // flag set over empty decks reads as a `GaplessTransition` that never
    // happened and advances the queue out of a stopped player.
    let fx = fixture()?;
    let (engine, mut mix) = player()?;
    engine.set_crossfade_fade_on_pause(true);
    start(&engine, &fx.track_a);
    let _ = pull(&mut mix, WARMUP_FRAMES);

    // Nothing staged yet, so the fade is allowed and the clear really defers.
    engine.stop_with_fade(netodia_playback::player::playback::crossfade::PAUSE_FADE_MS);
    engine.preload_gapless(Some(&fx.track_b), TrackReplayGain::default());
    assert!(
        engine.is_gapless_preloaded(),
        "a preload past the epoch bump must still be able to stage — that is the \
         window under test"
    );

    // ⚠ Poll the lock-free flag only. `check_playback_state` takes the decks
    // lock, which the deferred task holds while `Player::clear()` waits on the
    // audio thread — this one — so reaching for it here deadlocks the test. The
    // flag is stored *after* the clear for the same reason.
    pull_until(&mut mix, "the deferred clear never landed", || !engine.is_gapless_preloaded());

    assert_eq!(
        engine.check_playback_state(),
        netodia_engine::player::engine::backend::PlaybackCheck::EndOfStream,
        "the deferred clear must empty both decks, staged source included"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_preload_that_outlives_the_stop_it_raced_is_refused() -> std::io::Result<()> {
    // The mirror of the test above, and the half the deferred clear's flag-drop
    // can't reach: neither `stop()` nor `DeferredOp::ClearAll` bumps the deck
    // epoch, so a preload that snapshots it, decodes slowly, and reaches the deck
    // lock *after* the stop emptied the decks passes its own re-check, appends
    // behind nothing and re-sets `gapless_pending` over decks the stop cleared.
    // `preload_gapless` refuses an empty active deck for exactly that reason.
    //
    // Calling the preload after `stop()` returns *is* that race — the epoch it
    // reads is the post-bump one either way, which is why the epoch cannot be
    // what catches this.
    let fx = fixture()?;
    let (engine, mut mix) = player()?;
    start(&engine, &fx.track_a);
    let _ = pull(&mut mix, WARMUP_FRAMES);

    // `stop()` clears a live deck, so it blocks on the audio thread — us.
    let stopper = Arc::clone(&engine);
    drive_until(&mut mix, move || stopper.stop());

    engine.preload_gapless(Some(&fx.track_b), TrackReplayGain::default());

    assert!(
        !engine.is_gapless_preloaded(),
        "a preload behind an emptied deck must be refused, not staged"
    );
    assert_eq!(
        engine.check_playback_state(),
        netodia_engine::player::engine::backend::PlaybackCheck::EndOfStream,
        "and the decks must stay empty — a source staged here would read as a \
         `GaplessTransition` off a stopped player"
    );
    Ok(())
}

/// An output reopen reshapes the puller between two streams and touches nothing above it, so a
/// crossfade caught halfway has to finish as if the device had never moved: the sum still flat,
/// the outgoing deck still draining on its own ramp, and the clock still counting media time.
///
/// Rate only, because the channel count is what [`pull`] reads a frame by. The ratio is the
/// half of this a reopen can get wrong: a converter left stepping at the old one would play the
/// rest of the fade pitched, which the position below would show as time running fast or slow.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_crossfade_carries_on_through_an_output_reopen() -> std::io::Result<()> {
    /// Device time pulled after the reshape, over which the clock has to advance as much media.
    const MEASURED_MS: u64 = FADE_MS / 4;
    /// Both reads floor to a whole millisecond, and the converter carries a fraction of a frame.
    const CLOCK_SLACK_MS: u64 = 2;

    let fx = fixture()?;
    let (engine, mut mix) = player()?;
    start(&engine, &fx.track_a);
    let _ = pull(&mut mix, WARMUP_FRAMES * 2);

    crossfade_into(&engine, &fx.track_b);
    let before_reopen = pull(&mut mix, frames_for_ms(FADE_MS / 2));
    assert_no_clipping(&before_reopen, "first half of the fade");

    mix.reshape(shape(CHANNELS, REOPENED_RATE));

    let started_at = engine.query_position();
    let after_reopen = pull(&mut mix, frames_at(REOPENED_RATE, MEASURED_MS));
    let advanced = engine.query_position() - started_at;
    assert_no_clipping(&after_reopen, "the fade after a reopen");
    assert_holds_at(&after_reopen, AMPLITUDE, AMPLITUDE * SKEW, "mid-overlap sum after a reopen");
    assert!(
        advanced.abs_diff(MEASURED_MS) <= CLOCK_SLACK_MS,
        "{MEASURED_MS} ms at the new rate moved the clock {advanced} ms: the converter is stepping at the old ratio"
    );

    pull_lenient(&mut mix, frames_at(REOPENED_RATE, FADE_MS / 4) + WARMUP_FRAMES);
    let after_fade = pull(&mut mix, frames_at(REOPENED_RATE, 200));
    assert!(!engine.is_crossfading(), "the outgoing deck must still drain when its ramp lands");
    assert_holds_at(&after_fade, AMPLITUDE, 1e-3, "deck B alone after a reopen");
    Ok(())
}

/// A gapless successor is staged *on the voice*, behind the source playing there, so a reopen
/// that rebuilt voices or dropped what they queue would lose it and turn the handover into an end
/// of stream. It has to survive the reshape and take over with no gap, from its own top.
///
/// The two tracks sit at different levels so the handover can be seen, and a frame of silence
/// between them is the one thing the boundary window may not contain.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_staged_gapless_track_takes_over_through_an_output_reopen() -> std::io::Result<()> {
    const NEXT_LEVEL: f32 = AMPLITUDE / 2.0;
    /// The reopened rate puts the seam through the band-limited kernel, which rings on the step
    /// between the two levels by up to the Gibbs overshoot of a sharp filter. A frame of silence
    /// dips almost to zero, far past that.
    const RINGING: f32 = (AMPLITUDE - NEXT_LEVEL) * 0.09;
    /// How far into the successor the pull runs past the first track's one second.
    const INTO_NEXT_MS: u64 = 500;
    /// The handover lands inside a lockstep step, and both position reads floor.
    const CLOCK_SLACK_MS: u64 = 5;

    let tmp = tempfile::tempdir()?;
    let first = tmp.path().join("first.wav");
    let next = tmp.path().join("next.wav");
    write_dc_wav(&first, 1, AMPLITUDE, AMPLITUDE)?;
    write_dc_wav(&next, 6, NEXT_LEVEL, NEXT_LEVEL)?;

    let (engine, mut mix) = player()?;
    start(&engine, &first.to_string_lossy());
    let _ = pull(&mut mix, WARMUP_FRAMES);
    engine.preload_gapless(Some(&next.to_string_lossy()), TrackReplayGain::default());
    assert!(engine.is_gapless_preloaded(), "the next track must really be staged");

    let played_ms = engine.query_position();
    mix.reshape(shape(CHANNELS, REOPENED_RATE));

    let remaining_ms = 1_000 - played_ms;
    let across = pull(&mut mix, frames_at(REOPENED_RATE, remaining_ms + INTO_NEXT_MS));
    let quietest = across.iter().fold(f32::MAX, |low, s| low.min(*s));
    assert!(
        quietest >= NEXT_LEVEL - RINGING,
        "the handover after a reopen dipped to {quietest}: a gap between the two tracks"
    );
    assert_eq!(
        engine.check_playback_state(),
        netodia_engine::player::engine::backend::PlaybackCheck::GaplessTransition,
        "the staged track must take over the voice rather than the deck running dry"
    );

    let settled = pull(&mut mix, frames_at(REOPENED_RATE, 100));
    assert_holds_at(&settled, NEXT_LEVEL, 1e-3, "the successor after a reopen");
    let position = engine.query_position();
    assert!(
        position.abs_diff(INTO_NEXT_MS + 100) <= CLOCK_SLACK_MS,
        "the successor reads {position} ms, not its own top plus what was pulled of it"
    );
    Ok(())
}
