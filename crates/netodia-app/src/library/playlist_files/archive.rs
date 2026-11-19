//! The zip a multi-playlist export is saved as, and read back from.
//!
//! Only the container lives here: each entry's text is [`super::m3u`]'s, and which playlist
//! becomes which entry is the caller's. Nothing is ever extracted to disk, so an entry's name
//! matters only as the name its playlist falls back to and the folder its relative paths
//! resolve against.

use std::io::{Read, Seek, Write};
use std::path::Path;

use jiff::civil::DateTime;
use zip::result::DateTimeRangeError;
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipArchive, ZipWriter};

use super::{PLAYLIST_EXTENSIONS, has_extension};
use netodia_core::error::{AppError, describe};

/// How much playlist text one archive may expand to before it is refused whole.
///
/// A plain playlist file is bounded by its own size on disk and an archive is not, deflate being
/// built to expand. The budget counts bytes actually read rather than the sizes the archive
/// declares, which it is free to lie about. Set far past any real library, since landing on it
/// refuses the whole import, and export refuses the whole archive rather than write one past it.
pub const MAX_TEXT_BYTES: u64 = 64 * 1024 * 1024;

/// What is left of [`MAX_TEXT_BYTES`] for one archive. Export spends it the way import does, so
/// the app never writes an archive it would refuse to read back.
pub struct TextBudget {
    remaining: u64,
}

/// The playlists expand past [`MAX_TEXT_BYTES`].
#[derive(Debug)]
pub struct TooLarge;

impl Default for TextBudget {
    fn default() -> Self {
        Self { remaining: MAX_TEXT_BYTES }
    }
}

impl TextBudget {
    /// Spends `bytes` of playlist text, leaving the budget untouched when they don't fit.
    pub fn charge(&mut self, bytes: usize) -> Result<(), TooLarge> {
        let bytes = u64::try_from(bytes).unwrap_or(u64::MAX);
        self.remaining = self.remaining.checked_sub(bytes).ok_or(TooLarge)?;
        Ok(())
    }

    pub fn remaining(&self) -> u64 {
        self.remaining
    }
}

/// One playlist file inside an archive.
pub struct Entry {
    /// Its path inside the archive, which is where it lands once extracted.
    pub name: String,
    pub text: String,
}

/// The playlist files an archive held.
#[derive(Default)]
pub struct Contents {
    /// In archive order.
    pub entries: Vec<Entry>,
    /// Playlist entries that couldn't be read, skipped so the rest still import.
    pub unreadable: u32,
}

/// Zips `entries` into `out`, each dated `modified`.
///
/// A date the format can't hold (before 1980 or past 2107) takes the format's own epoch rather
/// than failing an export over a clock.
pub fn write<W: Write + Seek>(
    out: W,
    entries: &[Entry],
    modified: DateTime,
) -> Result<(), AppError> {
    let options = SimpleFileOptions::default()
        .compression_method(CompressionMethod::Deflated)
        .last_modified_time(zip_modified(modified).unwrap_or_default());

    let mut archive = ZipWriter::new(out);
    for entry in entries {
        archive.start_file(entry.name.as_str(), options).map_err(AppError::io_source)?;
        archive.write_all(entry.text.as_bytes())?;
    }
    archive.finish().map_err(AppError::io_source)?;
    Ok(())
}

fn zip_modified(modified: DateTime) -> Result<zip::DateTime, DateTimeRangeError> {
    zip::DateTime::from_date_and_time(
        modified.year().try_into()?,
        modified.month().try_into()?,
        modified.day().try_into()?,
        modified.hour().try_into()?,
        modified.minute().try_into()?,
        modified.second().try_into()?,
    )
}

/// Reads every `.m3u8` and `.m3u` entry out of an archive. Anything else in it is ignored.
///
/// # Errors
///
/// When the reader isn't a zip, or its playlists expand past [`MAX_TEXT_BYTES`]. A single entry
/// that won't read is counted in [`Contents::unreadable`] instead.
pub fn read<R: Read + Seek>(reader: R) -> Result<Contents, AppError> {
    let mut archive = ZipArchive::new(reader).map_err(AppError::io_source)?;
    let mut contents = Contents::default();
    let mut budget = TextBudget::default();

    for index in 0..archive.len() {
        let is_playlist = archive
            .name_for_index(index)
            .and_then(Result::ok)
            .is_some_and(|name| has_extension(Path::new(name.as_ref()), &PLAYLIST_EXTENSIONS));
        if !is_playlist {
            continue;
        }

        let mut bytes = Vec::new();
        let entry = read_entry(&mut archive, index, budget.remaining(), &mut bytes);

        // Charged before the result is looked at: an entry that inflates and then fails its
        // checksum has cost the work all the same.
        budget.charge(bytes.len()).map_err(|TooLarge| {
            AppError::Validation(format!("Playlist archive expands past {MAX_TEXT_BYTES} bytes"))
        })?;

        let name = match entry {
            Ok(Some(name)) => name,
            Ok(None) => continue,
            Err(e) => {
                log::warn!("playlist archive: entry {index} won't read: {}", describe(&e));
                contents.unreadable = contents.unreadable.saturating_add(1);
                continue;
            }
        };

        match String::from_utf8(bytes) {
            Ok(mut text) => {
                // Held beside every other entry until the import reaches it, and `read_to_end`
                // grows by doubling, so without this the budget could cost twice itself.
                text.shrink_to_fit();
                contents.entries.push(Entry { name, text });
            }
            Err(e) => {
                log::warn!("playlist archive: {name} is not UTF-8: {}", describe(&e));
                contents.unreadable = contents.unreadable.saturating_add(1);
            }
        }
    }
    Ok(contents)
}

/// Reads one entry into `bytes` through one byte past `limit`, so the caller can tell an entry that
/// stops on the budget from one that runs over it. A read failing partway leaves what it got in
/// `bytes`.
///
/// `None` for a directory. A name that climbs out of the archive or is absolute is an error, so it
/// is counted like any unreadable entry: used, it would pick the folder its playlist's relative
/// paths resolve against.
fn read_entry<R: Read + Seek>(
    archive: &mut ZipArchive<R>,
    index: usize,
    limit: u64,
    bytes: &mut Vec<u8>,
) -> Result<Option<String>, AppError> {
    let file = archive.by_index(index).map_err(AppError::io_source)?;
    if file.is_dir() {
        return Ok(None);
    }
    let Some(name) = file.enclosed_name() else {
        return Err(AppError::Validation(format!("Entry {index} is named outside the archive")));
    };

    file.take(limit.saturating_add(1)).read_to_end(bytes)?;
    Ok(Some(name.to_string_lossy().into_owned()))
}

#[cfg(test)]
#[path = "tests/archive_tests.rs"]
mod tests;
