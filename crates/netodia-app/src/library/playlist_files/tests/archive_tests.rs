use std::io::Cursor;

use jiff::civil::datetime;

use super::*;

/// What a zip entry is written as, for the archives production's `write` can't produce.
enum Crafted<'a> {
    File(&'a str, &'a [u8]),
    Directory(&'a str),
}

fn crafted(entries: &[Crafted<'_>]) -> Result<Vec<u8>, AppError> {
    let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
    let mut archive = ZipWriter::new(Cursor::new(Vec::new()));
    for entry in entries {
        match *entry {
            Crafted::File(name, bytes) => {
                archive.start_file(name, options).map_err(AppError::io_source)?;
                archive.write_all(bytes)?;
            }
            Crafted::Directory(name) => {
                archive.add_directory(name, options).map_err(AppError::io_source)?;
            }
        }
    }
    Ok(archive.finish().map_err(AppError::io_source)?.into_inner())
}

fn entry(name: &str, text: &str) -> Entry {
    Entry { name: name.to_owned(), text: text.to_owned() }
}

fn written(entries: &[Entry], modified: DateTime) -> Result<Vec<u8>, AppError> {
    let mut out = Cursor::new(Vec::new());
    write(&mut out, entries, modified)?;
    Ok(out.into_inner())
}

/// Every entry's name and text, in archive order.
fn read_back(bytes: Vec<u8>) -> Result<(Vec<(String, String)>, u32), AppError> {
    let contents = read(Cursor::new(bytes))?;
    let entries = contents.entries.into_iter().map(|entry| (entry.name, entry.text)).collect();
    Ok((entries, contents.unreadable))
}

fn modified_of_first_entry(bytes: Vec<u8>) -> Result<Option<zip::DateTime>, AppError> {
    let mut archive = ZipArchive::new(Cursor::new(bytes)).map_err(AppError::io_source)?;
    let file = archive.by_index(0).map_err(AppError::io_source)?;
    Ok(file.last_modified())
}

// --- TextBudget ---------------------------------------------------------------

#[test]
fn the_budget_spends_exactly_its_cap() {
    let mut budget = TextBudget::default();
    let spent = budget.charge(usize::try_from(MAX_TEXT_BYTES).unwrap_or(usize::MAX));
    assert!(spent.is_ok());
    assert_eq!(budget.remaining(), 0);
}

#[test]
fn a_charge_past_what_is_left_is_refused_and_spends_nothing() {
    let mut budget = TextBudget::default();
    let over = budget.charge(usize::try_from(MAX_TEXT_BYTES + 1).unwrap_or(usize::MAX));
    assert!(matches!(over, Err(TooLarge)));
    assert_eq!(budget.remaining(), MAX_TEXT_BYTES);
}

#[test]
fn a_refused_charge_leaves_room_for_a_smaller_one() {
    let mut budget = TextBudget::default();
    let half = usize::try_from(MAX_TEXT_BYTES / 2).unwrap_or(usize::MAX);
    assert!(budget.charge(half).is_ok());
    assert!(budget.charge(half + 1).is_err());
    assert!(budget.charge(half).is_ok());
    assert_eq!(budget.remaining(), 0);
}

#[test]
fn a_spent_budget_still_fits_nothing_but_nothing() {
    let mut budget = TextBudget::default();
    assert!(budget.charge(usize::try_from(MAX_TEXT_BYTES).unwrap_or(usize::MAX)).is_ok());
    assert!(budget.charge(0).is_ok(), "an empty playlist costs nothing");
    assert!(budget.charge(1).is_err());
}

// --- write / read --------------------------------------------------------------

#[test]
fn an_archive_reads_back_the_entries_it_was_written_with_in_order() -> Result<(), AppError> {
    let bytes = written(
        &[entry("Road.m3u8", "#EXTM3U\n#PLAYLIST:Road\n"), entry("Mix.m3u8", "#EXTM3U\n")],
        DateTime::default(),
    )?;

    let (entries, unreadable) = read_back(bytes)?;

    assert_eq!(
        entries,
        [
            ("Road.m3u8".to_owned(), "#EXTM3U\n#PLAYLIST:Road\n".to_owned()),
            ("Mix.m3u8".to_owned(), "#EXTM3U\n".to_owned()),
        ]
    );
    assert_eq!(unreadable, 0);
    Ok(())
}

#[test]
fn an_archive_written_with_nothing_reads_as_no_entries() -> Result<(), AppError> {
    let (entries, unreadable) = read_back(written(&[], DateTime::default())?)?;
    assert!(entries.is_empty());
    assert_eq!(unreadable, 0);
    Ok(())
}

#[test]
fn a_date_the_format_cannot_hold_is_stored_as_its_epoch() -> Result<(), AppError> {
    for modified in [datetime(1970, 1, 1, 0, 0, 0, 0), datetime(2200, 6, 1, 12, 0, 0, 0)] {
        let bytes = written(&[entry("a.m3u8", "#EXTM3U\n")], modified)?;
        assert_eq!(modified_of_first_entry(bytes)?, Some(zip::DateTime::default()), "{modified}");
    }
    Ok(())
}

#[test]
fn a_date_the_format_can_hold_is_stored_as_given() -> Result<(), AppError> {
    // An even second, the format keeping two-second steps.
    let bytes = written(&[entry("a.m3u8", "#EXTM3U\n")], datetime(2026, 9, 14, 12, 34, 56, 0))?;

    let stored = modified_of_first_entry(bytes)?
        .map(|dt| (dt.year(), dt.month(), dt.day(), dt.hour(), dt.minute(), dt.second()));

    assert_eq!(stored, Some((2026, 9, 14, 12, 34, 56)));
    Ok(())
}

#[test]
fn entries_that_are_not_playlists_are_neither_read_nor_counted() -> Result<(), AppError> {
    let bytes = crafted(&[
        Crafted::File("README.txt", b"notes"),
        Crafted::File("cover.jpg", &[0xff, 0xd8, 0xff]),
        Crafted::File("Road.m3u8", b"#EXTM3U\n"),
    ])?;

    let (entries, unreadable) = read_back(bytes)?;

    assert_eq!(entries, [("Road.m3u8".to_owned(), "#EXTM3U\n".to_owned())]);
    assert_eq!(unreadable, 0);
    Ok(())
}

#[test]
fn a_playlist_extension_is_recognised_in_any_case() -> Result<(), AppError> {
    let bytes = crafted(&[
        Crafted::File("SHOUTED.M3U8", b"#EXTM3U\n"),
        Crafted::File("plain.m3u", b"#EXTM3U\n"),
    ])?;

    let (entries, _) = read_back(bytes)?;

    let names: Vec<&str> = entries.iter().map(|(name, _)| name.as_str()).collect();
    assert_eq!(names, ["SHOUTED.M3U8", "plain.m3u"]);
    Ok(())
}

#[test]
fn a_folder_named_like_a_playlist_is_skipped_rather_than_counted() -> Result<(), AppError> {
    let bytes =
        crafted(&[Crafted::Directory("Old.m3u8/"), Crafted::File("New.m3u8", b"#EXTM3U\n")])?;

    let (entries, unreadable) = read_back(bytes)?;

    assert_eq!(entries, [("New.m3u8".to_owned(), "#EXTM3U\n".to_owned())]);
    assert_eq!(unreadable, 0, "a folder is not a playlist that failed to read");
    Ok(())
}

/// Used, the name would choose the folder its playlist's relative entries resolve against.
#[test]
fn an_entry_named_outside_the_archive_counts_as_unreadable() -> Result<(), AppError> {
    let bytes = crafted(&[
        Crafted::File("../escaped.m3u8", b"#EXTM3U\n"),
        Crafted::File("Kept.m3u8", b"#EXTM3U\n"),
    ])?;

    let (entries, unreadable) = read_back(bytes)?;

    assert_eq!(entries, [("Kept.m3u8".to_owned(), "#EXTM3U\n".to_owned())]);
    assert_eq!(unreadable, 1);
    Ok(())
}

#[test]
fn a_playlist_that_is_not_utf8_counts_as_unreadable() -> Result<(), AppError> {
    let bytes = crafted(&[Crafted::File("Latin1.m3u8", &[0x23, 0xff, 0xfe, 0x0a])])?;

    let (entries, unreadable) = read_back(bytes)?;

    assert!(entries.is_empty());
    assert_eq!(unreadable, 1);
    Ok(())
}

#[test]
fn bytes_that_are_not_a_zip_are_refused() {
    let refused = read(Cursor::new(b"#EXTM3U\n#PLAYLIST:Not a zip\n".to_vec()));
    assert!(matches!(refused, Err(AppError::Io(_))));
}

/// Points both of an archive's CRC-32 fields for its first entry at a value its bytes won't hash
/// to, so the entry inflates to its end and then fails its checksum.
fn corrupt_first_checksum(bytes: &mut [u8]) -> Result<(), AppError> {
    const LOCAL_HEADER_CRC: usize = 14;
    const CENTRAL_HEADER_CRC: usize = 16;
    const END_RECORD_LEN: usize = 22;
    const END_RECORD_DIRECTORY_OFFSET: usize = 16;

    let malformed = || AppError::Validation("crafted archive has an unexpected layout".to_owned());
    let end_record = bytes.len().checked_sub(END_RECORD_LEN).ok_or_else(malformed)?;
    let offset_field = end_record + END_RECORD_DIRECTORY_OFFSET;
    let directory = bytes
        .get(offset_field..offset_field + 4)
        .and_then(|field| <[u8; 4]>::try_from(field).ok())
        .map(u32::from_le_bytes)
        .and_then(|offset| usize::try_from(offset).ok())
        .ok_or_else(malformed)?;

    for field in [LOCAL_HEADER_CRC, directory + CENTRAL_HEADER_CRC] {
        let crc = bytes.get_mut(field..field + 4).ok_or_else(malformed)?;
        for byte in crc {
            *byte = !*byte;
        }
    }
    Ok(())
}

/// An entry that inflates and then fails its checksum has cost the whole read, so counting only
/// the entries that succeed would let an archive of corrupt entries expand without limit.
#[test]
fn an_entry_that_fails_its_checksum_still_spends_the_budget() -> Result<(), AppError> {
    let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
    let mut archive = ZipWriter::new(Cursor::new(Vec::new()));
    archive.start_file("Corrupt.m3u8", options).map_err(AppError::io_source)?;
    std::io::copy(&mut std::io::repeat(b'#').take(MAX_TEXT_BYTES - 4), &mut archive)?;
    archive.start_file("Small.m3u8", options).map_err(AppError::io_source)?;
    archive.write_all(b"#EXTM3U\n")?;
    let mut bytes = archive.finish().map_err(AppError::io_source)?.into_inner();
    corrupt_first_checksum(&mut bytes)?;

    let refused = read(Cursor::new(bytes));

    assert!(matches!(refused, Err(AppError::Validation(_))));
    Ok(())
}
