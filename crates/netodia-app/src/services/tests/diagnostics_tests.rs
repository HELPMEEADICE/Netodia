use std::path::Path;

use jiff::civil::datetime;

use super::{
    FolderCounts, LibraryFacts, Paths, assemble, crash_block, head_of, library_block, log_section,
    settings_block, suggested_file_name, tail_of,
};
use netodia_core::error::AppError;
use netodia_core::utils::redact::home_dir_string;
use netodia_testkit::reading_env;

/// A [`Paths`] rooted in a throwaway directory, with the subdirectories [`Paths::resolve`]
/// creates already in place. Creation is best-effort — a failure surfaces as a missing-file
/// error in the test body.
///
/// Per-crate rather than shared: the testkit names no workspace type, which is what keeps it a
/// leaf every other crate can dev-depend on without a cycle.
fn paths_in(dir: &Path) -> Paths {
    let paths = Paths::rooted_at(dir.to_path_buf());
    let _ = paths.create_dirs();
    paths
}

/// A file of `lines` numbered lines, each padded to a known width so a byte
/// budget maps onto a predictable number of them.
fn write_numbered(path: &Path, lines: usize) -> Result<(), AppError> {
    let body: Vec<String> = (0..lines).map(|n| format!("line {n:04}\n")).collect();
    std::fs::write(path, body.concat())?;
    Ok(())
}

/// The tail is what a reporter needs — the end of the run, not its start.
#[test]
fn a_tail_returns_the_end_of_the_file() -> Result<(), AppError> {
    let tmp = tempfile::tempdir()?;
    let path = tmp.path().join("netodia_rCURRENT.log");
    write_numbered(&path, 1000)?;

    let tail = reading_env(|| tail_of(&path, 100)).unwrap_or_default();

    assert!(tail.contains("line 0999"), "tail lost the last line");
    assert!(!tail.contains("line 0000"), "tail reached the first line");
    assert!(tail.len() <= 100, "tail blew its budget: {}", tail.len());
    Ok(())
}

/// A budget cuts mid-line, and half a log record reads like a whole one. The
/// partial head has to go.
#[test]
fn a_tail_starts_on_a_line_boundary() -> Result<(), AppError> {
    let tmp = tempfile::tempdir()?;
    let path = tmp.path().join("netodia_rCURRENT.log");
    write_numbered(&path, 1000)?;

    // 25 bytes lands inside a record, never on its boundary.
    let tail = reading_env(|| tail_of(&path, 25)).unwrap_or_default();

    for line in tail.lines() {
        assert!(line.starts_with("line "), "tail kept a partial record: {line:?}");
    }
    Ok(())
}

/// A file smaller than the budget is returned whole — the boundary trim must
/// not eat its first line.
#[test]
fn a_short_file_keeps_its_first_line() -> Result<(), AppError> {
    let tmp = tempfile::tempdir()?;
    let path = tmp.path().join("small.log");
    write_numbered(&path, 3)?;

    let tail = reading_env(|| tail_of(&path, 64 * 1024)).unwrap_or_default();

    assert!(tail.contains("line 0000"), "trim ate the first line");
    assert!(tail.contains("line 0002"));
    Ok(())
}

/// The bundle goes into a public issue. A home directory usually holds a real
/// name, so it may not survive the trip — in the log body or the file header.
#[test]
fn the_tail_redacts_the_home_directory() -> Result<(), AppError> {
    let Some(home) = home_dir_string() else {
        return Ok(());
    };
    let tmp = tempfile::tempdir()?;
    let path = tmp.path().join("netodia_rCURRENT.log");
    std::fs::write(&path, format!("WARN scan failed for {home}/Music/x.flac\n"))?;

    let tail = reading_env(|| tail_of(&path, 64 * 1024)).unwrap_or_default();

    assert!(!tail.contains(&home), "home leaked: {tail}");
    assert!(tail.contains("~/Music/x.flac"));
    Ok(())
}

/// Most runs never panic, and a bundle from a healthy install still has to
/// build.
#[test]
fn an_install_that_never_crashed_still_reports() -> Result<(), AppError> {
    let tmp = tempfile::tempdir()?;
    let block = reading_env(|| crash_block(tmp.path()));
    assert!(block.contains("<none>"));
    Ok(())
}

/// A crash report opens with what a reporter is asked for and *ends* with the
/// backtrace, so an over-budget one is cut from the far end. Trimmed like a log
/// file it would keep the deepest frames and drop the version, the location and
/// the panic message — the report reads complete and says nothing.
#[test]
fn an_over_budget_crash_report_keeps_its_header() -> Result<(), AppError> {
    let tmp = tempfile::tempdir()?;
    let path = tmp.path().join("crash-20260806-143000.txt");
    let frames = "  0: netodia::some::deeply::nested::frame\n".repeat(500);
    std::fs::write(&path, format!("Netodia crash report\npanic     : boom\n{frames}"))?;

    let head = reading_env(|| head_of(&path, 512)).unwrap_or_default();

    assert!(head.contains("Netodia crash report"), "lost the header: {head}");
    assert!(head.contains("panic     : boom"), "lost the panic message");
    assert!(head.len() <= 512, "head blew its budget: {}", head.len());
    Ok(())
}

/// The budget cuts mid-line as readily as a tail does, and half a frame reads
/// like a whole one.
#[test]
fn a_head_ends_on_a_line_boundary() -> Result<(), AppError> {
    let tmp = tempfile::tempdir()?;
    let path = tmp.path().join("crash-20260806-143000.txt");
    write_numbered(&path, 1000)?;

    // 25 bytes lands inside a record, never on its boundary.
    let head = reading_env(|| head_of(&path, 25)).unwrap_or_default();

    for line in head.lines() {
        assert!(line.starts_with("line "), "head kept a partial record: {line:?}");
    }
    assert!(head.contains("line 0000"), "head lost the first line");
    Ok(())
}

/// A file inside the budget is returned whole — the boundary trim must not eat
/// its last line, which is the mirror of the tail's first-line case.
#[test]
fn a_short_file_keeps_its_last_line() -> Result<(), AppError> {
    let tmp = tempfile::tempdir()?;
    let path = tmp.path().join("small.txt");
    std::fs::write(&path, b"first\nlast without a newline")?;

    let head = reading_env(|| head_of(&path, 64 * 1024)).unwrap_or_default();

    assert_eq!(head, "first\nlast without a newline\n");
    Ok(())
}

/// Which end `crash_block` cuts from is the contract, and the two tests above
/// pin only the helpers. Today a swap back to `tail_of` is caught by `head_of`
/// going unused, but that gate disappears the moment it has a second caller —
/// and what a silent swap costs is the version, the location and the panic
/// message on every over-budget report in the bundle.
#[test]
fn the_crash_block_cuts_a_report_from_the_backtrace_end() -> Result<(), AppError> {
    let tmp = tempfile::tempdir()?;
    let frames = "  0: netodia::some::deeply::nested::frame\n".repeat(1000);
    std::fs::write(
        tmp.path().join("crash-20260806-143000.txt"),
        format!("Netodia crash report\npanic     : boom\n{frames}"),
    )?;

    let block = reading_env(|| crash_block(tmp.path()));

    assert!(block.contains("panic     : boom"), "the report was cut from the wrong end: {block}");
    Ok(())
}

/// A locked or corrupt database is a plausible thing to be filing a bug about,
/// and the logs and crash reports beside it are still worth handing over — so
/// the library block degrades the way every other block here does rather than
/// taking the bundle down with it.
///
/// The mixed case is the one worth having: a lock is released between two
/// queries as readily as it is held across both, and a track count is worth
/// handing over without the folder list.
#[test]
fn each_library_line_degrades_on_its_own() {
    let both = library_block(&LibraryFacts {
        tracks: Some(4211),
        folders: Some(FolderCounts { total: 3, enabled: 2 }),
    });
    assert!(both.contains("4211"), "lost the track count: {both}");
    assert!(both.contains("3 (2 enabled)"), "lost the folder counts: {both}");

    let counted_tracks_only = library_block(&LibraryFacts { tracks: Some(4211), folders: None });
    assert!(
        counted_tracks_only.contains("4211"),
        "a failed folder query took the track count with it: {counted_tracks_only}"
    );
    assert!(counted_tracks_only.contains("<unavailable>"));

    let neither = library_block(&LibraryFacts { tracks: None, folders: None });
    assert_eq!(neither.matches("<unavailable>").count(), 2);
}

/// The two kinds of empty are the reason `unavailable_reason` exists at all: an
/// install that simply hasn't written a log yet wants a different answer from a
/// sink that couldn't open its file, and from an empty section they look alike.
#[test]
fn the_two_kinds_of_empty_logs_read_differently() {
    let never_started = reading_env(|| {
        log_section(Some("Log cannot be written: Permission denied (os error 13)"), Vec::new())
    });
    assert!(never_started.contains("<unavailable"));
    assert!(
        never_started.contains("Permission denied"),
        "the reason is the whole value of the branch: {never_started}"
    );

    let nothing_yet = reading_env(|| log_section(None, Vec::new()));
    assert!(nothing_yet.contains("<none>"), "{nothing_yet}");
}

/// The reason comes from `flexi_logger` rather than from this module, so it is
/// the one string here that could grow a path without anyone editing this file.
#[test]
fn the_unavailable_reason_is_redacted_like_everything_else() {
    let Some(home) = home_dir_string() else {
        return;
    };
    let block = reading_env(|| {
        log_section(Some(&format!("cannot open {home}/.local/share/Netodia/logs")), Vec::new())
    });

    assert!(!block.contains(&home), "home leaked: {block}");
    assert!(block.contains("~/.local/share/Netodia/logs"));
}

/// Which root the boot landed on is the one fact a reporter can't infer, `Netodia-dev` and
/// `NETODIA_DATA_DIR` both moving it. It is also a path, so it owes the redaction every other one
/// here goes through.
#[test]
fn the_report_names_the_data_root_and_redacts_it() {
    let Some(home) = home_dir_string() else {
        return;
    };
    let paths = Paths::rooted_at(Path::new(&home).join("Netodia-dev"));
    let facts = LibraryFacts { tracks: None, folders: None };

    let report = reading_env(|| assemble(&paths, &facts));

    assert!(report.contains("data dir  : ~"), "{report}");
    assert!(!report.contains(&home), "home leaked: {report}");
}

/// The whole point of the allowlist: the block names the fields it was written
/// to name, and no others. The mutation to catch is someone reaching for a
/// whole-struct `Debug` dump, which would pull in the window geometry, the
/// per-theme preference map and — the reason this test exists — anything a
/// later release adds to `SettingsData` without thinking about this file.
#[test]
fn the_settings_block_is_an_allowlist() -> Result<(), AppError> {
    let tmp = tempfile::tempdir()?;
    let paths = paths_in(tmp.path());

    let block = reading_env(|| settings_block(&paths));

    for expected in ["theme", "locale", "titlebar", "tray", "crossfade", "verbose"] {
        assert!(block.contains(expected), "block is missing {expected:?}");
    }
    for forbidden in [
        "window_x",
        "window_y",
        "sidebar_width",
        "theme_preferences",
        "eq_band_gains",
        "last_manifest_etag",
        "scrobble",
        "session_key",
        "password",
    ] {
        assert!(!block.contains(forbidden), "block leaked {forbidden:?}:\n{block}");
    }
    Ok(())
}

/// The bundle sits in a downloads folder next to whatever else the reporter has
/// saved, so the suggested name has to say what it is *and* when — stamped the
/// same way a crash report's name is, since the two get read together.
#[test]
fn the_suggested_name_carries_a_sortable_local_stamp() {
    let now = datetime(2026, 8, 6, 14, 30, 5, 0);

    assert_eq!(suggested_file_name(now), "netodia-diagnostics-20260806-143005.txt");
}
