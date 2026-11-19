use std::collections::HashSet;

use jiff::civil::datetime;

#[allow(clippy::wildcard_imports)]
use super::*;
use netodia_core::entities::smart_criteria::{Rule, RuleField, RuleOp, RuleValue};
use netodia_core::error::AppError;
use netodia_store::database::DbPool;
use netodia_store::database::queries;
use netodia_store::database::queries::fixtures::insert_test_track;

fn entry(path: &str, hash: Option<&str>) -> m3u::ParsedEntry {
    m3u::ParsedEntry {
        path: path.to_owned(),
        hash: hash.map(ToOwned::to_owned),
        duration_secs: None,
        display: None,
    }
}

/// Seed a pool with three tracks; returns `(db, id_a, id_b, id_c)`.
/// `make_test_metadata` sets `file_hash = blake3(title)`, so each track's
/// hash is deterministic from its title.
async fn seed() -> Result<(DbPool, i64, i64, i64), AppError> {
    let db = DbPool::test_pool().await?;
    queries::folder::insert_folder(&db, "/music", true).await?;
    let a =
        insert_test_track(&db, "/music/a.mp3", "Alpha Song", "Artist A", "Album", "Rock").await?;
    let b = insert_test_track(&db, "/music/b.mp3", "Beta Song", "Artist B", "Album", "Pop").await?;
    let c =
        insert_test_track(&db, "/music/c.mp3", "Gamma Song", "Artist A", "Album", "Rock").await?;
    Ok((db, a, b, c))
}

fn hash_of(title: &str) -> String {
    blake3::hash(title.as_bytes()).to_hex().to_string()
}

#[tokio::test]
async fn match_entries_by_path() -> Result<(), AppError> {
    let (db, a, b, c) = seed().await?;
    let entries =
        [entry("/music/a.mp3", None), entry("/music/b.mp3", None), entry("/music/c.mp3", None)];
    let out = match_entries(&db, &entries, None).await?;
    assert_eq!(out.matched_by_path, 3);
    assert_eq!(out.matched_by_hash, 0);
    assert_eq!(out.missing, 0);
    assert_eq!(out.ordered_ids, vec![a, b, c]);
    Ok(())
}

#[tokio::test]
async fn match_entries_by_hash_when_path_is_stale() -> Result<(), AppError> {
    let (db, a, _b, _c) = seed().await?;
    // Path no longer exists, but the hash matches track A (title "Alpha Song").
    let entries = [entry("/moved/elsewhere.mp3", Some(&hash_of("Alpha Song")))];
    let out = match_entries(&db, &entries, None).await?;
    assert_eq!(out.matched_by_path, 0);
    assert_eq!(out.matched_by_hash, 1);
    assert_eq!(out.missing, 0);
    assert_eq!(out.ordered_ids, vec![a]);
    Ok(())
}

#[tokio::test]
async fn match_entries_reports_missing() -> Result<(), AppError> {
    let (db, ..) = seed().await?;
    let entries = [entry("/nope/gone.mp3", Some(&hash_of("Not In Library")))];
    let out = match_entries(&db, &entries, None).await?;
    assert_eq!(out.matched_by_path, 0);
    assert_eq!(out.matched_by_hash, 0);
    assert_eq!(out.missing, 1);
    assert!(out.ordered_ids.is_empty());
    Ok(())
}

#[tokio::test]
async fn match_entries_preserves_file_order_across_passes() -> Result<(), AppError> {
    let (db, a, b, c) = seed().await?;
    // Order: hash-match(b), path-match(a), missing, path-match(c).
    let entries = [
        entry("/stale/b.mp3", Some(&hash_of("Beta Song"))),
        entry("/music/a.mp3", None),
        entry("/gone.mp3", None),
        entry("/music/c.mp3", None),
    ];
    let out = match_entries(&db, &entries, None).await?;
    assert_eq!(out.matched_by_path, 2);
    assert_eq!(out.matched_by_hash, 1);
    assert_eq!(out.missing, 1);
    // Misses omitted, surviving ids keep file order.
    assert_eq!(out.ordered_ids, vec![b, a, c]);
    Ok(())
}

#[tokio::test]
async fn match_entries_does_not_dedup_repeated_tracks() -> Result<(), AppError> {
    // match_entries preserves every match; dedup is add_tracks_to_playlist's job.
    let (db, a, ..) = seed().await?;
    let entries = [entry("/music/a.mp3", None), entry("/music/a.mp3", None)];
    let out = match_entries(&db, &entries, None).await?;
    assert_eq!(out.ordered_ids, vec![a, a]);
    Ok(())
}

#[tokio::test]
async fn match_entries_resolves_relative_paths_against_base() -> Result<(), AppError> {
    let db = DbPool::test_pool().await?;
    let base = std::path::Path::new("/music");
    queries::folder::insert_folder(&db, "/music", true).await?;
    // Seeded through the same `join` the resolver runs, rather than the shared `seed()`'s
    // POSIX literal: `join` appends the native separator without touching the one already
    // there, so on Windows it yields `/music\a.mp3` and a hand-spelled `/music/a.mp3` is a
    // row the lookup can never reach.
    let seeded = base.join("a.mp3").to_string_lossy().into_owned();
    let a = insert_test_track(&db, &seeded, "Alpha Song", "Artist A", "Album", "Rock").await?;

    let entries = [entry("a.mp3", None)];
    let out = match_entries(&db, &entries, Some(base)).await?;
    assert_eq!(out.matched_by_path, 1);
    assert_eq!(out.ordered_ids, vec![a]);
    Ok(())
}

// --- Filename sanitization ---

#[test]
fn sanitize_stem_replaces_illegal_chars() {
    assert_eq!(sanitize_stem("a/b:c?"), "a_b_c_");
    assert_eq!(sanitize_stem("a\nb"), "a_b");
    assert_eq!(sanitize_stem("x<y>z|w*"), "x_y_z_w_");
    // Illegal chars become underscores rather than triggering the fallback.
    assert_eq!(sanitize_stem("///"), "___");
}

#[test]
fn sanitize_stem_falls_back_when_empty() {
    assert_eq!(sanitize_stem(""), "playlist");
    assert_eq!(sanitize_stem("   "), "playlist");
}

#[test]
fn sanitize_stem_guards_windows_reserved_names() {
    assert_eq!(sanitize_stem("CON"), "_CON");
    assert_eq!(sanitize_stem("con"), "_con");
    assert_eq!(sanitize_stem("Lpt9"), "_Lpt9");
    // Not reserved — left alone.
    assert_eq!(sanitize_stem("CONsole"), "CONsole");
}

#[test]
fn sanitize_stem_strips_trailing_dots_and_spaces() {
    assert_eq!(sanitize_stem("name...  "), "name");
    assert_eq!(sanitize_stem("  spaced  "), "spaced");
}

#[test]
fn sanitize_stem_caps_length() {
    let long = "x".repeat(300);
    assert_eq!(sanitize_stem(&long).chars().count(), MAX_STEM_CHARS);
}

#[test]
fn unique_entry_name_disambiguates_collisions() {
    let mut used: HashSet<String> = HashSet::new();
    assert_eq!(unique_entry_name("Road", &mut used), "Road.m3u8");
    assert_eq!(unique_entry_name("Road", &mut used), "Road (2).m3u8");
    assert_eq!(unique_entry_name("Road", &mut used), "Road (3).m3u8");
}

/// The archive may be extracted onto a filesystem that folds case, where two entries differing
/// only in case are one file.
#[test]
fn unique_entry_name_treats_names_differing_only_in_case_as_one() {
    let mut used: HashSet<String> = HashSet::new();
    assert_eq!(unique_entry_name("Road", &mut used), "Road.m3u8");
    assert_eq!(unique_entry_name("road", &mut used), "road (2).m3u8");
}

#[test]
fn unique_entry_name_steps_past_a_name_already_carrying_a_suffix() {
    let mut used: HashSet<String> = HashSet::new();
    assert_eq!(unique_entry_name("Road (2)", &mut used), "Road (2).m3u8");
    assert_eq!(unique_entry_name("Road", &mut used), "Road.m3u8");
    assert_eq!(unique_entry_name("Road", &mut used), "Road (3).m3u8");
}

#[test]
fn a_suggested_file_name_has_no_folder_for_a_save_dialog_to_read_into() {
    assert_eq!(suggested_file_name("Rock/Roll"), "Rock_Roll.m3u8");
}

#[test]
fn a_suggested_archive_name_is_dated_from_the_instant_passed_in() {
    let named = suggested_archive_name(datetime(2026, 3, 7, 12, 0, 0, 0));
    assert_eq!(named, "netodia-playlists-2026-03-07.zip");
}

#[test]
fn a_playlist_counts_its_path_and_hash_matches_together_toward_the_total() {
    let mut total = ImportFileResult::default();
    total.add(&ImportPlaylistResult {
        playlist_id: 1,
        playlist_name: "Mixed".to_owned(),
        total_entries: 6,
        matched_by_path: 2,
        matched_by_hash: 3,
        missing: 1,
    });
    assert_eq!((total.imported, total.matched, total.missing, total.failed), (1, 5, 1, 0));
}

/// A pool whose three rows live under `dir`, joined rather than spelled.
///
/// [`seed`] above spells `/music/a.mp3`, which is fine while both sides of the comparison spell
/// it the same way. The door tests below cannot: they resolve entry paths against the playlist
/// file's own directory, and `/music/a.mp3` is not an absolute path on Windows.
async fn seed_under(dir: &Path) -> Result<(DbPool, Vec<i64>), AppError> {
    let db = DbPool::test_pool().await?;
    queries::folder::insert_folder(&db, &dir.to_string_lossy(), true).await?;

    let mut ids = Vec::new();
    for (file, title) in [("a.mp3", "Alpha Song"), ("b.mp3", "Beta Song"), ("c.mp3", "Gamma Song")]
    {
        let path = dir.join(file);
        ids.push(
            insert_test_track(&db, &path.to_string_lossy(), title, "Artist A", "Album", "Rock")
                .await?,
        );
    }
    Ok((db, ids))
}

async fn playlist_with_tracks(db: &DbPool, name: &str, ids: &[i64]) -> Result<i64, AppError> {
    let playlist = queries::playlist::create_playlist(db, name, None).await?;
    queries::playlist::add_tracks_to_playlist(db, playlist.id, ids).await?;
    Ok(playlist.id)
}

async fn titles_in(db: &DbPool, playlist_id: i64) -> Result<Vec<String>, AppError> {
    Ok(queries::playlist::get_playlist_tracks_for_list(db, playlist_id)
        .await?
        .into_iter()
        .map(|t| t.title)
        .collect())
}

/// The playlist entries an archive at `path` holds, in archive order.
fn archived(path: &Path) -> Result<Vec<archive::Entry>, AppError> {
    Ok(archive::read(std::fs::File::open(path)?)?.entries)
}

fn names(entries: &[archive::Entry]) -> Vec<&str> {
    entries.iter().map(|entry| entry.name.as_str()).collect()
}

/// The counts of an archive export that was written, failing the test when it was refused.
fn written(export: ExportOutcome) -> Result<ExportPlaylistsResult, AppError> {
    match export {
        ExportOutcome::Exported(result) => Ok(result),
        ExportOutcome::TooLarge => {
            Err(AppError::Validation("export refused as too large".to_owned()))
        }
    }
}

#[tokio::test]
async fn an_exported_playlist_lands_under_a_sanitized_name() -> Result<(), AppError> {
    let tmp = tempfile::tempdir()?;
    let (db, ids) = seed_under(tmp.path()).await?;
    let playlist_id = playlist_with_tracks(&db, "Rock/Roll: Best?", &ids).await?;

    let out = tmp.path().join("exported.zip");
    let result = written(write_archive(&db, &[playlist_id], &out, DateTime::default()).await?)?;

    assert_eq!(result.exported, 1);
    assert_eq!(result.failed, 0);
    let entries = archived(&out)?;
    assert_eq!(names(&entries), ["Rock_Roll_ Best_.m3u8"]);

    let text = &entries[0].text;
    assert!(text.starts_with("#EXTM3U\n"));
    assert!(
        text.contains("#PLAYLIST:Rock/Roll: Best?"),
        "the name is sanitized for the filename, never for the file's own contents"
    );
    Ok(())
}

/// The batch de-duplicates against itself, so two playlists the user named differently cannot
/// end up as one entry that only holds the second.
#[tokio::test]
async fn two_playlists_that_sanitize_alike_get_separate_files() -> Result<(), AppError> {
    let tmp = tempfile::tempdir()?;
    let (db, ids) = seed_under(tmp.path()).await?;
    let first = playlist_with_tracks(&db, "A/B", &ids).await?;
    let second = playlist_with_tracks(&db, "A:B", &ids).await?;

    let out = tmp.path().join("exported.zip");
    let result = written(write_archive(&db, &[first, second], &out, DateTime::default()).await?)?;

    assert_eq!(result.exported, 2);
    assert_eq!(names(&archived(&out)?), ["A_B.m3u8", "A_B (2).m3u8"]);
    Ok(())
}

/// Export is a batch over a multi-select, so one bad id is counted rather than a reason to lose
/// the playlists beside it.
#[tokio::test]
async fn a_playlist_that_cannot_be_read_is_reported_without_stopping_the_batch()
-> Result<(), AppError> {
    let tmp = tempfile::tempdir()?;
    let (db, ids) = seed_under(tmp.path()).await?;
    let playlist_id = playlist_with_tracks(&db, "Kept", &ids).await?;

    let out = tmp.path().join("exported.zip");
    let result =
        written(write_archive(&db, &[playlist_id, 9_999], &out, DateTime::default()).await?)?;

    assert_eq!(result.exported, 1, "the readable playlist still writes");
    assert_eq!(result.failed, 1);
    assert_eq!(names(&archived(&out)?), ["Kept.m3u8"]);
    Ok(())
}

/// The three match categories over one file, which is the only place their sum is checkable. A
/// category that silently swallowed an entry would leave the user a shorter playlist and a
/// completion toast claiming otherwise.
#[tokio::test]
async fn every_entry_is_counted_as_matched_by_path_by_hash_or_missing() -> Result<(), AppError> {
    let tmp = tempfile::tempdir()?;
    let (db, _) = seed_under(tmp.path()).await?;

    let src = tmp.path().join("Mixed.m3u8");
    let text = format!(
        "#EXTM3U\n#PLAYLIST:Mixed\n\
         #EXTINF:-1,Alpha Song\n{here}\n\
         #EXTINF:-1,Beta Song\n#NETODIA-HASH:{hash}\n{moved}\n\
         #EXTINF:-1,Nowhere\n{gone}\n",
        here = tmp.path().join("a.mp3").to_string_lossy(),
        hash = hash_of("Beta Song"),
        moved = tmp.path().join("moved-b.mp3").to_string_lossy(),
        gone = tmp.path().join("nope.mp3").to_string_lossy(),
    );
    std::fs::write(&src, text)?;

    let result = read_playlist_file(&db, &src).await?;

    assert_eq!(result.total_entries, 3);
    assert_eq!(result.matched_by_path, 1);
    assert_eq!(result.matched_by_hash, 1, "a moved file is still found by its hash");
    assert_eq!(result.missing, 1);
    assert_eq!(
        result.matched_by_path + result.matched_by_hash + result.missing,
        result.total_entries,
        "every entry owes exactly one category"
    );
    assert_eq!(
        titles_in(&db, result.playlist_id).await?,
        ["Alpha Song", "Beta Song"],
        "the miss drops out and file order survives the two passes"
    );
    Ok(())
}

/// Playlist names are not unique, so an import cannot merge into one that happens to share a
/// name: the user would silently lose whichever tracks the two did not have in common.
#[tokio::test]
async fn importing_the_same_file_twice_creates_two_playlists() -> Result<(), AppError> {
    let tmp = tempfile::tempdir()?;
    let (db, _) = seed_under(tmp.path()).await?;

    let src = tmp.path().join("Twice.m3u8");
    let text = format!(
        "#EXTM3U\n#PLAYLIST:Twice\n#EXTINF:-1,Alpha Song\n{}\n",
        tmp.path().join("a.mp3").to_string_lossy()
    );
    std::fs::write(&src, text)?;

    let first = read_playlist_file(&db, &src).await?;
    let second = read_playlist_file(&db, &src).await?;

    assert_ne!(first.playlist_id, second.playlist_id);
    assert_eq!(first.playlist_name, second.playlist_name);
    assert_eq!(queries::playlist::get_all_playlists(&db).await?.len(), 2);
    Ok(())
}

#[tokio::test]
async fn an_import_without_a_name_tag_takes_the_file_stem() -> Result<(), AppError> {
    let tmp = tempfile::tempdir()?;
    let (db, _) = seed_under(tmp.path()).await?;

    let src = tmp.path().join("My Mix.m3u8");
    let text =
        format!("#EXTM3U\n#EXTINF:-1,Alpha Song\n{}\n", tmp.path().join("a.mp3").to_string_lossy());
    std::fs::write(&src, text)?;

    let result = read_playlist_file(&db, &src).await?;
    assert_eq!(result.playlist_name, "My Mix");
    Ok(())
}

/// A file with nothing in it is the one import that errors, so a mis-picked text file does not
/// leave an empty playlist behind for the user to find and delete.
#[tokio::test]
async fn a_file_with_no_entries_creates_no_playlist() -> Result<(), AppError> {
    let tmp = tempfile::tempdir()?;
    let (db, _) = seed_under(tmp.path()).await?;

    let src = tmp.path().join("Empty.m3u8");
    std::fs::write(&src, "#EXTM3U\n#PLAYLIST:Empty\n")?;

    let refused = read_playlist_file(&db, &src).await;
    assert!(matches!(refused, Err(AppError::Validation(_))));
    assert!(queries::playlist::get_all_playlists(&db).await?.is_empty());
    Ok(())
}

/// The pair's whole point: what export writes is what import reads back. Either side drifting
/// alone is the failure this file exists to survive an OS reinstall against.
#[tokio::test]
async fn an_exported_playlist_imports_back_with_the_same_tracks() -> Result<(), AppError> {
    let tmp = tempfile::tempdir()?;
    let (db, ids) = seed_under(tmp.path()).await?;
    let playlist_id = playlist_with_tracks(&db, "Round Trip", &ids).await?;

    let out = tmp.path().join("Round Trip.m3u8");
    export_playlist_to_file(&db, playlist_id, &out).await?;

    let result = read_playlist_file(&db, &out).await?;

    assert_eq!(result.playlist_name, "Round Trip");
    assert_eq!(result.matched_by_path, 3);
    assert_eq!(result.missing, 0);

    let restored: Vec<i64> =
        queries::playlist::get_playlist_tracks_for_list(&db, result.playlist_id)
            .await?
            .into_iter()
            .map(|t| t.id)
            .collect();
    assert_eq!(restored, ids);
    Ok(())
}

/// Zips `entries` as `(name, text)` into a file at `path`, through the writer export uses.
fn archive_at(path: &Path, entries: &[(&str, &str)]) -> Result<(), AppError> {
    let entries: Vec<archive::Entry> = entries
        .iter()
        .map(|&(name, text)| archive::Entry { name: name.to_owned(), text: text.to_owned() })
        .collect();
    archive::write(std::fs::File::create(path)?, &entries, DateTime::default())
}

/// A playlist file's text holding one entry, `line` being the path as the file spells it.
fn playlist_text(name: Option<&str>, line: &str) -> String {
    let tag = name.map(|name| format!("#PLAYLIST:{name}\n")).unwrap_or_default();
    format!("#EXTM3U\n{tag}#EXTINF:-1,Alpha Song\n{line}\n")
}

/// Every playlist in the pool as `(name, titles)`, oldest first.
async fn playlists_by_age(db: &DbPool) -> Result<Vec<(String, Vec<String>)>, AppError> {
    let mut playlists = queries::playlist::get_all_playlists(db).await?;
    playlists.sort_by_key(|playlist| playlist.id);
    let mut named = Vec::with_capacity(playlists.len());
    for playlist in playlists {
        named.push((playlist.name, titles_in(db, playlist.id).await?));
    }
    Ok(named)
}

#[tokio::test]
async fn an_exported_archive_imports_back_as_the_same_playlists() -> Result<(), AppError> {
    let tmp = tempfile::tempdir()?;
    let (db, ids) = seed_under(tmp.path()).await?;
    let road = playlist_with_tracks(&db, "Road", &ids).await?;
    let short = playlist_with_tracks(&db, "Short", &[ids[2], ids[0]]).await?;
    let out = tmp.path().join("backup.zip");
    written(write_archive(&db, &[road, short], &out, DateTime::default()).await?)?;

    let result = import_playlists_from_file(&db, &out).await?;

    assert_eq!((result.imported, result.matched, result.missing, result.failed), (2, 5, 0, 0));
    let road_titles =
        vec!["Alpha Song".to_owned(), "Beta Song".to_owned(), "Gamma Song".to_owned()];
    let short_titles = vec!["Gamma Song".to_owned(), "Alpha Song".to_owned()];
    assert_eq!(
        playlists_by_age(&db).await?[2..],
        [("Road".to_owned(), road_titles), ("Short".to_owned(), short_titles)],
        "each playlist comes back under its own name, holding its own tracks in its own order"
    );
    Ok(())
}

#[tokio::test]
async fn an_archive_export_with_nothing_readable_writes_no_file() -> Result<(), AppError> {
    let tmp = tempfile::tempdir()?;
    let (db, _) = seed_under(tmp.path()).await?;
    let out = tmp.path().join("backup.zip");

    let result = written(write_archive(&db, &[9_999], &out, DateTime::default()).await?)?;

    assert_eq!((result.exported, result.failed), (0, 1));
    assert!(!out.exists(), "an archive with nothing in it would import back as a refusal");
    Ok(())
}

/// Import refuses an archive expanding past its budget whole, so an export that wrote one would
/// hand the user a backup that can never be restored. Refusing has to leave what the save dialog
/// was about to replace alone.
#[tokio::test]
async fn playlists_too_large_to_import_back_write_nothing_and_leave_the_old_file()
-> Result<(), AppError> {
    // One track whose path alone spends a sixteenth of the budget, exported seventeen times.
    const PATH_BYTES: usize = 4 * 1024 * 1024;
    const COPIES: usize = 17;

    let tmp = tempfile::tempdir()?;
    let db = DbPool::test_pool().await?;
    queries::folder::insert_folder(&db, &tmp.path().to_string_lossy(), true).await?;
    let long = tmp.path().join("x".repeat(PATH_BYTES));
    let track =
        insert_test_track(&db, &long.to_string_lossy(), "Long", "A", "Album", "Rock").await?;
    let playlist = playlist_with_tracks(&db, "Long", &[track]).await?;
    let out = tmp.path().join("backup.zip");
    std::fs::write(&out, "an earlier backup")?;

    let export = write_archive(&db, &[playlist; COPIES], &out, DateTime::default()).await?;

    assert!(matches!(export, ExportOutcome::TooLarge));
    assert_eq!(std::fs::read_to_string(&out)?, "an earlier backup");
    Ok(())
}

/// A smart playlist has no stored tracks, so exporting what it stores wrote a header and nothing
/// else, which import then refuses.
#[tokio::test]
async fn a_smart_playlist_exports_the_tracks_its_rules_match() -> Result<(), AppError> {
    let tmp = tempfile::tempdir()?;
    let (db, _) = seed_under(tmp.path()).await?;
    let criteria = SmartCriteria {
        rules: vec![Rule {
            field: RuleField::Title,
            op: RuleOp::NotContains,
            value: Some(RuleValue::Text("Beta".to_owned())),
        }],
        ..SmartCriteria::default()
    };
    let json = criteria.to_json().map_err(AppError::io_source)?;
    let smart = queries::playlist::create_smart_playlist(&db, "No Beta", None, &json).await?;
    let out = tmp.path().join("No Beta.m3u8");

    export_playlist_to_file(&db, smart.id, &out).await?;

    let result = read_playlist_file(&db, &out).await?;
    assert_eq!(titles_in(&db, result.playlist_id).await?, ["Alpha Song", "Gamma Song"]);
    Ok(())
}

#[tokio::test]
async fn an_archive_holding_no_playlist_files_is_refused() -> Result<(), AppError> {
    let tmp = tempfile::tempdir()?;
    let (db, _) = seed_under(tmp.path()).await?;
    let src = tmp.path().join("photos.zip");
    archive_at(&src, &[("README.txt", "holiday photos")])?;

    let refused = import_playlists_from_file(&db, &src).await;

    assert!(matches!(refused, Err(AppError::Validation(_))));
    Ok(())
}

/// An archive whose every playlist failed still held playlists, so the toast reports them as
/// failures rather than as a file that isn't a playlist archive at all.
#[tokio::test]
async fn an_archive_whose_every_playlist_is_unreadable_reports_them_rather_than_failing()
-> Result<(), AppError> {
    let tmp = tempfile::tempdir()?;
    let (db, _) = seed_under(tmp.path()).await?;
    let src = tmp.path().join("backup.zip");
    let a = tmp.path().join("a.mp3");
    archive_at(&src, &[("../escaped.m3u8", &playlist_text(None, &a.to_string_lossy()))])?;

    let result = import_playlists_from_file(&db, &src).await?;

    assert_eq!((result.imported, result.failed), (0, 1));
    Ok(())
}

#[tokio::test]
async fn an_empty_playlist_in_an_archive_is_counted_and_its_siblings_still_land()
-> Result<(), AppError> {
    let tmp = tempfile::tempdir()?;
    let (db, _) = seed_under(tmp.path()).await?;
    let src = tmp.path().join("backup.zip");
    let a = tmp.path().join("a.mp3");
    archive_at(
        &src,
        &[
            ("Empty.m3u8", "#EXTM3U\n#PLAYLIST:Empty\n"),
            ("Full.m3u8", &playlist_text(Some("Full"), &a.to_string_lossy())),
        ],
    )?;

    let result = import_playlists_from_file(&db, &src).await?;

    assert_eq!((result.imported, result.matched, result.failed), (1, 1, 1));
    Ok(())
}

#[tokio::test]
async fn an_archive_playlist_without_a_name_tag_is_named_after_its_entry() -> Result<(), AppError> {
    let tmp = tempfile::tempdir()?;
    let (db, _) = seed_under(tmp.path()).await?;
    let src = tmp.path().join("backup.zip");
    let a = tmp.path().join("a.mp3");
    archive_at(&src, &[("sub/Road Trip.m3u8", &playlist_text(None, &a.to_string_lossy()))])?;

    import_playlists_from_file(&db, &src).await?;

    let names: Vec<String> =
        playlists_by_age(&db).await?.into_iter().map(|(name, _)| name).collect();
    assert_eq!(names, ["Road Trip"]);
    Ok(())
}

/// Nothing is extracted, so a relative entry resolves against where its file would land beside
/// the archive: the archive's folder, then the entry's own folder inside it.
#[tokio::test]
async fn relative_entries_resolve_against_the_archive_and_entry_folders() -> Result<(), AppError> {
    let tmp = tempfile::tempdir()?;
    let (db, _) = seed_under(&tmp.path().join("sub")).await?;
    let src = tmp.path().join("backup.zip");
    archive_at(&src, &[("sub/Relative.m3u8", &playlist_text(Some("Relative"), "a.mp3"))])?;

    let result = import_playlists_from_file(&db, &src).await?;

    assert_eq!((result.imported, result.matched, result.missing), (1, 1, 0));
    Ok(())
}

#[tokio::test]
async fn an_archive_extension_is_recognised_in_any_case() -> Result<(), AppError> {
    let tmp = tempfile::tempdir()?;
    let (db, ids) = seed_under(tmp.path()).await?;
    let road = playlist_with_tracks(&db, "Road", &ids).await?;
    let out = tmp.path().join("BACKUP.ZIP");
    written(write_archive(&db, &[road], &out, DateTime::default()).await?)?;

    let result = import_playlists_from_file(&db, &out).await?;

    assert_eq!((result.imported, result.matched), (1, 3));
    Ok(())
}
