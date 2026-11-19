//! The hand-over bundle: one text file a reporter attaches to an issue.
//!
//! Everything here is chosen twice — once for being useful in a bug report,
//! once for being safe to publish. **The settings block is an allowlist, never
//! a whole-struct dump**, so a field added to `SettingsData` cannot silently
//! start shipping; `scrobble_credentials.json` is not read by this module at
//! all, at any point.
//!
//! **The log tail rides on a property worth keeping**: no `log::` call site in
//! this tree interpolates a token, session key or password. Anything added that
//! does breaks this file's safety without touching it, so log the fact that a
//! request was signed rather than what signed it.
//!
//! Every path goes through [`redact_home`], since a home directory usually
//! holds a real name.

use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use jiff::{ZonedDateTime, civil::DateTime};

use crate::services::settings::read_settings;
use crate::state::AppState;
use netodia_core::config::Paths;
use netodia_core::error::{AppError, AppResult, describe};
use netodia_core::utils::redact::redact_home;
use netodia_platform::services::platform::crash_report::FILE_TS_FORMAT;
use netodia_platform::services::platform::{crash_report, logging};
use netodia_store::database::queries;

/// How many recent crash reports the bundle embeds. A panic that reproduces is
/// worth two samples; past that the older ones describe a version the reporter
/// has already updated away from.
const CRASH_REPORTS_IN_BUNDLE: usize = 3;

/// Per-report cap, spent from the *front* ([`head_of`]) — a backtrace this long
/// has already said what it knows, and everything a reporter is asked for sits
/// above it.
const MAX_CRASH_REPORT_BYTES: u64 = 16 * 1024;

/// Total cap across all log files. Enough to hold the session that failed plus
/// the tail of the one before it, and small enough to attach to an issue.
const MAX_LOG_TAIL_BYTES: u64 = 256 * 1024;

/// What to pre-fill the save dialog with. Stamped local, like a crash report's
/// name and for the same reason — a reporter matching a file against "it broke
/// around 2pm" shouldn't have to apply an offset.
pub fn suggested_file_name(now: DateTime) -> String {
    format!("netodia-diagnostics-{}.txt", now.strftime(FILE_TS_FORMAT))
}

/// Build the report. Returns the text; the caller owns where it lands.
pub async fn build_report(state: &AppState) -> AppResult<String> {
    let library = library_facts(state).await;
    let paths = Arc::clone(&state.paths);

    // The rest is file I/O — settings, crash reports, and up to a quarter
    // megabyte of log tail.
    tokio::task::spawn_blocking(move || assemble(&paths, &library))
        .await
        .map_err(AppError::io_source)
}

/// What the library looks like, as far as the database could answer.
///
/// Each line degrades on its own, and that is the point: a locked or corrupt database is a
/// plausible thing to be filing a bug *about*, and exactly the case where one query wins and the
/// next loses. A shared failure arm would leave the reporter with no bundle at all while the logs
/// and crash reports sat there readable.
struct LibraryFacts {
    tracks: Option<i64>,
    folders: Option<FolderCounts>,
}

/// How many library folders there are, and how many of them are enabled.
struct FolderCounts {
    total: usize,
    enabled: usize,
}

async fn library_facts(state: &AppState) -> LibraryFacts {
    LibraryFacts {
        tracks: queries::track::count_tracks(&state.db)
            .await
            .inspect_err(|e| log::warn!("diagnostics: track count unavailable: {}", describe(e)))
            .ok(),
        folders: queries::folder::get_all_folders(&state.db)
            .await
            .inspect_err(|e| log::warn!("diagnostics: folder list unavailable: {}", describe(e)))
            .ok()
            .map(|folders| FolderCounts {
                total: folders.len(),
                enabled: folders.iter().filter(|folder| folder.is_enabled).count(),
            }),
    }
}

fn assemble(paths: &Paths, library: &LibraryFacts) -> String {
    format!(
        "Netodia diagnostics report\n\
         {facts}\
         data dir  : {data_dir}\n\
         {library}{settings}{crashes}{logs}",
        facts = crash_report::system_facts(&ZonedDateTime::now()),
        // Its own line rather than left to the boot log below: that budget is spent from the
        // tail, so the line naming the root drops out of exactly the long session worth
        // reporting. Not in `system_facts`, which a crash report shares and which has no `Paths`.
        data_dir = display_path(&paths.data_dir),
        library = library_block(library),
        settings = settings_block(paths),
        crashes = crash_block(&paths.logs_dir),
        logs = log_block(),
    )
}

fn library_block(facts: &LibraryFacts) -> String {
    format!(
        "\nlibrary\n\
         tracks    : {tracks}\n\
         folders   : {folders}\n",
        tracks = facts.tracks.map_or_else(|| "<unavailable>".to_owned(), |count| count.to_string()),
        folders = facts.folders.as_ref().map_or_else(
            || "<unavailable>".to_owned(),
            |counts| format!("{} ({} enabled)", counts.total, counts.enabled),
        ),
    )
}

/// The allowlist. Adding a line here is a deliberate act; adding a field to
/// `SettingsData` is not, and must stay that way.
fn settings_block(paths: &Paths) -> String {
    let Ok(settings) = read_settings(paths) else {
        return "\nsettings\n<unreadable>\n".to_owned();
    };

    format!(
        "\nsettings\n\
         theme     : {theme} / {variant}\n\
         locale    : {locale}\n\
         titlebar  : {titlebar}\n\
         tray      : {tray} (close-to-tray {close_to_tray})\n\
         crossfade : {crossfade}\n\
         equalizer : {equalizer}\n\
         replaygain: {replaygain}\n\
         autoupdate: {autoupdate}\n\
         verbose   : {verbose}\n",
        theme = settings.theme_id,
        variant = settings.theme_variant,
        locale = settings.locale,
        titlebar = if settings.window.use_native_titlebar { "native" } else { "custom" },
        tray = settings.tray.tray_enabled,
        close_to_tray = settings.tray.close_to_tray,
        crossfade = settings.crossfade.crossfade_enabled,
        equalizer = settings.equalizer.eq_enabled,
        replaygain = settings.replaygain.rg_enabled,
        autoupdate = settings.updates.auto_check_enabled,
        // Decides how to read the log tail below it: thin because nothing
        // happened, or thin because nobody asked for the detail.
        verbose = settings.diagnostics.verbose_logging,
    )
}

fn crash_block(logs_dir: &Path) -> String {
    let reports = crash_report::recent(logs_dir, CRASH_REPORTS_IN_BUNDLE);
    if reports.is_empty() {
        return "\ncrash reports\n<none>\n".to_owned();
    }

    let bodies: Vec<String> = reports
        .iter()
        .map(|path| {
            let body = head_of(path, MAX_CRASH_REPORT_BYTES)
                .unwrap_or_else(|| "<unreadable>\n".to_owned());
            format!("\n--- {} ---\n{body}", display_path(path))
        })
        .collect();
    format!("\ncrash reports\n{}", bodies.concat())
}

fn log_block() -> String {
    log_section(logging::unavailable_reason(), logging::log_files())
}

/// The logs section over an already-resolved sink state. Split from [`log_block`]
/// so both kinds of empty are reachable without a live logger, and because the
/// reason is the one string in this file that comes from outside it — it goes
/// through [`redact_home`] like everything else.
fn log_section(unavailable: Option<&str>, files: Vec<PathBuf>) -> String {
    if let Some(reason) = unavailable {
        // Distinguishes "this install never wrote a log" from "the file sink
        // couldn't start", which look identical from an empty section and want
        // different answers from whoever reads the bundle.
        return format!("\nlogs\n<unavailable: {}>\n", redact_home(reason));
    }

    if files.is_empty() {
        return "\nlogs\n<none>\n".to_owned();
    }

    // Newest first, so the budget is spent on what happened most recently.
    let mut sections = Vec::with_capacity(files.len());
    let mut budget = MAX_LOG_TAIL_BYTES;
    for path in files {
        if budget == 0 {
            break;
        }
        let Some(text) = tail_of(&path, budget) else {
            continue;
        };
        budget = budget.saturating_sub(u64::try_from(text.len()).unwrap_or(u64::MAX));
        sections.push(format!("\n--- {} ---\n{text}", display_path(&path)));
    }
    format!("\nlogs\n{}", sections.concat())
}

/// The last `max_bytes` of `path`, starting at a line boundary.
fn tail_of(path: &Path, max_bytes: u64) -> Option<String> {
    let mut file = std::fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    let start = len.saturating_sub(max_bytes);
    file.seek(SeekFrom::Start(start)).ok()?;

    let mut buf = Vec::with_capacity(usize::try_from(len - start).unwrap_or(0));
    file.read_to_end(&mut buf).ok()?;
    let text = String::from_utf8_lossy(&buf);

    // A seek into the middle of the file lands mid-line; drop the partial one
    // rather than emit a fragment that reads like a whole record.
    let trimmed =
        if start > 0 { text.find('\n').map_or("", |i| &text[i + 1..]) } else { text.as_ref() };

    Some(redacted_lines(trimmed))
}

/// The first `max_bytes` of `path`, ending at a line boundary.
///
/// The opposite end from [`tail_of`], and the one call site is why both exist:
/// a crash report *opens* with what a reporter is asked for — version,
/// timestamp, thread, location, panic message — and it is the backtrace that
/// runs long. Cut from the other end, an over-budget report keeps its deepest
/// frames and loses the identity of the crash. A log file is the reverse, so it
/// keeps [`tail_of`].
fn head_of(path: &Path, max_bytes: u64) -> Option<String> {
    let mut file = std::fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();

    let mut buf = Vec::with_capacity(usize::try_from(len.min(max_bytes)).unwrap_or(0));
    file.by_ref().take(max_bytes).read_to_end(&mut buf).ok()?;
    let text = String::from_utf8_lossy(&buf);

    // Only when the budget actually cut something — a whole file that happens
    // to end without a newline must keep its last line.
    let trimmed =
        if len > max_bytes { text.rfind('\n').map_or("", |i| &text[..=i]) } else { text.as_ref() };

    Some(redacted_lines(trimmed))
}

/// The shared exit of both readers: nothing leaves here holding a home
/// directory, and every section ends on a newline so the next one's header
/// starts its own line.
fn redacted_lines(text: &str) -> String {
    let mut owned = redact_home(text).into_owned();
    if !owned.ends_with('\n') {
        owned.push('\n');
    }
    owned
}

fn display_path(path: &Path) -> String {
    redact_home(&path.display().to_string()).into_owned()
}

#[cfg(test)]
#[path = "tests/diagnostics_tests.rs"]
mod tests;
