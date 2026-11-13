//! What both decoders need before and around the codec: the registry, the track pick, the open
//! that resolves the two, and the packet cursor.
//!
//! [`super::file_decode`] hands [`open`] a file source with a known length and seek capability.
//!
//! **Why Symphonia 0.6 rather than the 0.5 rodio pinned.** 0.5's probe picks a demuxer by matching a
//! two-byte marker and nothing else — its `format()` takes a `Hint` and discards it, and scoring is
//! still a `TODO` there. The only ADTS marker it registers is `0xFFF1`, so MPEG-2 ADTS (`0xFFF9`)
//! matches nothing at all, the search runs on into the payload, and the first stray `0xFFFB` in it
//! hands an AAC stream to the MP3 demuxer. That reader then hunts forever for two consecutive
//! similar frames it will never find: no decoder is built, no error is returned, and the audio
//! simply never starts. An MPEG-2 `.aac` file in a scanned folder is the same bytes and the same
//! silence. 0.6 scores each candidate against the frames that follow it and registers all four ADTS
//! sync words.

use std::sync::LazyLock;
use std::time::Duration;

use symphonia::core::audio::GenericAudioBufferRef;
use symphonia::core::codecs::CodecParameters;
use symphonia::core::codecs::audio::well_known::{
    CODEC_ID_AAC, CODEC_ID_ADPCM_IMA_QT, CODEC_ID_ADPCM_IMA_WAV, CODEC_ID_ADPCM_MS, CODEC_ID_MP1,
    CODEC_ID_MP2, CODEC_ID_MP3, CODEC_ID_OPUS, CODEC_ID_PCM_ALAW, CODEC_ID_PCM_MULAW,
    CODEC_ID_VORBIS,
};
use symphonia::core::codecs::audio::{
    AudioCodecId, AudioCodecParameters, AudioDecoder, AudioDecoderOptions, CODEC_ID_NULL_AUDIO,
};
use symphonia::core::codecs::registry::CodecRegistry;
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, FormatReader, Track, TrackType};
use symphonia::core::io::{MediaSource, MediaSourceStream, MediaSourceStreamOptions};
use symphonia::core::meta::MetadataOptions;
use symphonia::core::units::{Duration as SymphoniaDuration, TimeBase};

use super::aac_config;
use super::audio::{ChannelCount, SampleRate, Shape, SourceFormat};

/// Our own registry, rather than `symphonia::default::get_codecs`.
///
/// That one is fixed at whatever the crate's own features enable, so a decoder living outside it
/// could never be reached: Symphonia ships none for Opus and offers no feature for one, and
/// registering [`super::opus`] is a line here and nothing at either call site.
static CODECS: LazyLock<CodecRegistry> = LazyLock::new(|| {
    let mut registry = CodecRegistry::new();
    symphonia::default::register_enabled_codecs(&mut registry);
    registry.register_audio_decoder::<super::opus::OpusDecoder>();
    registry
});

/// The track to decode, or `None` when the container declares nothing playable.
///
/// `default_track` matches the container's default flag before it looks at the codec
/// ([symphonia#258](https://github.com/pdeljanov/Symphonia/issues/258)), so a track naming a null
/// one can win the pick and the fallback that would have rejected it never runs.
pub(super) fn audio_track(format: &dyn FormatReader) -> Option<&Track> {
    format
        .default_track(TrackType::Audio)
        .filter(|track| audio_params(track).is_some())
        .or_else(|| format.first_track_known_codec(TrackType::Audio))
}

/// The track's audio parameters, when it has some and they name a codec.
pub(super) fn audio_params(track: &Track) -> Option<&AudioCodecParameters> {
    track
        .codec_params
        .as_ref()
        .and_then(CodecParameters::audio)
        .filter(|params| params.codec != CODEC_ID_NULL_AUDIO)
}

/// Which way [`ticks_to_frames`] resolves a count the timebase does not divide evenly.
#[derive(Clone, Copy)]
pub(super) enum Rounding {
    /// Never claim more frames than the count is worth — a trim that overshoots cuts real audio.
    Down,
    /// Never claim fewer — a post-seek trim that undershoots replays the tail of the last packet.
    Up,
}

/// Restates `ticks`, counted against `time_base`, in frames decoded at `rate`.
///
/// `u128` throughout: the product of a tick count, a timebase numerator and a sample rate overflows
/// `u64` for lengths a container can legitimately state. `None` where the result does not fit back
/// into one, which is a container stating nonsense rather than a case to round.
pub(super) fn ticks_to_frames(
    ticks: u64,
    time_base: TimeBase,
    rate: SampleRate,
    rounding: Rounding,
) -> Option<u64> {
    let scaled = u128::from(ticks) * u128::from(time_base.numer.get()) * u128::from(rate.get());
    let denom = u128::from(time_base.denom.get());
    let frames = match rounding {
        Rounding::Down => scaled / denom,
        Rounding::Up => scaled.div_ceil(denom),
    };
    u64::try_from(frames).ok()
}

/// Build a decoder for `params`, taking them by value because it rewrites an HE-AAC config on the
/// way past ([`super::aac_config`]).
///
/// The Symphonia error goes back unwrapped so the caller can name the file, which this layer cannot.
pub(super) fn make_decoder(
    mut params: AudioCodecParameters,
) -> Result<Box<dyn AudioDecoder>, SymphoniaError> {
    aac_config::demote_he_aac(&mut params);
    drop_companded_sample_width(&mut params);
    CODECS.make_audio_decoder(&params, &AudioDecoderOptions::default())
}

/// Clears the sample widths on an A-law or mu-law track, which two Symphonia crates disagree about.
///
/// Both companded formats code 8 bits and decode to 16, and 0.6's PCM decoder reads the width
/// fields as the *decoded* one: it refuses anything but 16, defaulting to exactly that when they
/// are absent. `symphonia-format-caf` fills them from the file's own coded width instead, so every
/// A-law `.caf` fails to build a decoder at all. 0.5 had no such check and played them, which makes
/// this a file that stops playing rather than one that never did — the same shape of regression as
/// [`aac_config`], and the reason the fixture walk in `file_decode`'s tests opens every container
/// rather than trusting that a format the manifest enables is a format that decodes.
///
/// Clearing rather than correcting, because the decoder derives the right width from the codec and
/// a demuxer that already agreed loses nothing.
fn drop_companded_sample_width(params: &mut AudioCodecParameters) {
    if matches!(params.codec, CODEC_ID_PCM_ALAW | CODEC_ID_PCM_MULAW) {
        params.bits_per_sample = None;
        params.bits_per_coded_sample = None;
    }
}

/// Whether `codec` throws part of the signal away, so its decode is a reconstruction.
///
/// Spelled out because Symphonia carries no such flag, only an id whose numbering groups the
/// lossy codecs in a comment. The companded PCMs count: they code a 13- or 14-bit signal in 8. A
/// codec added to [`CODECS`] belongs here too if it is lossy, or its tracks read as lossless.
fn lossy_codec(codec: AudioCodecId) -> bool {
    matches!(
        codec,
        CODEC_ID_MP1
            | CODEC_ID_MP2
            | CODEC_ID_MP3
            | CODEC_ID_AAC
            | CODEC_ID_VORBIS
            | CODEC_ID_OPUS
            | CODEC_ID_ADPCM_MS
            | CODEC_ID_ADPCM_IMA_WAV
            | CODEC_ID_ADPCM_IMA_QT
            | CODEC_ID_PCM_ALAW
            | CODEC_ID_PCM_MULAW
    )
}

/// A demuxer, a decoder for its audio track, and enough of that track decoded to know its shape.
///
/// [`super::file_decode`] and [`super::stream_decode`] hold the first four of these verbatim and
/// differ only in what they hand [`open`] and in how they name a failure, which is why the sequence
/// that produces them lives here rather than once in each. `time_base` and `total_duration` are the
/// two a mount has no use for and drops.
pub(super) struct Opened {
    pub format: Box<dyn FormatReader>,
    pub decoder: Box<dyn AudioDecoder>,
    pub track: u32,
    pub cursor: Cursor,
    pub source_format: SourceFormat,
    pub time_base: Option<TimeBase>,
    pub total_duration: Option<Duration>,
}

/// Why a source could not be opened, phrased as the tail of a sentence naming it — so a caller
/// says "`/music/x.flac` has no audio" instead of returning an unexplained engine error.
pub(super) enum OpenError {
    Unreadable(SymphoniaError),
    NoAudio,
    NoCodec,
    Undecodable(SymphoniaError),
    Empty,
}

impl std::fmt::Display for OpenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unreadable(e) => write!(f, "could not be read: {e}"),
            Self::NoAudio => f.write_str("has no audio"),
            Self::NoCodec => f.write_str("names no audio codec"),
            Self::Undecodable(e) => write!(f, "could not be decoded: {e}"),
            Self::Empty => f.write_str("decoded to nothing"),
        }
    }
}

/// Probe `source`, build a decoder for its audio track, and decode enough of it to know the shape
/// of what follows.
///
/// `hint` is the one thing the two callers answer differently — a file's extension against a
/// mount's content type — and 0.6 scores rather than trusting it either way.
pub(super) fn open(source: Box<dyn MediaSource>, hint: &Hint) -> Result<Opened, OpenError> {
    let mss = MediaSourceStream::new(source, MediaSourceStreamOptions::default());
    let mut format = symphonia::default::get_probe()
        .probe(hint, mss, FormatOptions::default(), MetadataOptions::default())
        .map_err(OpenError::Unreadable)?;

    let track = audio_track(&*format).ok_or(OpenError::NoAudio)?;
    let params = audio_params(track).ok_or(OpenError::NoCodec)?.clone();
    let stated_bits = params.bits_per_sample;
    let lossy = lossy_codec(params.codec);
    let track_id = track.id;
    let time_base = track.time_base;
    let total_duration = playing_time(&*format, track);

    let mut decoder = make_decoder(params).map_err(OpenError::Undecodable)?;
    let (cursor, decoded) = Cursor::open(&mut *format, &mut *decoder, track_id)
        .map_err(OpenError::Unreadable)?
        .ok_or(OpenError::Empty)?;
    let source_format = SourceFormat { lossy, ..narrowed_to_stated(decoded, stated_bits) };

    Ok(Opened {
        format,
        decoder,
        track: track_id,
        cursor,
        source_format,
        time_base,
        total_duration,
    })
}

/// The decoded format with its width taken down to what the track states it carries.
///
/// A decoder hands integer PCM over in whatever container it has a buffer type for, so a 24-bit
/// file can arrive in 32-bit samples. Read at the container width it would count as losing its low
/// bits on the way to `f32`, which it doesn't.
fn narrowed_to_stated(decoded: SourceFormat, stated_bits: Option<u32>) -> SourceFormat {
    let stated = stated_bits.and_then(|bits| u8::try_from(bits).ok());
    match stated {
        Some(bits) if !decoded.float && bits < decoded.bits => SourceFormat { bits, ..decoded },
        _ => decoded,
    }
}

/// The width and kind of sample a decoder produced, before the copy flattens it to `f32`.
fn decoded_format(decoded: &GenericAudioBufferRef<'_>) -> SourceFormat {
    let (bits, float) = match decoded {
        GenericAudioBufferRef::U8(_) | GenericAudioBufferRef::S8(_) => (8, false),
        GenericAudioBufferRef::U16(_) | GenericAudioBufferRef::S16(_) => (16, false),
        GenericAudioBufferRef::U24(_) | GenericAudioBufferRef::S24(_) => (24, false),
        GenericAudioBufferRef::U32(_) | GenericAudioBufferRef::S32(_) => (32, false),
        GenericAudioBufferRef::F32(_) => (32, true),
        GenericAudioBufferRef::F64(_) => (64, true),
    };
    SourceFormat { bits, float, lossy: false }
}

/// How long the track plays for.
///
/// Three sources, because no one of them answers for every container. `Track::duration` is the one
/// upstream says to present, a timebase not having to be the reciprocal of the frame rate;
/// `num_frames` is where a container states its length in frames instead; and Matroska states
/// neither on the track, putting the segment's own duration on the media. A `.mka` reaching the
/// library reading 0:00 is what this exists to prevent, and it is the container that needs the
/// third.
///
/// A stated zero is the reader saying it doesn't know rather than the file being empty — a
/// fragmented MP4 zeroes both track fields — so each one has to be discarded before the next is
/// reached, or the first answers for all three. A live mount states none of the three, which is
/// what lets the stream path take this answer unchanged.
fn playing_time(format: &dyn FormatReader, track: &Track) -> Option<Duration> {
    let from_track = track.time_base.and_then(|time_base| {
        let frames = || track.num_frames.filter(|frames| *frames > 0).map(SymphoniaDuration::new);
        let ticks = track.duration.filter(|stated| stated.get() > 0).or_else(frames)?;
        time_base.calc_duration(ticks)
    });
    let from_media = || {
        let media = format.media_info();
        media.time_base?.calc_duration(media.duration.filter(|stated| stated.get() > 0)?)
    };

    let (seconds, nanos) = from_track.or_else(from_media)?.parts();
    Some(Duration::new(u64::try_from(seconds).ok()?, nanos))
}

/// The packet being handed out, one interleaved sample at a time.
///
/// Both decoders own one and neither adds to it: the refill, the shape check a deck's fixed append
/// makes necessary, and the latch that stops a re-poll past the end serving the packet that ended
/// it are the same three answers whether the bytes came off a socket or off disk.
pub(super) struct Cursor {
    samples: Vec<f32>,
    next: usize,
    shape: Shape,
    /// Set once there is nothing more to hand out.
    ended: bool,
}

impl Cursor {
    /// Decode enough to know the shape of what follows. `Ok(None)` where the source held no
    /// audio at all, against `Err` for one that could not be read.
    ///
    /// The two are worth telling apart here and nowhere else: this is the one call reached off the
    /// audio thread, and its answer becomes what the user is shown, where a permissions failure and
    /// an empty container read very differently.
    ///
    /// The first packet is decoded eagerly because the deck builds its converter from the channel
    /// count and sample rate the source reports at the append, and cannot rebuild it mid-source.
    /// It is also the only place the decoder's own sample format can be read, so that comes back
    /// beside the cursor.
    pub(super) fn open(
        format: &mut dyn FormatReader,
        decoder: &mut dyn AudioDecoder,
        track: u32,
    ) -> Result<Option<(Self, SourceFormat)>, SymphoniaError> {
        let mut samples = Vec::new();
        let Some(packet) = fill(format, decoder, track, &mut samples)? else {
            return Ok(None);
        };
        let cursor = Self { samples, next: 0, shape: packet.shape, ended: false };
        Ok(Some((cursor, packet.format)))
    }

    pub(super) fn shape(&self) -> Shape {
        self.shape
    }

    /// Steps past the buffered packet, reporting the channel of the frame the puller was part way
    /// through so a seek can put it back there.
    pub(super) fn discard_buffered(&mut self) -> usize {
        let channel_phase = self.next % usize::from(self.shape.channels.get());
        self.next = self.samples.len();
        self.ended = false;
        channel_phase
    }

    pub(super) fn next_sample(
        &mut self,
        format: &mut dyn FormatReader,
        decoder: &mut dyn AudioDecoder,
        track: u32,
    ) -> Option<f32> {
        if self.ended {
            return None;
        }
        if self.next >= self.samples.len() {
            // The sample iterator has no error channel. A read failure and a clean end both
            // exhaust the cursor.
            let Ok(Some(packet)) = fill(format, decoder, track, &mut self.samples) else {
                self.ended = true;
                return None;
            };
            // The deck's converter was built from the shape reported at the append, so a source
            // that changes one mid-track ends here rather than playing on at the wrong rate. A
            // mount doing it is a reconnect the feed thread then refuses.
            if packet.shape != self.shape {
                self.ended = true;
                return None;
            }
            self.next = 0;
        }
        let sample = *self.samples.get(self.next)?;
        self.next += 1;
        Some(sample)
    }
}

/// Decode packets into `samples` until one yields audio, or the source runs out.
///
/// A packet that fails to decode is skipped rather than ending it: a mount joined mid-frame opens
/// with a few that do, and a file with a damaged frame plays past it the way every other player
/// does. So is a packet that decodes to nothing, which is not the end of the stream either
/// ([symphonia#403](https://github.com/pdeljanov/Symphonia/issues/403)).
///
/// `Err(ResetRequired)` does end it, and that one is a choice rather than an oversight: Ogg raises
/// it where a chained stream starts a new physical one, and recovering means re-reading the track
/// list and rebuilding the decoder against parameters that may have moved. Neither reference player
/// does it. A mount gets the feed thread's reconnect on top; a chained file stops there.
///
/// A read that fails is kept apart from a clean end, which 0.6 made possible by spelling the end
/// as `Ok(None)` where 0.5 raised `UnexpectedEof`. Only [`Cursor::open`] has any use for the
/// distinction; [`Cursor::next_sample`] folds the two back together and says why.
fn fill(
    format: &mut dyn FormatReader,
    decoder: &mut dyn AudioDecoder,
    track: u32,
    samples: &mut Vec<f32>,
) -> Result<Option<Packet>, SymphoniaError> {
    loop {
        let Some(packet) = format.next_packet()? else {
            return Ok(None);
        };
        if packet.track_id != track {
            continue;
        }

        match decoder.decode(&packet) {
            Ok(decoded) => {
                // Borrowed, not cloned: `copy_to_vec_interleaved` takes `&self` too, and
                // `Channels` has a boxed variant, so a clone would allocate on every packet.
                let spec = decoded.spec();
                decoded.copy_to_vec_interleaved(samples);
                if !samples.is_empty() {
                    let channels =
                        u16::try_from(spec.channels().count()).ok().and_then(ChannelCount::new);
                    let (Some(channels), Some(rate)) = (channels, SampleRate::new(spec.rate()))
                    else {
                        // Nothing a deck can be handed, so it is the decoder rather than the read
                        // that this source runs out on.
                        return Err(SymphoniaError::Unsupported(
                            "channel count or sample rate out of range",
                        ));
                    };
                    let shape = Shape { channels, rate };
                    return Ok(Some(Packet { shape, format: decoded_format(&decoded) }));
                }
            }
            Err(SymphoniaError::DecodeError(_)) => {}
            Err(e) => return Err(e),
        }
    }
}

/// What [`fill`] decoded, as far as anything above it asks.
struct Packet {
    shape: Shape,
    format: SourceFormat,
}

#[cfg(test)]
#[path = "tests/decode_tests.rs"]
mod tests;
