//! The device under the decks: reopening it, following each track's format into it, and taking
//! it exclusively.
//!
//! **A reopen is decided at the track boundary, from the decoder, before the track is heard.**
//! It happens under the decks lock, ahead of the append that starts the track, so nothing of it
//! plays in the old format. That is also why a crossfade and a reopen cannot share a transition,
//! and why a gapless stage refuses a track that needs one: both would need the outgoing track
//! still playing across it. So each asks of the next track whether the stream open now plays it,
//! and one that needs a reopen starts at `EndOfStream` instead.
//!
//! **An exclusive claim is held through a pause and given back on stop**, so a stopped player
//! never keeps other applications off the card. Where the user asks for it, a long pause ends in
//! that stop too, the track left on screen paused.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, MutexGuard};
use std::time::Duration;

use netodia_audio::player::source::audio::{AudioSource, SampleRate, Shape, SourceFormat};
use netodia_audio::player::source::buffered::BufferedAudioSource;
use netodia_audio::player::source::file_decode::FileDecoder;
use netodia_core::error::{AppError, describe};
use netodia_playback::player::playback::decks::Decks;
use netodia_playback::player::playback::output::device::{ExternalVolume, Lead};
use netodia_playback::player::playback::output::{
    self, AudioOutput, ExclusiveRequest, ExclusiveTuning, Negotiated, OutputMode, OutputRequest,
    OutputStatus, RateFallback,
};
use parking_lot::Mutex;

use super::PlaybackEngine;
use crate::player::engine::signal_path::{self, SignalInputs, SignalPath, Transport};

/// How the user asked for the output to be opened.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OutputChoice {
    pub mode: OutputMode,
    /// The card an exclusive claim is aimed at, and where [`output::SHARED_DEVICE_SUPPORTED`] the
    /// one shared output plays through too. `None` for the first the system lists, which a shared
    /// stream reads as following the system default.
    pub device: Option<String>,
    pub tuning: ExclusiveTuning,
    /// Carry the volume on the device's own control where an exclusive claim can.
    pub hardware_volume: bool,
    pub rate_fallback: RateFallback,
}

impl OutputChoice {
    /// The chosen device as shared output's target, which a backend whose ids can't name one never
    /// gets: a Linux card's would go round the sound server.
    pub fn shared_device(&self) -> Option<&str> {
        self.device.as_deref().filter(|_| output::SHARED_DEVICE_SUPPORTED)
    }

    /// A shared stream at `rate` on [`Self::shared_device`].
    fn shared_request(&self, rate: Option<SampleRate>) -> OutputRequest {
        OutputRequest::Shared { rate, device: self.shared_device().map(str::to_owned) }
    }
}

/// What opening a file for a track-boundary decision found.
#[derive(Debug, Clone, Copy)]
enum Probed {
    Decodes(Shape, SourceFormat),
    Unreadable,
}

/// The device under the decks, what it is asked to open for, and the cells it publishes. Those
/// are read without the device's lock, which a reopen holds for as long as a device takes to open.
#[derive(Default)]
pub(super) struct EngineOutput {
    /// `None` on the device-free rigs the tests build. Taken after the decks lock wherever both
    /// are held, the order `reopen_output` takes them in. `Arc` so the deferred half of a faded
    /// stop can give an exclusive card back once the fade has played out.
    pub(super) device: Arc<Mutex<Option<AudioOutput>>>,
    /// Open the output at each track's own rate. Read only at a track boundary, never by the audio
    /// thread.
    follow_rate: AtomicBool,
    /// Give an exclusive device back once a pause has held it long enough. Read by the monitor on
    /// its paused ticks, never by the audio thread.
    release_when_paused: AtomicBool,
    /// Shared or exclusive, and which card. Read at a track boundary, never by the audio thread.
    choice: Mutex<OutputChoice>,
    /// How far the ear is behind the decks' clocks, measured by whichever stream is open.
    pub(super) lead: Arc<Lead>,
    /// Where a claim carrying the volume on the device reports the system moved it.
    external_volume: Arc<ExternalVolume>,
    /// Whether the output is parked, waiting on a disconnected device, or reopening: the health
    /// checks ask on every tick, from a runtime worker.
    status: Arc<OutputStatus>,
    /// The last file a track-boundary decision opened, and what it held. The monitor asks about
    /// the next track on every tick until it starts, and this keeps that to one open. The format
    /// is kept rather than the verdict, which is asked again of the output as it stands.
    probed: Mutex<Option<(String, Probed)>>,
}

impl EngineOutput {
    /// `output`, with the cells it publishes for every stream it opens.
    pub(super) fn over(output: AudioOutput) -> Self {
        Self {
            lead: output.lead(),
            external_volume: output.external_volume(),
            status: output.status(),
            device: Arc::new(Mutex::new(Some(output))),
            follow_rate: AtomicBool::new(false),
            release_when_paused: AtomicBool::new(false),
            choice: Mutex::default(),
            probed: Mutex::default(),
        }
    }
}

impl PlaybackEngine {
    /// Reopen the output on what the last stream was asked for, and carry on from where the
    /// decks are. `Ok(None)` while the output is parked, which nothing should reopen.
    ///
    /// **The decks lock is held across the whole reopen**, so a transport op queues behind it
    /// instead of sending a command to a stream that isn't there and waiting out
    /// `output::voice::SERVICE_TIMEOUT` for an answer. Blocking: it opens a device.
    ///
    /// # Errors
    ///
    /// [`AppError::Player`] when there is no output to reopen, and the device's own open failure
    /// when it refuses.
    pub fn reopen_output(&self) -> Result<Option<Negotiated>, AppError> {
        let decks = self.lock_decks();
        let mut output = self.output.device.lock();
        let output = output
            .as_mut()
            .ok_or_else(|| AppError::Player("There is no audio output to reopen".to_owned()))?;
        if output.is_parked() {
            return Ok(None);
        }
        let request = output.request().clone();
        reopen_routed(&decks, output, request).map(Some)
    }

    /// Close the output for good, giving an exclusive claim back and the device's own volume with
    /// it. For the way out: `process::exit` runs no destructor, and a parked output would be
    /// reopened by a track starting while the monitor still runs. Parked on the way, so the stall
    /// watch doesn't take the missing stream for a lost one while the process exits.
    pub fn close_output(&self) {
        let closed = self.output.device.lock().take();
        if let Some(mut output) = closed {
            output.park();
        }
    }

    /// Set the user's volume, as an amplitude: on the device's own control where an exclusive claim
    /// carries it there, leaving the voices at unity, and on the voices otherwise.
    pub fn set_volume(&self, volume: f64) {
        let decks = self.lock_decks();
        decks.set_volume_all(self.route_volume(volume));
    }

    /// Hand the user's `volume`, as an amplitude, to the output and answer the gain the voices
    /// apply for it on the stream open now.
    pub(super) fn route_volume(&self, volume: f64) -> f64 {
        self.set_output_volume(volume);
        self.voice_gain(volume)
    }

    /// [`Self::route_volume`] for a track starting fresh from `source`, reopening the output for it
    /// in between: handed over first, so a claim opens with the device's own control already
    /// there, and the gain read after, off whichever route the reopen landed on.
    pub(super) fn prepare_output_for(
        &self,
        decks: &MutexGuard<'_, Decks>,
        source: &impl AudioSource,
        volume: f64,
    ) -> f64 {
        self.set_output_volume(volume);
        self.reopen_for_track(decks, source.shape(), source.format());
        self.voice_gain(volume)
    }

    /// Hand the user's volume, as an amplitude, to the output for a claim carrying it on the
    /// device's own control.
    fn set_output_volume(&self, volume: f64) {
        if let Some(output) = self.output.device.lock().as_ref() {
            output.set_volume(volume);
        }
    }

    /// The gain the voices apply for `volume` on the output as it is. `volume` itself with no
    /// output, which is the device-free rigs the tests build.
    fn voice_gain(&self, volume: f64) -> f64 {
        self.output.device.lock().as_ref().map_or(volume, |output| output.voice_gain(volume))
    }

    /// Where the system moving the device's own volume control is reported, while a claim carries
    /// the volume there, for the player to follow. Lock-free, and the same cell across reopens.
    pub fn external_volume(&self) -> Arc<ExternalVolume> {
        Arc::clone(&self.output.external_volume)
    }

    /// Whether the output was closed on purpose, so a stream that stopped beating is not a fault.
    /// Lock-free, so a health check never waits out a reopen.
    pub fn output_parked(&self) -> bool {
        self.output.status.parked()
    }

    /// Whether a pause would be keeping an exclusive device from everything else and the user
    /// asked for it back: the setting is on, exclusive output is chosen, and the output is open.
    ///
    /// The choice rather than the open stream, so a shared one standing in for a refused claim
    /// counts too. Giving that back costs nothing, and the track start that follows retries the
    /// claim. Never takes the device's lock, which a reopen holds.
    pub fn holds_releasable_claim(&self) -> bool {
        self.output.release_when_paused.load(Ordering::Relaxed)
            && self.output.choice.lock().mode == OutputMode::Exclusive
            && !self.output_parked()
    }

    /// Whether the output stands in for a chosen device that wasn't connected, as a claim's
    /// fallback or a shared stream on the default, which is the only case
    /// [`Self::disconnected_device_returned`] has anything to look for. Lock-free.
    pub fn awaiting_disconnected_device(&self) -> bool {
        self.output.status.awaiting_device()
    }

    /// Whether a reopen is holding the decks lock across a device open, for a poll that would
    /// rather skip a round than wait on it or read its silent gap as a stall. Lock-free.
    pub fn output_reopening(&self) -> bool {
        self.output.status.reopening()
    }

    /// What the device agreed to, or `None` while no stream is open.
    pub fn negotiated(&self) -> Option<Negotiated> {
        self.output.device.lock().as_ref().and_then(AudioOutput::negotiated)
    }

    /// Whether an output standing in for a chosen device that wasn't connected can find it listed
    /// again, so it is worth reopening now. A track start asks again only where it reopens for
    /// another format anyway, which a gapless album never does. Lists the devices only while such
    /// a stand-in lasts. Blocking.
    pub fn disconnected_device_returned(&self) -> bool {
        if !self.awaiting_disconnected_device() {
            return false;
        }
        let chosen = self.output.choice.lock().device.clone();
        let listed = output::devices();
        match chosen {
            Some(id) => listed.iter().any(|device| device.id == id),
            None => !listed.is_empty(),
        }
    }

    /// The path the playing source takes to the device, or `None` while nothing plays or no
    /// stream is open. Takes the decks lock and then the output lock, never both at once.
    pub fn signal_path(&self, transport: Transport) -> Option<SignalPath> {
        let source = self.lock_decks().active().voice.playing()?;
        let negotiated = self.negotiated()?;
        Some(signal_path::evaluate(SignalInputs {
            negotiated,
            source,
            eq_on: self.eq.enabled(),
            rg_on: self.rg.enabled(),
            transport,
        }))
    }

    /// Open the output at each track's own rate from the next track on, or go back to the
    /// device's own config. Lock-free; nothing reopens until a track boundary asks.
    ///
    /// Stays off where that changes nothing ([`output::FOLLOW_RATE_SUPPORTED`]), whatever a
    /// settings file says: it would only reopen the output at every track.
    pub fn set_follow_rate(&self, on: bool) {
        self.output.follow_rate.store(on && output::FOLLOW_RATE_SUPPORTED, Ordering::Relaxed);
    }

    /// Give an exclusive device back once a pause has held it long enough, from the next pause
    /// on. Lock-free. Stays off where nothing can be claimed, whatever a settings file says.
    pub fn set_release_when_paused(&self, on: bool) {
        self.output.release_when_paused.store(on && output::EXCLUSIVE_SUPPORTED, Ordering::Relaxed);
    }

    /// Write `hold` of silence after each reopen onto a new rate, from the next one on. Blocking:
    /// it waits out any reopen in flight.
    pub fn set_resync_hold(&self, hold: Duration) {
        if let Some(output) = self.output.device.lock().as_mut() {
            output.set_resync_hold(hold);
        }
    }

    /// Take the card for ourselves or give it back, now rather than at the next track.
    ///
    /// Whatever is loaded, paused included, reopens under the new choice. With nothing loaded an
    /// exclusive choice waits for the first play to claim, and a shared one reopens the chosen
    /// device shared, which is what hands the card back to everything else. Blocking: it opens a
    /// device.
    pub fn set_output_choice(&self, choice: OutputChoice) {
        let exclusive = choice.mode == OutputMode::Exclusive;
        *self.output.choice.lock() = choice;
        let decks = self.lock_decks();
        let wanted = match decks.active().voice.playing() {
            Some(playing) => Some(self.wanted_request(playing.shape, playing.format)),
            None if exclusive => None,
            None => Some(self.output.choice.lock().shared_request(None)),
        };
        let mut output = self.output.device.lock();
        let Some(output) = output.as_mut() else {
            return;
        };
        output.forget_refusal();
        let Some(wanted) = wanted else {
            output.park();
            return;
        };
        if output.serves(&wanted) {
            return;
        }
        match reopen_routed(&decks, output, wanted) {
            Ok(negotiated) => log::info!("Output reopened for a new output choice: {negotiated:?}"),
            Err(e) => log::warn!("Failed to reopen the output: {}", describe(&e)),
        }
    }

    /// What the output should be asked for while a source in `shape` and `format` plays.
    fn wanted_request(&self, shape: Shape, format: SourceFormat) -> OutputRequest {
        let choice = self.output.choice.lock();
        match choice.mode {
            OutputMode::Exclusive => OutputRequest::Exclusive(ExclusiveRequest {
                device: choice.device.clone(),
                shape,
                format,
                tuning: choice.tuning,
                hardware_volume: choice.hardware_volume,
                rate_fallback: choice.rate_fallback,
            }),
            OutputMode::Shared => {
                let rate = self.output.follow_rate.load(Ordering::Relaxed).then_some(shape.rate);
                choice.shared_request(rate)
            }
        }
    }

    /// Whether a source in `shape` and `format` can play on the output as it is, with no reopen.
    /// Always true with no output, which is the device-free rigs the tests build.
    pub(super) fn plays_without_reopen(&self, shape: Shape, format: SourceFormat) -> bool {
        let wanted = self.wanted_request(shape, format);
        self.output.device.lock().as_ref().is_none_or(|output| output.serves(&wanted))
    }

    /// Whether the output has to reopen for the file at `path`, which no crossfade can cross. A
    /// file that won't open counts as one, so nothing fades into it. Blocking: it opens the file
    /// the first time it is asked about.
    pub fn reopens_for(&self, path: &str) -> bool {
        let plays = self.recorded_answer(path).unwrap_or_else(|| match self.open_recorded(path) {
            Ok(decoded) => self.plays_without_reopen(decoded.shape(), decoded.format()),
            Err(e) => {
                log::debug!("Not crossfading into {path}, which won't open: {}", describe(&e));
                false
            }
        });
        !plays
    }

    /// Open the file at `path` to stage behind the playing track, or `None` where the output would
    /// have to reopen for it. Asked every tick until the track ends, so a refusal opens the file
    /// once.
    ///
    /// # Errors
    ///
    /// Failure to open the file or fill its initial PCM reserve.
    pub(super) fn open_for_gapless(
        &self,
        path: &str,
    ) -> Result<Option<BufferedAudioSource>, AppError> {
        if self.recorded_answer(path) == Some(false) {
            return Ok(None);
        }
        let decoded = self.open_recorded(path)?;
        // Left unstaged, it ends in `EndOfStream` and starts through `play_media`, which reopens.
        if !self.plays_without_reopen(decoded.shape(), decoded.format()) {
            log::debug!("Not staging {path} gapless: the output reopens for its format");
            return Ok(None);
        }
        BufferedAudioSource::from_positioned(decoded).map(Some)
    }

    /// Whether the file at `path` plays on the output as it is, by what opening it last found.
    /// `None` until something has opened it.
    fn recorded_answer(&self, path: &str) -> Option<bool> {
        let found = self
            .output
            .probed
            .lock()
            .as_ref()
            .and_then(|(probed, found)| (probed == path).then_some(*found))?;
        Some(match found {
            Probed::Decodes(shape, format) => self.plays_without_reopen(shape, format),
            Probed::Unreadable => false,
        })
    }

    /// Open the file at `path` for a track boundary, recording what it holds for the asks after.
    ///
    /// # Errors
    ///
    /// Whatever [`FileDecoder::open`] refuses the file with.
    fn open_recorded(&self, path: &str) -> Result<FileDecoder, AppError> {
        let opened = FileDecoder::open(Path::new(path));
        let found = match &opened {
            Ok(decoded) => Probed::Decodes(decoded.shape(), decoded.format()),
            Err(_) => Probed::Unreadable,
        };
        *self.output.probed.lock() = Some((path.to_owned(), found));
        opened
    }

    /// Reopen the output for a track starting fresh in `shape` and `format`.
    ///
    /// **A shared output following the rate reopens even at the rate already asked for.**
    /// `PipeWire` settles the card's rate when a stream starts, not when one leaves, and ours
    /// never idles, so a rate another app held the card at when ours opened would stick after that
    /// app left. A same-rate reopen writes no resync silence, so a track start is the cheap place
    /// to take the card back. An exclusive claim that fell back is retried here for the same
    /// reason, since whatever held the card may have let go, wherever
    /// `FallbackReason::retry_at_track_start` allows. A gapless transition doesn't come through
    /// here and keeps the output as it is.
    ///
    /// Takes the decks guard as proof it is held: this runs inside a transport op, between its
    /// decode and its append. A failure is logged and playback carries on over whatever
    /// `AudioOutput::reopen` fell back to.
    fn reopen_for_track(&self, decks: &MutexGuard<'_, Decks>, shape: Shape, format: SourceFormat) {
        let wanted = self.wanted_request(shape, format);
        let mut output = self.output.device.lock();
        let Some(output) = output.as_mut() else {
            return;
        };
        let open = output.negotiated();
        let previous_rate = open.as_ref().map(|negotiated| negotiated.shape.rate);
        let retry_claim = open
            .and_then(|negotiated| negotiated.fallback)
            .is_some_and(|fallback| fallback.reason.retry_at_track_start());
        let reclaim = matches!(wanted, OutputRequest::Shared { rate: Some(_), .. }) || retry_claim;
        if output.serves(&wanted) && !reclaim {
            return;
        }
        match reopen_routed(decks, output, wanted) {
            Ok(negotiated) if previous_rate == Some(negotiated.shape.rate) => {
                log::debug!("Output reopened: {negotiated:?}");
            }
            Ok(negotiated) => log::info!("Output reopened: {negotiated:?}"),
            Err(e) => log::warn!("Failed to reopen the output: {}", describe(&e)),
        }
    }
}

/// Reopen `output` for `request`, with the voices' gain on whichever side of the device carries
/// the volume once it has.
///
/// **The software gain across the reopen, unity only after it**, and only where the new stream took
/// the device's own control. Whatever plays in between is attenuated twice or at the software
/// gain, so a change of route, a fallback to shared included, dips and never jumps.
fn reopen_routed(
    decks: &Decks,
    output: &mut AudioOutput,
    request: OutputRequest,
) -> Result<Negotiated, AppError> {
    let volume = output.volume();
    decks.set_volume_all(volume);
    let reopened = output.reopen(request);
    decks.set_volume_all(output.voice_gain(volume));
    reopened
}

/// Close an exclusive stream so the card goes back to everything else. A shared one stays open:
/// it holds nothing another application wants.
pub(super) fn release_exclusive(output: &Mutex<Option<AudioOutput>>) {
    if let Some(output) = output.lock().as_mut()
        && matches!(output.request(), OutputRequest::Exclusive(_))
    {
        output.park();
    }
}

#[cfg(all(test, any(target_os = "windows", target_os = "linux")))]
#[path = "tests/output_tests.rs"]
mod tests;
