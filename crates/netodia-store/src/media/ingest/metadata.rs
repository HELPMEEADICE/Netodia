use std::io::Read;
use std::path::Path;
use std::time::UNIX_EPOCH;

use jiff::tz::Offset;
use lofty::file::{FileType, TaggedFile, TaggedFileExt};
use lofty::prelude::*;
use lofty::properties::FileProperties;
use lofty::tag::Tag;
use lofty::tag::items::Timestamp;

use super::{rating_tags, role_tags};
use netodia_artwork::media::image::artwork;
use netodia_core::entities::artist::ArtistCredit;
use netodia_core::entities::genre::GenreList;
use netodia_core::entities::scan::{ExtractedMetadata, ReleaseTags, SortTags};
use netodia_core::error::AppError;
use netodia_core::utils::format_rfc3339;

/// Compute a full BLAKE3 hash of a file (64-char hex string).
/// Uses `update_reader` for optimized streaming I/O with SIMD-friendly buffering.
pub fn compute_file_hash(path: &Path) -> Result<String, AppError> {
    let mut file = std::fs::File::open(path).map_err(|e| {
        AppError::metadata(format!("Failed to open {} for hashing", path.display()), e)
    })?;
    let mut hasher = blake3::Hasher::new();
    hasher
        .update_reader(&mut file)
        .map_err(|e| AppError::metadata(format!("Failed to hash {}", path.display()), e))?;
    Ok(hasher.finalize().to_hex().to_string())
}

/// Format an already-fetched `Metadata`'s modification time as an RFC 3339
/// string. Returns `None` if the mtime is unavailable or out of range.
///
/// This is the single source of truth for the mtime string format. Callers
/// that compare against a stored `date_modified` (e.g. the incremental-scan
/// filter `scanner::track_is_current`) must derive their value through here
/// so the formats can't drift apart. Takes `&Metadata` so a caller that
/// already `stat`-ed the file doesn't pay for a second syscall.
pub fn date_modified_from_metadata(meta: &std::fs::Metadata) -> Option<String> {
    let mtime = meta.modified().ok()?;
    mtime.duration_since(UNIX_EPOCH).ok()?;
    jiff::Timestamp::try_from(mtime).ok().map(|stamp| format_rfc3339(stamp, Offset::UTC))
}

/// Extract the file's modification time as an RFC 3339 string.
/// Returns `None` if the file metadata is unavailable or the timestamp is out of range.
pub fn extract_date_modified(path: &Path) -> Option<String> {
    std::fs::metadata(path).ok().as_ref().and_then(date_modified_from_metadata)
}

/// The container `path` holds, named by its header alone.
///
/// `FileType::from_buffer` is the strict half of lofty's sniffing, and the strictness is
/// the point. `Probe::guess_file_type` falls through to scanning the first kilobyte for
/// an MPEG frame sync, which arbitrary binary contains. A Matroska file comes back
/// confidently labelled AAC, carrying a sample rate and duration read out of the middle
/// of somebody's audio. A header matching nothing has to stay unidentified.
///
/// Reads what lofty's own sniffer reads: its longest check reaches byte 36.
///
/// **A fallback, never an identifier.** `from_buffer` makes no attempt to search past a leading
/// `ID3v2` tag, so it answers `None` for the great majority of MP3s, which open with one. That is
/// harmless where [`read_tags`] asks it, once the extension has resolved to nothing. Reached for
/// as a primary gate it silently refuses nearly every MP3 there is.
fn sniff_file_type(path: &Path) -> Option<FileType> {
    const SNIFF_BYTES: usize = 36;

    let mut head = Vec::with_capacity(SNIFF_BYTES);
    std::fs::File::open(path).ok()?.take(SNIFF_BYTES as u64).read_to_end(&mut head).ok()?;
    FileType::from_buffer(&head)
}

/// The channel count `path`'s Opus identification packet states, or `None` where the file is not
/// laid out the way an encoder writes one.
///
/// Derived rather than assumed, because an Ogg page header runs 27 bytes plus one per segment
/// and a packet laced into two would walk past a fixed offset. Anything this cannot make sense
/// of goes to lofty, which is where every other malformed header already goes.
fn opus_channel_count(path: &Path) -> Option<u8> {
    const PAGE_HEADER: usize = 27;
    const SEGMENT_COUNT_AT: usize = 26;
    const MAGIC: &[u8] = b"OpusHead";
    /// The longest a segment table runs, plus the version byte and the channel count behind the
    /// magic.
    const HEAD_BYTES: usize = PAGE_HEADER + 255 + 10;

    let mut head = Vec::with_capacity(HEAD_BYTES);
    std::fs::File::open(path).ok()?.take(HEAD_BYTES as u64).read_to_end(&mut head).ok()?;

    let packet = head.get(PAGE_HEADER + usize::from(*head.get(SEGMENT_COUNT_AT)?)..)?;
    let rest = packet.strip_prefix(MAGIC)?;
    // Version, then the count.
    rest.get(1).copied()
}

/// How much of a file [`read_tags`] is being asked for.
///
/// Both halves lofty can be talked out of are expensive and neither is optional by default:
/// embedded pictures are the larger part of a parse, and `read_properties` costs a full frame
/// scan on a headerless VBR MP3, which is the shape a duration has to be counted out of.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum TagScope {
    /// Tags, embedded pictures and the technical properties: what the scanner ingests.
    Full,
    /// Tags and properties. For a rescan, which already has the pictures.
    NoArtwork,
    /// Tags alone: no pictures, and no frame scan for duration or bitrate.
    TagsOnly,
}

/// Probe `path` for tags.
///
/// lofty keys `Probe::open` on the extension, and its map of those is narrower than what
/// its parsers cover: `.oga` resolves to nothing there, so an Ogg Vorbis file named that
/// way reads as an unknown format. Asking the header covers it. Only asked when the
/// extension resolved to nothing, so every file that parses today still parses the same
/// way, and the extra open stays off the scan's hot path.
///
/// Every lofty open in the tree comes through here, `media::ingest::tag_writer` included: a file the
/// scan identifies by its header and the tag editor refuses by its extension is a track whose
/// tags are visible and unsavable.
pub fn read_tags(path: &Path, scope: TagScope) -> Result<TaggedFile, AppError> {
    let parse_opts = lofty::config::ParseOptions::new()
        .read_cover_art(scope == TagScope::Full)
        .read_properties(scope != TagScope::TagsOnly);

    let mut probe = lofty::probe::Probe::open(path)
        .map_err(|e| AppError::metadata(format!("Failed to open {}", path.display()), e))?
        .options(parse_opts);

    if probe.file_type().is_none()
        && let Some(sniffed) = sniff_file_type(path)
    {
        probe = probe.set_file_type(sniffed);
    }

    // `lofty::ogg::opus::properties` guards a mapping family above 1 and a channel count too
    // large for the family named, and then hands the count to `ChannelMask::from_opus_channels`,
    // whose arms run 1 to 8 and whose `expect` takes everything else. `panic = "abort"` leaves
    // nothing to catch, so one crafted or truncated file in a watched folder ends the process
    // mid-scan. Checked here rather than in [`sniff_file_type`], which only runs once the
    // extension has resolved to nothing and so never sees a file named `.opus`.
    if matches!(probe.file_type(), Some(FileType::Opus)) && opus_channel_count(path) == Some(0) {
        return Err(AppError::metadata_msg(format!("{} states no audio channels", path.display())));
    }

    probe
        .read()
        .map_err(|e| AppError::metadata(format!("Failed to read tags from {}", path.display()), e))
}

/// Parse a `ReplayGain` gain string like "-6.50 dB" to f64. Rejects non-finite
/// values (`"nan"`, `"inf"` parse successfully as floats in Rust) so a malformed
/// tag can't poison the playback DSP — the value is baked into the audio source
/// and a `NaN`/`inf` gain would render the track as silence.
fn parse_replaygain_gain(s: &str) -> Option<f64> {
    s.trim().trim_end_matches("dB").trim().parse::<f64>().ok().filter(|v| v.is_finite())
}

/// Parse a `ReplayGain` peak string (linear scale, e.g. "0.988553") to f64.
/// Rejects non-finite values for the same reason as the gain parser.
fn parse_replaygain_peak(s: &str) -> Option<f64> {
    s.trim().parse::<f64>().ok().filter(|v| v.is_finite())
}

/// What an `R128_*_GAIN` value has to move by to mean the same thing as a `REPLAYGAIN_*` one.
///
/// EBU R128 normalises to −23 LUFS where `ReplayGain` 2.0 normalises to −18, so a gain written
/// against the first plays this much under a library normalised against the second. RFC 7845
/// §5.2 fixes the reference the tag is written against, which is what makes the distance a
/// property of the format rather than of whichever tagger wrote it.
const R128_TO_REPLAYGAIN_DB: f64 = 5.0;

/// Parse an `R128_*_GAIN` value — Q7.8 fixed-point dB, e.g. "-1280" — restated against
/// `ReplayGain`'s reference so it can share the columns and the DSP.
///
/// `i16` because RFC 7845 §5.2 states the field as one, which caps the answer at ±128 dB: no
/// non-finite guard is owed, unlike the two above, and no absurd one can reach the DSP either.
fn parse_r128_gain(s: &str) -> Option<f64> {
    let q7_8 = s.trim().parse::<i16>().ok()?;
    Some(f64::from(q7_8) / 256.0 + R128_TO_REPLAYGAIN_DB)
}

/// What [`extract`] does with a file it can hash but whose tags won't parse.
#[derive(Clone, Copy)]
enum OnUnreadableTags {
    Fail,
    FilenameRow,
}

/// Read a file's tags, properties and artwork into a row.
///
/// Fails if the tags won't parse. Callers that write a file and re-read it to refresh
/// its row want exactly that: a row built from a parse that didn't happen would blank
/// the track instead of reporting the failure.
pub fn extract_metadata(
    path: &Path,
    artwork_dir: &Path,
    cover_cache: &artwork::CoverCache,
    skip_artwork: bool,
) -> Result<ExtractedMetadata, AppError> {
    extract(path, artwork_dir, cover_cache, skip_artwork, OnUnreadableTags::Fail)
}

/// As [`extract_metadata`], but a file whose tags won't parse still yields a row, titled
/// from its filename, with external artwork and a decoder-probed duration if either is
/// there to be had.
///
/// For the scan paths, where the alternative is the file disappearing: a container with
/// no tag reader (Matroska, CAF) and one with tags too broken to parse both arrive here,
/// and dropping either leaves a file sitting in a watched folder that the library never
/// mentions. The hash above the parse is what makes this safe to do blind. It reads the
/// whole file, so anything that gets past it is readable and the parse failure is the
/// format's, not the disk's.
pub fn extract_or_filename_row(
    path: &Path,
    artwork_dir: &Path,
    cover_cache: &artwork::CoverCache,
    skip_artwork: bool,
) -> Result<ExtractedMetadata, AppError> {
    extract(path, artwork_dir, cover_cache, skip_artwork, OnUnreadableTags::FilenameRow)
}

fn extract(
    path: &Path,
    artwork_dir: &Path,
    cover_cache: &artwork::CoverCache,
    skip_artwork: bool,
    on_unreadable: OnUnreadableTags,
) -> Result<ExtractedMetadata, AppError> {
    // Only allocate the fallback name if a tag title is actually missing — for
    // a tagged music library this avoids ~1 String allocation per scanned file
    // on the hot scan path.
    let file_name = || path.file_stem().and_then(|s| s.to_str()).unwrap_or("Unknown").to_owned();

    let (file_size, date_modified) = file_stat(path);
    let file_hash = compute_file_hash(path)?;
    let scope = if skip_artwork { TagScope::NoArtwork } else { TagScope::Full };
    let tagged_file = open_tags(path, scope, on_unreadable)?;
    let audio = AudioProperties::read(path, tagged_file.as_ref());

    // Try to read tags - check all tag types and pick the first one with data
    let tag =
        tagged_file.as_ref().and_then(|tagged| tagged.primary_tag().or_else(|| tagged.first_tag()));

    // Extract artwork: check external cover files first, then embedded tag
    let artwork_path = if skip_artwork {
        None
    } else {
        artwork::find_and_cache_artwork(path, tag, artwork_dir, cover_cache)
    };

    // Trim + drop whitespace-only tags. Some ripped/transcoded files carry an `Album`/`Genre`
    // field that's nothing but spaces; left as-is they bypass the `is_empty()` guard in
    // `upsert_album`/`upsert_genre` and create ghost entity rows. `read_credit` below owes the
    // same for the artist fields, where the ghost rows also cost a futile image fetch.
    let title = text(tag, ItemKey::TrackTitle).unwrap_or_else(file_name);

    // Their own lines rather than slots in a tuple: an artist field is a pair of tags read
    // together, and neither half alone says what the credit is.
    let artist = read_credit(tag, ItemKey::TrackArtist, ItemKey::TrackArtists);
    let album_artist = read_credit(tag, ItemKey::AlbumArtist, ItemKey::AlbumArtists);

    // Both halves of the release date are kept: the year every index and smart-playlist rule is
    // built on, and the month and day beside it.
    let released = release_timestamp(tag);
    let originally_released = read_timestamp(tag, ItemKey::OriginalReleaseDate);

    // The conversion this fronts is argued in `rating_tags`, where every format's shape is in
    // view; the role credits' per-format holes are argued in `role_tags` for the same reason.
    let rating = tag.and_then(rating_tags::stars_from_tag);
    let credits = tag.map(role_tags::read_roles).unwrap_or_default();

    Ok(ExtractedMetadata {
        artist_mbids: read_credit_mbids(tag, ItemKey::MusicBrainzArtistId, artist.artists().len()),
        album_artist_mbids: read_credit_mbids(
            tag,
            ItemKey::MusicBrainzReleaseArtistId,
            album_artist.artists().len(),
        ),
        title,
        artist,
        album_artist,
        album: text(tag, ItemKey::AlbumTitle),
        genres: read_genres(tag),
        credits,
        sort: read_sort_tags(tag),
        release: read_release_tags(tag),
        track_number: count(tag.and_then(Tag::track)),
        track_total: count(tag.and_then(Tag::track_total)),
        disc_number: count(tag.and_then(Tag::disk)),
        disc_total: count(tag.and_then(Tag::disk_total)),
        disc_subtitle: text(tag, ItemKey::SetSubtitle),
        subtitle: text(tag, ItemKey::TrackSubtitle),
        release_date: released.map(|ts| ts.to_string()),
        year: released.map(|ts| i32::from(ts.year)),
        original_date: originally_released.map(|ts| ts.to_string()),
        original_year: originally_released.map(|ts| i32::from(ts.year)),
        comment: text(tag, ItemKey::Comment),
        bpm: read_bpm(tag),
        initial_key: text(tag, ItemKey::InitialKey),
        mood: text(tag, ItemKey::Mood),
        grouping: text(tag, ItemKey::ContentGroup),
        work: text(tag, ItemKey::Work),
        movement: text(tag, ItemKey::Movement),
        movement_number: number(tag, ItemKey::MovementNumber),
        movement_total: number(tag, ItemKey::MovementTotal),
        language: text(tag, ItemKey::Language),
        copyright: text(tag, ItemKey::CopyrightMessage),
        isrc: text(tag, ItemKey::Isrc),
        musicbrainz_track_id: text(tag, ItemKey::MusicBrainzRecordingId),
        musicbrainz_release_id: text(tag, ItemKey::MusicBrainzReleaseId),
        musicbrainz_release_track_id: text(tag, ItemKey::MusicBrainzTrackId),
        replaygain_track_gain: replaygain_gain(
            tag,
            ItemKey::ReplayGainTrackGain,
            ItemKey::R128TrackGain,
        ),
        replaygain_track_peak: replaygain_peak(tag, ItemKey::ReplayGainTrackPeak),
        replaygain_album_gain: replaygain_gain(
            tag,
            ItemKey::ReplayGainAlbumGain,
            ItemKey::R128AlbumGain,
        ),
        replaygain_album_peak: replaygain_peak(tag, ItemKey::ReplayGainAlbumPeak),
        rating,
        duration_ms: audio.duration_ms,
        codec: audio.codec,
        bitrate: audio.bitrate,
        channels: audio.channels,
        sample_rate: audio.sample_rate,
        bit_depth: audio.bit_depth,
        file_size,
        file_hash,
        date_modified,
        artwork_path,
    })
}

/// The file's size and its stored mtime string, off one `stat`.
///
/// Derived from the `Metadata` already in hand — `extract_date_modified` would
/// `stat` the file a second time. This is exactly what
/// `date_modified_from_metadata` exists for; `scanner::track_is_current` is the
/// other caller that already holds one.
fn file_stat(path: &Path) -> (i64, Option<String>) {
    let fs_meta = std::fs::metadata(path);
    let file_size = fs_meta.as_ref().map_or(0, |m| i64::try_from(m.len()).unwrap_or(i64::MAX));
    let date_modified = fs_meta.as_ref().ok().and_then(date_modified_from_metadata);
    (file_size, date_modified)
}

/// The parsed file, or `None` where the tags won't parse and the caller keeps a filename row.
fn open_tags(
    path: &Path,
    scope: TagScope,
    on_unreadable: OnUnreadableTags,
) -> Result<Option<TaggedFile>, AppError> {
    match read_tags(path, scope) {
        Ok(tagged) => Ok(Some(tagged)),
        Err(e) => match on_unreadable {
            OnUnreadableTags::Fail => Err(e),
            OnUnreadableTags::FilenameRow => {
                log::debug!(
                    "{}; keeping a filename-derived row",
                    netodia_core::error::describe(&e)
                );
                Ok(None)
            }
        },
    }
}

/// What the stream is rather than what its tags say.
struct AudioProperties {
    duration_ms: i64,
    codec: Option<String>,
    bitrate: Option<i32>,
    channels: Option<i32>,
    sample_rate: Option<i32>,
    bit_depth: Option<i32>,
}

impl AudioProperties {
    fn read(path: &Path, tagged_file: Option<&TaggedFile>) -> Self {
        let properties = tagged_file.map(TaggedFile::properties);
        let duration_ms = match properties {
            Some(props) => i64::try_from(props.duration().as_millis()).unwrap_or(i64::MAX),
            // Lofty reports duration off the parse that just failed, so the decoder is the
            // only thing left that knows. Still `0` where it can't say either.
            None => netodia_audio::player::source::file_decode::probe_duration(path)
                .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX)),
        };
        Self {
            duration_ms,
            codec: tagged_file.map(|tagged| format!("{:?}", tagged.file_type())),
            bitrate: properties
                .and_then(|props| props.overall_bitrate().or(props.audio_bitrate()))
                .map(|br| i32::try_from(br).unwrap_or(i32::MAX)),
            channels: properties.and_then(FileProperties::channels).map(i32::from),
            sample_rate: properties
                .and_then(FileProperties::sample_rate)
                .map(|rate| i32::try_from(rate).unwrap_or(i32::MAX)),
            bit_depth: properties.and_then(FileProperties::bit_depth).map(i32::from),
        }
    }
}

/// `ItemKey::Bpm` has NO `ID3v2` mapping — MP3 / WAV / AIFF keep BPM in `TBPM`, which lofty
/// exposes as `IntegerBpm`. Reading only `Bpm` therefore misses it on every `ID3v2` file,
/// including the ones `tag_writer`'s BPM write puts there. Prefer the decimal key (Vorbis
/// `BPM`, MP4 freeform); fall back to the integer.
fn read_bpm(tag: Option<&Tag>) -> Option<f64> {
    text(tag, ItemKey::Bpm)
        .or_else(|| text(tag, ItemKey::IntegerBpm))
        .and_then(|s| s.parse::<f64>().ok())
        .filter(|v| v.is_finite())
}

/// A `ReplayGain` gain, or its `R128` counterpart restated against the same reference.
///
/// Opus carries `R128_*_GAIN` where everything else carries `REPLAYGAIN_*`, and read as
/// untagged it would play at unity against a normalised library. `REPLAYGAIN_*` wins where
/// both are present: it is already written against the reference the columns mean, so it
/// owes no conversion.
fn replaygain_gain(tag: Option<&Tag>, replaygain: ItemKey, r128: ItemKey) -> Option<f64> {
    text(tag, replaygain)
        .as_deref()
        .and_then(parse_replaygain_gain)
        .or_else(|| text(tag, r128).as_deref().and_then(parse_r128_gain))
}

/// Neither peak has an R128 counterpart — R128 defines none — and the prevent-clipping path
/// already treats a peak it doesn't know as no ceiling.
fn replaygain_peak(tag: Option<&Tag>, key: ItemKey) -> Option<f64> {
    text(tag, key).as_deref().and_then(parse_replaygain_peak)
}

/// One tag value, trimmed, `None` when absent or blank.
///
/// Takes `Option<&Tag>` so a file whose tags wouldn't parse reads as a file with none, which is
/// what the filename-row fallback needs — every field below is one line rather than a slot in a
/// tuple whose `else` arm has to spell the same count of `None`s.
fn text(tag: Option<&Tag>, key: ItemKey) -> Option<String> {
    tag.and_then(|tag| tag.get_string(key)).map(|s| s.trim().to_owned()).filter(|s| !s.is_empty())
}

/// [`text`] parsed as a whole number, for the counts that have no [`Accessor`] getter.
fn number(tag: Option<&Tag>, key: ItemKey) -> Option<i32> {
    text(tag, key).and_then(|s| s.parse().ok())
}

/// Whether a flag tag is set. Anything but a `0` or a `false` counts, `TCMP` being written as `1`
/// and `COMPILATION` as either.
///
/// **Case-folded, and it is the false half that has to be.** `upsert_album`'s `is_compilation` is
/// an `OR`, so nothing but an explicit Edit-Tags clear ever lowers the flag again — one file whose
/// tagger spelled it `FALSE` would mark the whole release, and no rescan could repair it. A
/// spelling this doesn't know still fails toward "set", which is the direction a *present* flag
/// argues for.
fn flag(tag: Option<&Tag>, key: ItemKey) -> bool {
    text(tag, key).is_some_and(|value| value != "0" && !value.eq_ignore_ascii_case("false"))
}

/// A count lofty already parsed, saturated rather than wrapped.
fn count(value: Option<u32>) -> Option<i32> {
    value.map(|n| i32::try_from(n).unwrap_or(i32::MAX))
}

/// A whole date under one key. `BestAttempt` parsing, so a year-only tag is a year-only
/// [`Timestamp`] and renders back as the four digits it came in as.
fn read_timestamp(tag: Option<&Tag>, key: ItemKey) -> Option<Timestamp> {
    text(tag, key).and_then(|value| value.parse().ok())
}

/// Where a release date can sit, in the order it is taken.
///
/// `TDRL`/`RELEASEDATE` is the explicit answer, `TDRC`/`DATE` the one Picard actually writes, and
/// `YEAR` a Vorbis-only spelling that `Accessor::date` keeps an arm for and some rippers still
/// emit alone. Shared with [`super::tag_writer`], which clears the whole list before writing so an
/// edit cannot land behind a key read first.
pub(super) const RELEASE_DATE_KEYS: [ItemKey; 3] =
    [ItemKey::ReleaseDate, ItemKey::RecordingDate, ItemKey::Year];

/// The release date a file carries, under whichever of [`RELEASE_DATE_KEYS`] it used.
pub(super) fn release_timestamp(tag: Option<&Tag>) -> Option<Timestamp> {
    RELEASE_DATE_KEYS.into_iter().find_map(|key| read_timestamp(tag, key))
}

/// Every genre the file names.
///
/// A repeated `GENRE` is the multi-value form and needs no splitting. A *single* value is split on
/// `;`, and on nothing else: that is the separator Picard flattens a list to when the frame cannot
/// hold one, which is every ID3v2.3 file. Not `/` — `ID3v1`'s own genre list contains `Pop/Funk`, so
/// a slash is genuinely ambiguous where a semicolon is not — and not `,` or `&`, which live inside
/// names like `Drum & Bass`.
fn read_genres(tag: Option<&Tag>) -> GenreList {
    let Some(tag) = tag else {
        return GenreList::default();
    };
    let values = trimmed_values(tag, ItemKey::Genre);
    let names = match values.as_slice() {
        [single] => {
            single.split(';').map(str::trim).filter(|s| !s.is_empty()).map(str::to_owned).collect()
        }
        _ => values,
    };
    GenreList::new(names)
}

fn read_sort_tags(tag: Option<&Tag>) -> SortTags {
    SortTags {
        artist: text(tag, ItemKey::TrackArtistSortOrder),
        album_artist: text(tag, ItemKey::AlbumArtistSortOrder),
        album: text(tag, ItemKey::AlbumTitleSortOrder),
        title: text(tag, ItemKey::TrackTitleSortOrder),
    }
}

fn read_release_tags(tag: Option<&Tag>) -> ReleaseTags {
    ReleaseTags {
        label: text(tag, ItemKey::Label),
        catalog_number: text(tag, ItemKey::CatalogNumber),
        barcode: text(tag, ItemKey::Barcode),
        media: text(tag, ItemKey::OriginalMediaType),
        release_type: text(tag, ItemKey::MusicBrainzReleaseType),
        release_country: text(tag, ItemKey::ReleaseCountry),
        musicbrainz_release_group_id: text(tag, ItemKey::MusicBrainzReleaseGroupId),
        is_compilation: flag(tag, ItemKey::FlagCompilation),
    }
}

/// The `MusicBrainz` ids listed in parallel with a credit's names.
///
/// **All of them or none.** Picard writes one id per credited artist in the same order, so the
/// pairing is positional and a file whose counts disagree has no alignment left to trust — and a
/// misaligned id stamps the wrong `MBID` onto a real artist, which nothing downstream can detect
/// or undo. A solo credit is the common case and still passes.
fn read_credit_mbids(tag: Option<&Tag>, key: ItemKey, credited: usize) -> Vec<String> {
    let Some(tag) = tag else {
        return Vec::new();
    };
    let ids = trimmed_values(tag, key);
    if ids.len() == credited { ids } else { Vec::new() }
}

/// A file's two artist credits, over a tag already in hand.
///
/// The file is the authority, which is what a database seeded from `artist_id` alone cannot be.
/// Takes the tag rather than a path because its one caller — the Edit-Tags dialog — is opening the
/// file for the lyrics tag anyway.
pub fn credits_from_tag(tag: Option<&Tag>) -> (ArtistCredit, ArtistCredit) {
    (
        read_credit(tag, ItemKey::TrackArtist, ItemKey::TrackArtists),
        read_credit(tag, ItemKey::AlbumArtist, ItemKey::AlbumArtists),
    )
}

/// What one artist field reads as: the credit behind it, and the string that renders.
///
/// `ARTISTS` wins where it exists. Failing that, a multi-value `ARTIST` — a NUL-separated `TPE1`,
/// a repeated Vorbis field — is the same list of names with nothing said about how they join. A
/// *single* value is one artist whatever delimiters it contains, which is what the list tag
/// exists for: `AC/DC` and `Earth, Wind & Fire` are one name each, and the exceptions list that
/// splitting on punctuation would need is not one anybody can finish.
///
/// The string comes back rendered from the credit rather than copied off the tag, so the column
/// and the join rows written beside it cannot disagree.
fn read_credit(tag: Option<&Tag>, printed_key: ItemKey, list_key: ItemKey) -> ArtistCredit {
    let Some(tag) = tag else {
        return ArtistCredit::default();
    };
    let listed = unflattened(trimmed_values(tag, list_key));
    let printed = trimmed_values(tag, printed_key);
    let credit_line = printed.first().cloned().unwrap_or_default();
    let names = if listed.is_empty() { printed } else { listed };

    ArtistCredit::from_tags(&credit_line, &names)
}

/// A list tag that arrived as one `"; "`-joined string, split back into the names it holds.
///
/// ID3v2.3 has no multi-value frame, so Picard writes `TXXX:Artists` as `"Alice; Bob"` — and that
/// is most MP3s in the wild. Left whole it reads as one artist named that, which is the exact bug
/// the `ARTISTS` tag exists to prevent.
///
/// Narrower than reversing [`read_credit`]'s never-split rule, and deliberately: this runs on the
/// **list** key only, where a single value can only ever be a flattened list, never on the printed
/// key where `AC/DC` and `Earth, Wind & Fire` live. The separator is the one Picard documents for
/// the purpose, with the space required — a bare `;` inside a single name stays inside it.
fn unflattened(values: Vec<String>) -> Vec<String> {
    let [single] = values.as_slice() else {
        return values;
    };
    if !single.contains("; ") {
        return values;
    }
    single.split("; ").map(str::trim).filter(|s| !s.is_empty()).map(str::to_owned).collect()
}

/// Every value under `key`, trimmed, blanks dropped. The one multi-value read in the tree —
/// [`super::role_tags`] unpacks its ten keys through it too.
///
/// The blanks are not hypothetical: a whitespace-only artist field is common enough in ripped
/// libraries to have its own guard downstream, and a NUL-terminated UTF-8 frame leaves an empty
/// tail value behind it.
pub(super) fn trimmed_values(tag: &Tag, key: ItemKey) -> Vec<String> {
    tag.get_strings(key).map(str::trim).filter(|s| !s.is_empty()).map(str::to_owned).collect()
}

#[cfg(test)]
#[path = "tests/metadata_tests.rs"]
mod tests;
