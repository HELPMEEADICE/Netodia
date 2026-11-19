use std::path::Path;

use jiff::{SignedDuration, ZonedDateTime, civil::datetime, tz::Offset};

use super::{
    LAST_SEEN_FILE, MAX_CRASH_REPORTS, file_name, format_report, prune, recent, take_unseen,
    timestamp_of,
};
use netodia_core::error::AppError;
use netodia_core::utils::redact::home_dir_string;
use netodia_testkit::reading_env;

/// Everything in `dir`, sorted. Report names are fixed-width, so lexicographic
/// order is chronological order — the same property retention leans on.
fn entry_names(dir: &Path) -> Result<Vec<String>, AppError> {
    let mut names: Vec<String> = std::fs::read_dir(dir)?
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    Ok(names)
}

/// A timestamp `n` seconds past an arbitrary fixed instant, so the tests need no wall clock.
fn stamp(offset_secs: i64) -> ZonedDateTime {
    let base = datetime(2026, 8, 6, 14, 30, 0, 0)
        .to_zoned_datetime(Offset::UTC.to_time_zone())
        .unwrap_or(ZonedDateTime::UNIX_EPOCH);
    base + SignedDuration::from_secs(offset_secs)
}

/// The report is what a reporter attaches, so every field earning a line in the
/// format has to actually appear in the output.
///
/// Under `reading_env` because the header reaches `$HOME`, the two XDG session
/// variables and — through `current_target_key()` — `$APPIMAGE`, and a reader
/// races a sibling's mutation exactly as a second mutator would.
#[test]
fn a_report_carries_every_field() {
    let report = reading_env(|| {
        format_report(
            &stamp(0),
            Some("netodia-bg"),
            Some("src/ui/foo.rs:123"),
            "assertion failed",
            "  0: netodia::ui::foo::bar",
        )
    });

    for expected in [
        "Netodia crash report",
        env!("CARGO_PKG_VERSION"),
        std::env::consts::OS,
        std::env::consts::ARCH,
        "netodia-bg",
        "src/ui/foo.rs:123",
        "assertion failed",
        "backtrace",
        "netodia::ui::foo::bar",
    ] {
        assert!(report.contains(expected), "report is missing {expected:?}");
    }
}

/// A panic can happen on an unnamed thread, without a location, or with a
/// payload that isn't a string. None of those may cost the report its shape.
#[test]
fn a_report_survives_missing_fields() {
    let report = reading_env(|| format_report(&stamp(0), None, None, "boom", ""));
    assert!(report.contains("<unnamed>"));
    assert!(report.contains("<unknown>"));
    assert!(report.contains("boom"));
}

/// The report goes into a public issue, so the home directory — which usually
/// holds a real name — must not ride along in the payload or the backtrace.
#[test]
fn a_report_redacts_the_home_directory() {
    let Some(home) = home_dir_string() else {
        return;
    };
    reading_env(|| {
        let report = format_report(
            &stamp(0),
            Some("main"),
            Some("src/lib.rs:1"),
            &format!("failed to open {home}/Music/x.flac"),
            &format!("  0: at {home}/Development/Netodia/src/lib.rs"),
        );

        assert!(!report.contains(&home), "home leaked: {report}");
        assert!(report.contains("~/Music/x.flac"));
        assert!(report.contains("~/Development/Netodia"));
    });
}

/// `file_name` and `timestamp_of` are one scheme and its inverse; pruning is
/// gated on the latter, so a drift between them silently disarms the sweep.
#[test]
fn a_name_round_trips_through_its_inverse() {
    let now = stamp(0).datetime();
    let parsed = timestamp_of(&file_name(now));
    assert_eq!(parsed, Some(now));
}

/// Written out of order on purpose: a prune that trusted `read_dir` order rather
/// than the timestamp in the name would pass on a sorted directory.
#[test]
fn prune_keeps_the_newest_and_deletes_the_rest() -> Result<(), AppError> {
    let tmp = tempfile::tempdir()?;
    let offsets: Vec<i64> = (0..i64::try_from(MAX_CRASH_REPORTS).unwrap_or(10) + 3).collect();
    for offset in offsets.iter().rev() {
        std::fs::write(tmp.path().join(file_name(stamp(*offset).datetime())), b"x")?;
    }

    prune(tmp.path());

    let kept = entry_names(tmp.path())?;
    assert_eq!(kept.len(), MAX_CRASH_REPORTS);
    // The three oldest are the ones that go.
    for offset in &offsets[..3] {
        assert!(!kept.contains(&file_name(stamp(*offset).datetime())), "prune kept an old report");
    }
    Ok(())
}

/// The safety property: pruning is gated on parsing a name back into the scheme
/// this module writes, not on the file merely sitting in the directory. The log
/// files share that directory, so the mutation to catch is a sweep that deletes
/// by age or by count alone.
#[test]
fn prune_never_touches_a_name_it_did_not_write() -> Result<(), AppError> {
    let tmp = tempfile::tempdir()?;
    let decoys = [
        "netodia_rCURRENT.log",
        "netodia_r00000.log",
        "crash-NOPE.txt",
        "crash-20260806-143000.log",
        "notes.txt",
        // This module's own marker, which lives in the same directory and is
        // deliberately spelled outside the scheme so the sweep can't reach it.
        LAST_SEEN_FILE,
    ];
    for name in decoys {
        std::fs::write(tmp.path().join(name), b"x")?;
    }
    for offset in 0..i64::try_from(MAX_CRASH_REPORTS).unwrap_or(10) + 5 {
        std::fs::write(tmp.path().join(file_name(stamp(offset).datetime())), b"x")?;
    }

    prune(tmp.path());

    for name in decoys {
        assert!(tmp.path().join(name).exists(), "prune deleted {name}");
    }
    Ok(())
}

/// The diagnostics bundle wants the crash the user just had, not the first one
/// they ever had.
#[test]
fn recent_returns_the_newest_first() -> Result<(), AppError> {
    let tmp = tempfile::tempdir()?;
    for offset in 0..5 {
        std::fs::write(tmp.path().join(file_name(stamp(offset).datetime())), b"x")?;
    }

    let newest = recent(tmp.path(), 2);

    let names: Vec<String> =
        newest.iter().filter_map(|p| p.file_name()?.to_str().map(str::to_owned)).collect();
    assert_eq!(names, [file_name(stamp(4).datetime()), file_name(stamp(3).datetime())]);
    Ok(())
}

/// A directory with no reports in it — or none at all — is the normal case, not
/// an error: most runs never panic.
#[test]
fn an_empty_folder_yields_nothing() -> Result<(), AppError> {
    let tmp = tempfile::tempdir()?;
    assert!(recent(tmp.path(), 5).is_empty());
    assert!(recent(&tmp.path().join("missing"), 5).is_empty());
    assert_eq!(take_unseen(tmp.path()), None);
    assert_eq!(take_unseen(&tmp.path().join("missing")), None);
    prune(&tmp.path().join("missing"));
    Ok(())
}

/// The boot toast fires once per crash, so the second ask has to come back
/// empty — otherwise every launch after a panic re-announces it forever.
#[test]
fn take_unseen_hands_a_report_over_exactly_once() -> Result<(), AppError> {
    let tmp = tempfile::tempdir()?;
    for offset in 0..3 {
        std::fs::write(tmp.path().join(file_name(stamp(offset).datetime())), b"x")?;
    }

    let first = take_unseen(tmp.path());
    assert_eq!(
        first.as_ref().and_then(|p| p.file_name()?.to_str()),
        Some(file_name(stamp(2).datetime()).as_str()),
        "the newest report is the one worth announcing"
    );
    assert_eq!(take_unseen(tmp.path()), None);
    Ok(())
}

/// A *newer* crash after an acknowledged one is a new crash. The mutation this
/// catches is the marker being written but never compared against — which
/// silences every crash after the first.
#[test]
fn take_unseen_announces_a_crash_newer_than_the_marker() -> Result<(), AppError> {
    let tmp = tempfile::tempdir()?;
    std::fs::write(tmp.path().join(file_name(stamp(0).datetime())), b"x")?;
    assert!(take_unseen(tmp.path()).is_some());

    std::fs::write(tmp.path().join(file_name(stamp(60).datetime())), b"x")?;

    assert_eq!(
        take_unseen(tmp.path()).as_ref().and_then(|p| p.file_name()?.to_str()),
        Some(file_name(stamp(60).datetime()).as_str())
    );
    Ok(())
}

/// Reports older than the high-water mark stay silent even when the newer one
/// they were superseded by has since been pruned away — a `>` slipped to `>=`
/// (or a comparison against the wrong end of the list) shows up here.
#[test]
fn take_unseen_stays_quiet_for_a_report_the_marker_covers() -> Result<(), AppError> {
    let tmp = tempfile::tempdir()?;
    let newest = tmp.path().join(file_name(stamp(60).datetime()));
    std::fs::write(&newest, b"x")?;
    assert!(take_unseen(tmp.path()).is_some());

    std::fs::remove_file(&newest)?;
    std::fs::write(tmp.path().join(file_name(stamp(0).datetime())), b"x")?;

    assert_eq!(take_unseen(tmp.path()), None);
    Ok(())
}

/// The marker shares the directory with the reports, so it must not be one:
/// counted as a report it would be the newest name in there half the time.
#[test]
fn the_marker_is_not_itself_a_report() -> Result<(), AppError> {
    let tmp = tempfile::tempdir()?;
    std::fs::write(tmp.path().join(file_name(stamp(0).datetime())), b"x")?;
    assert!(take_unseen(tmp.path()).is_some());

    assert!(tmp.path().join(LAST_SEEN_FILE).exists(), "no marker was written");
    assert_eq!(timestamp_of(LAST_SEEN_FILE), None);
    assert!(recent(tmp.path(), 10).iter().all(|p| p.file_name() != Some(LAST_SEEN_FILE.as_ref())));
    Ok(())
}
