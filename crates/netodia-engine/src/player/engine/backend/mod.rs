//! The playback engine: the decks, the epoch that orders every op against them, and the transport
//! the action layer drives.
//!
//! **Everything here races something else here, and that is the reason it is one file.** The
//! transport ops, the fade gates, the gapless stage and the deferred pause/stop
//! carry an argument about one of the others, over the decks lock or [`PlaybackEngine::deck_epoch`], and those read as
//! arguments only while they sit next to each other. What moved out races nothing: the lock-free
//! DSP setters ([`controls`]), the decks' read-only answers ([`reads`]), the trait a test mocks
//! ([`player_backend`]), and the device under the decks ([`output`]), which a transport op here
//! calls into with the decks lock already held.

mod controls;
mod output;
mod player_backend;
mod reads;

pub use output::OutputChoice;
pub use player_backend::PlayerBackend;

use output::EngineOutput;

use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, MutexGuard};
use std::time::Duration;

use netodia_audio::player::source::audio::AudioSource;
use netodia_audio::player::source::buffered::BufferedAudioSource;
use netodia_audio::player::source::file_decode::FileDecoder;
use netodia_core::error::AppError;
use netodia_core::error::describe;
use netodia_playback::player::playback::crossfade::{self, CrossfadeShared};
use netodia_playback::player::playback::decks::{Deck, Decks, DeferredOp, lock_decks};
use netodia_playback::player::playback::equalizer::{self, EqShared, EqSource};
use netodia_playback::player::playback::output::AudioOutput;
use netodia_playback::player::playback::output::mixer::Mixer;
use netodia_playback::player::playback::replaygain::{ReplayGainShared, TrackReplayGain};
use netodia_playback::player::playback::visualizer::{VisualizerShared, VisualizerTap};

/// Result of checking what is on the active deck in a single lock acquisition.
#[derive(Debug, PartialEq)]
pub enum PlaybackCheck {
    /// The active voice dropped from two sources to one — the staged one took over.
    GaplessTransition,
    EndOfStream,
    Playing,
}

/// Pure half of [`PlaybackEngine::check_playback_state`], split out for testability.
///
/// `sources` is what the active voice still holds; the caller reads it once, under one lock.
pub fn evaluate_playback_check(was_gapless: bool, sources: usize) -> PlaybackCheck {
    if was_gapless && sources <= 1 {
        return PlaybackCheck::GaplessTransition;
    }
    if sources == 0 {
        return PlaybackCheck::EndOfStream;
    }
    PlaybackCheck::Playing
}

/// How a track joins the decks.
#[derive(Clone, Copy)]
enum Entry {
    /// On the idle deck, over this many media milliseconds, as the playing track fades out.
    Fade(u64),
    /// Alone, from this far into the track.
    Cut(Duration),
}

pub struct PlaybackEngine {
    // The device the decks play into. First so it drops first: the stream is what stops the
    // callback.
    output: EngineOutput,
    // The mutex is what makes a multi-op sequence atomic (a `Deck` is already
    // Send+Sync on its own); `Arc` so a deferred pause/stop can hold the decks
    // after its sleep.
    decks: Arc<std::sync::Mutex<Decks>>,
    // `Arc` because the deferred clear of a faded stop must drop this flag
    // alongside the deck contents it removes.
    gapless_pending: Arc<AtomicBool>,
    // Set while a crossfade's outgoing deck is still draining. Paired with the
    // idle deck's `empty()` in `is_crossfading`, which self-heals the flag.
    crossfade_armed: AtomicBool,
    // Bumped by every control op that replaces deck contents, so a newer op
    // always wins. Its two readers — a deferred pause/stop after its sleep, and
    // a `preload_gapless` after its unlocked decode — re-check it *under the
    // decks lock* and bail if it moved.
    deck_epoch: Arc<AtomicU64>,
    // Lock-free EQ / ReplayGain master state, shared by every `EqSource` we
    // append so a live change reaches the playing *and* the preloaded track.
    // The per-track RG gain is baked per source, not held here. Both are seeded
    // at boot from persisted settings.
    eq: Arc<EqShared>,
    rg: Arc<ReplayGainShared>,
    // Read by the control layer only — never by the audio thread, hence no
    // generation counter.
    xf: Arc<CrossfadeShared>,
    // One sample ring per deck, written by every source we append and read by
    // the UI as their sum — which is what makes a crossfade read as the mix
    // rather than as two interleaved tracks. Unlike the cells above it is *not*
    // seeded at boot: it stays disarmed (a no-op) until the Now-Playing view is
    // on screen, so the audio thread never fills a ring nobody reads.
    viz: Arc<VisualizerShared>,
    // Only ever schedules the deferred half of a faded pause / stop.
    runtime: tokio::runtime::Handle,
    // Orders network/decode jobs before they finish opening. A late source from an older request
    // must not replace the track selected by a newer action.
    playback_generation: AtomicU64,
}

impl PlaybackEngine {
    /// The engine over a mixer with no device behind it, which whoever built the mixer pulls.
    ///
    /// # Errors
    ///
    /// [`AppError::Player`] if `mixer` carries fewer voices than the decks need.
    pub fn new(mixer: &Mixer, runtime: tokio::runtime::Handle) -> Result<Self, AppError> {
        Ok(Self {
            output: EngineOutput::default(),
            decks: Arc::new(std::sync::Mutex::new(Decks::connect(mixer)?)),
            gapless_pending: Arc::new(AtomicBool::new(false)),
            crossfade_armed: AtomicBool::new(false),
            deck_epoch: Arc::new(AtomicU64::new(0)),
            eq: EqShared::new(false, &[0.0; equalizer::NUM_BANDS]),
            rg: ReplayGainShared::new(),
            xf: CrossfadeShared::new(),
            viz: VisualizerShared::new(false),
            runtime,
            playback_generation: AtomicU64::new(0),
        })
    }

    /// The engine playing into `output`, which it keeps so it can reopen it.
    ///
    /// # Errors
    ///
    /// [`AppError::Player`] if `output` carries fewer voices than the decks need.
    pub fn with_output(
        output: AudioOutput,
        runtime: tokio::runtime::Handle,
    ) -> Result<Self, AppError> {
        let mut engine = Self::new(output.mixer(), runtime)?;
        engine.output = EngineOutput::over(output);
        Ok(engine)
    }

    /// Invalidate any deferred pause/stop *and* any in-flight gapless preload,
    /// returning the new epoch. Every op that replaces deck contents calls this.
    fn bump_epoch(&self) -> u64 {
        self.deck_epoch.fetch_add(1, Ordering::AcqRel) + 1
    }

    /// Reserve a playback request before opening or decoding its source on a worker thread.
    ///
    /// Pass the returned generation to [`Self::play_prepared_source`]. A later reservation,
    /// pause, or stop makes an older result a no-op when it eventually finishes loading.
    pub fn reserve_playback_request(&self) -> u64 {
        let _decks = self.lock_decks();
        self.playback_generation.fetch_add(1, Ordering::AcqRel) + 1
    }

    /// Whether an async source resolution still owns the latest playback request slot.
    pub fn is_playback_request_current(&self, generation: u64) -> bool {
        self.playback_generation.load(Ordering::Acquire) == generation
    }

    /// Run `op` after `delay_ms`, unless a newer control op bumped the epoch in
    /// the meantime. Re-checked *under the decks lock* so a concurrent
    /// `play_media` can't be clobbered by a clear that passed a lock-free check.
    fn schedule_after(&self, epoch: u64, delay_ms: u64, op: DeferredOp) {
        let decks = self.decks.clone();
        let deck_epoch = self.deck_epoch.clone();
        let gapless_pending = self.gapless_pending.clone();
        let output = Arc::clone(&self.output.device);
        self.runtime.spawn(async move {
            tokio::time::sleep(Duration::from_millis(delay_ms)).await;
            // The wait is async and the landing is not: `Deck::clear` blocks until the audio
            // callback services it, twice over for `clear_all`, so it goes to the blocking pool
            // rather than parking an async worker for two device periods — or, against a card that
            // has stopped asking for samples, for `output::voice::SERVICE_TIMEOUT` apiece. Sleeping
            // there instead would hold a pool slot for the whole fade, which is far longer.
            tokio::task::spawn_blocking(move || {
                let guard = lock_decks(&decks);
                if deck_epoch.load(Ordering::Acquire) != epoch {
                    return;
                }
                match op {
                    // A pause keeps the deck contents — staged gapless source
                    // included — so it must leave `gapless_pending` alone.
                    DeferredOp::PauseAll => guard.pause_all(),
                    // The deferred half of `stop()`, so it drops the flag the same
                    // way: *after* the decks, never before. A preload that entered
                    // behind the epoch bump can still have staged a source onto the
                    // deck this throws away, and clearing here is what stops the
                    // flag outliving it; one arriving after is refused by
                    // `preload_gapless`'s empty-deck gate.
                    DeferredOp::ClearAll => {
                        guard.clear_all();
                        gapless_pending.store(false, Ordering::Release);
                        output::release_exclusive(&output);
                    }
                }
            });
        });
    }

    /// Is the active deck actually playing something we could fade out of? See
    /// [`Deck::busy`] for why a paused or empty deck can't carry a ramp.
    fn active_deck_busy(&self) -> bool {
        self.lock_decks().active().busy()
    }

    /// The shared gate of [`Self::pause_with_fade`] and [`Self::stop_with_fade`];
    /// each term falls through to the immediate op:
    ///
    /// - **`fade_ms == 0`** — checked first, so `stop_end_of_queue` never takes
    ///   the deck lock at all.
    /// - **an idle deck** — `player_stop` passes the pause-fade length whatever
    ///   the status, so a stop routinely lands on a paused deck.
    /// - **a staged gapless source** — it shares this deck's fade cell and would
    ///   inherit the ramp the moment the outgoing source drained. Same gate, and
    ///   reason, as [`crossfade::manual_fade_ms`].
    /// - **a crossfade in flight** — the outgoing ramp has no start gain to
    ///   restore on resume; `Pausable` just freezes both.
    fn can_fade_out(&self, fade_ms: u64) -> bool {
        fade_ms > 0
            && self.active_deck_busy()
            && !self.is_gapless_preloaded()
            && !self.is_crossfading()
    }

    /// Fade length for a *manual* track change, or `0` for a hard cut. Gathers
    /// the live inputs; [`crossfade::manual_fade_ms`] documents each gate.
    ///
    /// Only a wish: the `gapless_pending` read is advisory, the monitor's preload not being under
    /// the `exec_lock`, and whether the output reopens for the track is unknown until it is
    /// decoded. [`Self::start_track`] settles both under the deck lock, and the preload re-checks
    /// the epoch before staging, so whichever commits second yields.
    fn manual_fade_ms(&self, start_position_ms: Option<u64>) -> u64 {
        crossfade::manual_fade_ms(
            self.crossfade_settings(),
            start_position_ms.is_some(),
            self.active_deck_busy(),
            self.is_gapless_preloaded(),
        )
    }

    /// Start playback of a file, optionally seeking — all under one lock, so the
    /// playback monitor can't observe position ~0 between the play and the seek.
    pub fn play_media(
        &self,
        file_path: &str,
        volume: f64,
        speed: f64,
        start_position_ms: Option<u64>,
        baked_rg: TrackReplayGain,
    ) -> Result<(), AppError> {
        let generation = self.reserve_playback_request();
        let fade_ms = self.manual_fade_ms(start_position_ms);

        // Probe, seek and prefill outside the deck lock. After mounting, the callback only
        // pulls PCM that the decoder worker has already produced.
        let mut decoded = FileDecoder::open(Path::new(file_path))?;

        // One value for the decoder's seek and the deck's clock anchor both, so the two cannot
        // disagree about where this source begins.
        let mut start = start_position_ms.map_or(Duration::ZERO, Duration::from_millis);

        // Seeked on the source we still own, rather than through the deck once it is mounted: a
        // deck seek is serviced by the audio callback, so that spelling puts a demuxer scan on the
        // real-time thread and parks this one on the callback with the decks lock held. Same
        // reasoning as the hoisted open above, one step further.
        if !start.is_zero() {
            log::debug!("Resuming playback at {}ms", start.as_millis());
            if let Err(e) = decoded.try_seek(start) {
                log::warn!("Seek failed: {}", describe(&e));
                // The anchor goes with it: the clock counts frames handed out, and a source the
                // seek refused to move hands out its first one from wherever it still is.
                start = Duration::ZERO;
            }
        }

        // A manual fade is never asked for with a resume position, so the two can't both apply.
        let entry = if fade_ms > 0 { Entry::Fade(fade_ms) } else { Entry::Cut(start) };
        let source = BufferedAudioSource::from_positioned(decoded)?;
        self.start_track(source, baked_rg, entry, volume, speed, Some(generation));
        Ok(())
    }

    /// Play a source that another layer has already opened and positioned, including a buffered
    /// network decoder. Its reads must be nonblocking because the decks pull samples on the audio
    /// callback.
    pub fn play_prepared_source<S: AudioSource + 'static>(
        &self,
        generation: u64,
        source: S,
        volume: f64,
        speed: f64,
        start_position: Option<Duration>,
        baked_rg: TrackReplayGain,
    ) -> bool {
        let start_position_ms =
            start_position.map(|position| u64::try_from(position.as_millis()).unwrap_or(u64::MAX));
        let fade_ms = self.manual_fade_ms(start_position_ms);
        let start = start_position.unwrap_or(Duration::ZERO);
        let entry = if fade_ms > 0 { Entry::Fade(fade_ms) } else { Entry::Cut(start) };
        self.start_track(source, baked_rg, entry, volume, speed, Some(generation))
    }

    /// Put `decoded` on the decks as `entry` asks, cutting from the top where a fade can't go.
    ///
    /// Two things stop a fade, and neither is settled when the caller picks one: a gapless source
    /// staged since (its flag is set under this same lock), and an output that has to reopen for
    /// the track. A cut is always safe, clearing both decks with any staged source in them. A fade
    /// never reopens, so whatever a track start would reclaim waits for the next cut.
    fn start_track<S: AudioSource + 'static>(
        &self,
        decoded: S,
        baked_rg: TrackReplayGain,
        entry: Entry,
        volume: f64,
        speed: f64,
        generation: Option<u64>,
    ) -> bool {
        let mut decks = self.lock_decks();
        if generation.is_some_and(|generation| {
            self.playback_generation.load(Ordering::Acquire) != generation
        }) {
            return false;
        }
        self.bump_epoch();
        let fade_blocked = self.gapless_pending.load(Ordering::Acquire)
            || !self.plays_without_reopen(decoded.shape(), decoded.format());
        let entry = match entry {
            Entry::Fade(_) if fade_blocked => Entry::Cut(Duration::ZERO),
            entry => entry,
        };
        // The deck primitives hand the builder the *target* deck under the lock they append
        // through, so a source's ramp cell is its own deck's. `EqSource::new` does no I/O.
        match entry {
            Entry::Fade(fade_ms) => {
                let volume = self.route_volume(volume);
                decks.crossfade_to(fade_ms, volume, speed, |deck| {
                    self.build_source(decoded, baked_rg, deck)
                });
                self.crossfade_armed.store(true, Ordering::Release);
            }
            // Anchored where the decoder was seeked to, not at zero: the deck counts frames handed
            // out, and a resumed source hands out its first frame minutes in.
            Entry::Cut(start) => {
                self.cut_to_source(&decks, decoded, baked_rg, volume, speed, start);
            }
        }
        self.gapless_pending.store(false, Ordering::Release);
        true
    }

    /// Play `source` alone from `start`, reopening the output for it first.
    ///
    /// The decks are paused ahead of the reopen: it keeps the voices, so the new stream would play
    /// on from the outgoing source until the cut's clear lands.
    fn cut_to_source<S: AudioSource + 'static>(
        &self,
        decks: &MutexGuard<'_, Decks>,
        source: S,
        baked_rg: TrackReplayGain,
        volume: f64,
        speed: f64,
        start: Duration,
    ) {
        decks.pause_all();
        let volume = self.prepare_output_for(decks, &source, volume);
        decks.cut_to(volume, speed, start, |deck| self.build_source(source, baked_rg, deck));
        self.crossfade_armed.store(false, Ordering::Release);
    }

    /// Start `file_path` on the idle deck and cross-fade over `fade_ms` media
    /// milliseconds. The outgoing source ends itself when its ramp lands
    /// (`end_on_complete`), draining that deck — which is how
    /// [`Self::is_crossfading`] knows the overlap is over.
    ///
    /// A decode failure returns before any deck is touched, so the outgoing
    /// track keeps playing and the caller can fall back to a plain skip.
    ///
    /// A track the output has to reopen for is cut to instead, the state machine being on it
    /// already: the monitor found it plays without one, so that decision has gone stale.
    pub fn begin_crossfade(
        &self,
        file_path: &str,
        baked_rg: TrackReplayGain,
        fade_ms: u64,
        volume: f64,
        speed: f64,
    ) -> Result<(), AppError> {
        let generation = self.reserve_playback_request();
        // Decode outside the deck lock, as `play_media` does.
        let source =
            BufferedAudioSource::from_positioned(FileDecoder::open(Path::new(file_path))?)?;

        // A staged gapless source shares the active deck's fade cell and would fade out with the
        // track it follows. `crossfade_eligible` keeps the preload off for that, so one here is a
        // bug that `start_track`'s fallback cut would hide.
        debug_assert!(
            !self.gapless_pending.load(Ordering::Acquire),
            "crossfade must never race a staged gapless preload"
        );
        self.start_track(source, baked_rg, Entry::Fade(fade_ms), volume, speed, Some(generation));
        Ok(())
    }

    /// Wrap an audio source in what the decks actually play: the graphic EQ, this track's baked
    /// `ReplayGain` and `deck`'s ramp cell, under a visualizer tap writing into `deck`'s own ring.
    ///
    /// Generic over the source rather than over a reader, so the file decoder and DSP chain meet at
    /// [`AudioSource`]. Everything downstream is identical: EQ, limiter and visualizer all use the
    /// same processing path.
    ///
    /// Always called with the deck the source is about to be appended to — see
    /// [`netodia_playback::player::playback::decks`] for why the two can't be split. Building the
    /// tap also *claims* that ring for the life of the value and stamps its history away if the
    /// deck was idle, both only correct for a source about to play, so don't build one anywhere
    /// it might be held or discarded instead.
    fn build_source<S: AudioSource + 'static>(
        &self,
        input: S,
        baked_rg: TrackReplayGain,
        deck: &Deck,
    ) -> VisualizerTap<EqSource<S>> {
        VisualizerTap::new(
            EqSource::new(input, self.eq.clone(), self.rg.clone(), baked_rg, deck.fade.clone()),
            &self.viz,
            deck.viz_slot,
        )
    }

    /// Whether a crossfade's outgoing deck is still audible. Clearing the flag
    /// here once that deck drains doubles as the crossfade's completion hook —
    /// no `Drop` impl or extra atomic needed. Safe because `Deck::append` bumps
    /// the source count before it sends, so a just-fed deck never reports empty.
    pub fn is_crossfading(&self) -> bool {
        if !self.crossfade_armed.load(Ordering::Acquire) {
            return false;
        }
        let decks = self.lock_decks();
        if decks.idle().voice.is_empty() {
            drop(decks);
            self.crossfade_armed.store(false, Ordering::Release);
            return false;
        }
        true
    }

    /// Cancel an in-flight crossfade: drop the outgoing deck and return the survivor's cell to
    /// unity. Called before a seek, which would otherwise leave the new track stuck at partial
    /// gain — and, once the seek rebuilds that source, replay the whole fade-in from silence,
    /// the cell still holding the ramp `crossfade_to` armed.
    ///
    /// The rebuilt source starts *at* unity rather than gliding to it, having no gain of its own
    /// to resume from; [`crossfade::ABORT_RAMP_MS`] is the glide the seeks that bail still get.
    fn abort_crossfade(&self) {
        if !self.crossfade_armed.swap(false, Ordering::AcqRel) {
            return;
        }
        let decks = self.lock_decks();
        decks.idle().reset();
        decks.active().fade.arm(None, 1.0, crossfade::ABORT_RAMP_MS, false);
    }

    pub fn resume(&self) {
        self.bump_epoch();
        // A crossfade in flight owns both decks' ramps; re-arming the active
        // one here would restart its fade-in from silence.
        let crossfading = self.is_crossfading();
        let decks = self.lock_decks();
        decks.play_all();
        if !crossfading {
            // Armed unconditionally, not gated on the setting: a faded pause
            // leaves the deck holding silence and the setting may have been
            // turned off in between. A zero-length ramp snaps back to unity.
            let ramp = if self.xf.fade_on_pause() { crossfade::PAUSE_FADE_MS } else { 0 };
            decks.active().fade.arm(None, 1.0, ramp, false);
        }
    }

    /// Pause both decks now. The caller decides whether a fade is wanted; see
    /// [`Self::pause_with_fade`].
    pub fn pause(&self) {
        let decks = self.lock_decks();
        self.bump_epoch();
        self.playback_generation.fetch_add(1, Ordering::AcqRel);
        decks.pause_all();
    }

    /// Arm a fade to silence on the active deck and schedule `op` to land once
    /// the ramp has; `false` means refused and the caller runs the immediate op.
    ///
    /// The ramp holds at zero rather than ending its source — a self-ending fade
    /// would drain the active deck and read as `EndOfStream`.
    fn arm_fade_out(&self, fade_ms: u64, op: DeferredOp) -> bool {
        if !self.can_fade_out(fade_ms) {
            return false;
        }
        let epoch = self.bump_epoch();
        {
            let decks = self.lock_decks();
            // Backstop for the gapless race, mirroring `start_track`: the read in
            // `can_fade_out` is lock-free and the bump is a separate step, so a
            // preload can complete between the two and only this re-read sees
            // it. One arriving *after* the bump is benign — the pause path just
            // holds it at silence until the resume ramps back to unity, and the
            // stop path's deferred `ClearAll` takes source and flag together.
            if self.gapless_pending.load(Ordering::Acquire) {
                return false;
            }
            self.playback_generation.fetch_add(1, Ordering::AcqRel);
            decks.active().fade.arm(None, 0.0, fade_ms, false);
        }
        self.schedule_after(epoch, fade_ms, op);
        true
    }

    /// Fade to silence over `fade_ms`, then pause both decks.
    pub fn pause_with_fade(&self, fade_ms: u64) {
        if !self.arm_fade_out(fade_ms, DeferredOp::PauseAll) {
            self.pause();
        }
    }

    /// Stop playback. `Deck::clear` removes all sources and pauses automatically.
    pub fn stop(&self) {
        // Clearing the decks drops a live stream's source, whose `Drop` is what tells its feed
        // thread to close the connection; this only stops anyone still reading the cell.
        let decks = self.lock_decks();
        self.bump_epoch();
        self.playback_generation.fetch_add(1, Ordering::AcqRel);
        self.crossfade_armed.store(false, Ordering::Release);
        decks.clear_all();
        self.gapless_pending.store(false, Ordering::Release);
        output::release_exclusive(&self.output.device);
    }

    /// Fade to silence over `fade_ms`, then clear both decks via the deferred
    /// [`DeferredOp::ClearAll`].
    ///
    /// `gapless_pending` is deliberately *not* cleared here: the gate already
    /// refused the fade if a source was staged, so clearing eagerly could only
    /// start lying about one the deferred clear has yet to remove. A preload
    /// slipping in behind us is dropped with the flag by that `ClearAll`; one
    /// behind *the clear* finds an empty deck and is refused.
    pub fn stop_with_fade(&self, fade_ms: u64) {
        if !self.arm_fade_out(fade_ms, DeferredOp::ClearAll) {
            self.stop();
        }
    }

    /// Move the playing track to `position_ms`, by building a source already there.
    ///
    /// **The scan a seek costs happens here, on the caller's thread.** Moving the *mounted* source
    /// meant running the demuxer inside the audio callback — file I/O on the one thread that may
    /// not do any, and the caller parked on it meanwhile. So the file is opened and positioned
    /// first and the deck is handed the result, which leaves the callback a pointer swap.
    /// `file_path` and `baked_rg` ride the action for that reason; the gain is re-baked because
    /// the source carrying it is rebuilt.
    ///
    /// Costs a reopen, and a container with no seek index is walked from the top where the mounted
    /// decoder could have walked from where it was. That is the trade: it buys a seek that cannot
    /// glitch the audio, including the other deck's.
    ///
    /// **The deck is claimed before the open and the claim is checked after it**, because reading
    /// the file takes long enough for a gapless successor to take the deck over — which happens in
    /// the callback and so is under neither `exec_lock` nor [`Self::deck_epoch`]. Without the
    /// ticket a seek landing in that window mounts the track that just ended over the one that
    /// just started.
    ///
    /// The two lock acquisitions name the same voice because `Decks::crossfade_to` is the only
    /// thing that moves `active` and it runs under the `exec_lock` this executes under. A ticket
    /// read off one voice and compared against the other's counter would match by coincidence,
    /// both being small and equal early in a session.
    ///
    /// Deliberately does **not** bump the epoch: a seek replaces no deck *contents*, and
    /// cancelling a pending deferred pause would leave the decks running silently at the ramp's
    /// zero gain while the UI reads Paused. Nothing here touches `paused` either, so seeking a
    /// paused track leaves it paused.
    pub fn seek(&self, file_path: &str, position_ms: u64, baked_rg: TrackReplayGain) {
        self.abort_crossfade();

        // Nothing on the deck is nothing to seek, which is what moving the mounted source answered
        // too. Asked before the open so a seek against a drained deck costs no I/O.
        let mounted = {
            let decks = self.lock_decks();
            let voice = &decks.active().voice;
            if voice.is_empty() {
                return;
            }
            voice.mounted()
        };

        let position = Duration::from_millis(position_ms);
        let mut decoded = match FileDecoder::open(Path::new(file_path)) {
            Ok(decoded) => decoded,
            Err(e) => {
                log::warn!("Seek could not reopen the track: {}", describe(&e));
                return;
            }
        };
        if let Err(e) = decoded.try_seek(position) {
            log::warn!("Seek failed: {}", describe(&e));
            return;
        }

        let source = match BufferedAudioSource::from_positioned(decoded) {
            Ok(source) => source,
            Err(e) => {
                log::warn!("Seek could not buffer the track: {}", describe(&e));
                return;
            }
        };
        let decks = self.lock_decks();
        let deck = decks.active();
        deck.voice.replace(self.build_source(source, baked_rg, deck), position, mounted);
    }

    /// Both decks must run at the same speed or a crossfade would drift.
    ///
    /// **No re-anchoring seek.** rodio needed one because its position tracker sat after its speed
    /// stage, so a change rescaled the elapsed portion too and the slider jumped; the deck counts
    /// media frames the source actually handed over, which no later change to the ratio can
    /// retroactively alter.
    pub fn set_speed(&self, speed: f64) {
        self.lock_decks().set_speed_all(speed);
        // The tap sits *above* the converter, so it sees media-rate samples while the ear hears
        // them scaled: the analyzer needs the factor to place band edges on the pitch you hear.
        // Sole writer of that cell — see `VisualizerShared::set_speed`.
        self.viz.set_speed(speed);
    }

    /// Whether a gapless source is currently staged behind the playing one.
    /// Used by the playback monitor to avoid re-issuing the late preload each tick.
    pub fn is_gapless_preloaded(&self) -> bool {
        self.gapless_pending.load(Ordering::Acquire)
    }

    /// Stage the next track *behind* the current one on the **active** deck, so
    /// the deck sequences them back-to-back. Mutually exclusive with a
    /// crossfade for a given transition — see `crossfade::crossfade_eligible`.
    pub fn preload_gapless(&self, file_path: Option<&str>, baked_rg: TrackReplayGain) {
        let Some(path) = file_path else {
            // The action layer only ever *clears* a stale preload through here.
            self.gapless_pending.store(false, Ordering::Release);
            return;
        };

        // Snapshot before the unlocked decode: a mismatch below means the decks
        // moved out from under this preload, so staging anyway would queue the
        // wrong track *and*, if the active deck flipped, hand it the other
        // deck's ramp cell — possibly one armed to fade out and end.
        let epoch = self.deck_epoch.load(Ordering::Acquire);

        // Decode off the deck lock, as in `play_media` — a preload fires right
        // when a stall in position publication would be most visible.
        let decoded = match self.open_for_gapless(path) {
            Ok(Some(decoded)) => decoded,
            Ok(None) => return,
            Err(e) => {
                log::warn!("Failed to preload gapless track {path}: {}", describe(&e));
                self.gapless_pending.store(false, Ordering::Release);
                return;
            }
        };

        // Re-check and stage under one lock. `Deck::stage` takes the builder
        // rather than a source, for the reason the other two appends do.
        let decks = self.lock_decks();
        if self.deck_epoch.load(Ordering::Acquire) != epoch {
            log::debug!("Dropping stale gapless preload of {path}");
            return;
        }
        let deck = decks.active();
        // The epoch can't stand in for this: `stop()` and the deferred
        // [`DeferredOp::ClearAll`] empty the decks *without* bumping it, so a
        // preload whose decode outran either would pass the re-check, append
        // behind nothing, and leave `gapless_pending` set over an emptied deck —
        // which `check_playback_state` reads as a phantom `GaplessTransition`.
        // Never rejects a legitimate stage: the only caller passing a path is
        // the monitor's late preload, from its `Playing` branch, and
        // `start_track` appends under this same lock.
        if deck.voice.is_empty() {
            log::debug!("Dropping gapless preload of {path}: nothing left to follow");
            return;
        }
        deck.stage(|d| self.build_source(decoded, baked_rg, d));
        self.gapless_pending.store(true, Ordering::Release);
    }

    /// Free the sources the audio callback has finished with.
    ///
    /// Rides the monitor's existing tick rather than a timer of its own: a track change is the
    /// only thing that produces one, so anything faster would be polling an empty queue. See
    /// `output::voice::Voice::collect_spent` for what it saves.
    pub fn collect_spent(&self) {
        let decks = self.lock_decks();
        decks.active().voice.collect_spent();
        decks.idle().voice.collect_spent();
    }

    /// One lock acquisition, so gapless-transition and end-of-stream detection
    /// can't race each other.
    ///
    /// Reads the **active** deck only. During a crossfade that deck holds
    /// exactly the incoming track and this reports `Playing`; the outgoing deck
    /// draining is [`Self::is_crossfading`]'s business.
    pub fn check_playback_state(&self) -> PlaybackCheck {
        let was_gapless = self.gapless_pending.load(Ordering::Acquire);
        let decks = self.lock_decks();
        let sources = decks.active().voice.len();
        drop(decks);

        let result = evaluate_playback_check(was_gapless, sources);
        if result == PlaybackCheck::GaplessTransition {
            self.gapless_pending.store(false, Ordering::Release);
        }
        result
    }

    /// Lock the decks mutex, recovering from poison rather than panicking.
    fn lock_decks(&self) -> MutexGuard<'_, Decks> {
        lock_decks(&self.decks)
    }
}

#[cfg(test)]
#[path = "tests/backend_tests.rs"]
mod tests;
