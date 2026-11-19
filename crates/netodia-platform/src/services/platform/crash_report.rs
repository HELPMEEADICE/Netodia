//! Panic reports, written beside the rolling log.
//!
//! `panic = "abort"` in release means every Rust panic anywhere in the process
//! is fatal, so a hook is complete coverage of the Rust half rather than a
//! partial measure — and it does run under abort: the pinned toolchain's
//! `rust_panic_with_hook` dispatches the hook before `__rust_start_panic`. It
//! also means a run produces at most one report, since the first panic ends the
//! process.
//!
//! **On what a backtrace is worth here.** `[profile.release]` sets
//! `strip = "debuginfo"` rather than `strip = true` precisely so these stay
//! readable, but that keeps the symbol table and not the line table — frames
//! carry function names and no `file:line` — and `lto = "fat"` plus
//! `codegen-units = 1` inline hard enough that the trace is sparse. v0 mangling
//! at least makes the names unambiguous across monomorphizations. The `log`
//! macros' own `file:line` in the rolling log is what fills the gap, which is
//! why `services::platform::logging` formats files with `detailed_format`.
//!
//! Timestamps are **local**, unlike everything else persisted by this app
//! (`utils::now_rfc3339` is UTC). These reports sit in one folder with the log
//! files and get read together, `flexi_logger` stamps its lines in local time,
//! and a reporter saying "it crashed around 2pm" should match the filename. The
//! RFC-3339 offset keeps it unambiguous.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use jiff::{ZonedDateTime, civil::DateTime};

use netodia_core::error::describe;
use netodia_core::utils::format_rfc3339;
use netodia_core::utils::redact::redact_home;

/// How many reports survive a [`prune`].
///
/// Each is a few KiB of text, so this is bounded by usefulness rather than by
/// cost: what a reporter is asked for is the crash they just had, and anything
/// older than the last handful has been superseded by a release or forgotten.
const MAX_CRASH_REPORTS: usize = 10;

const PREFIX: &str = "crash-";
const SUFFIX: &str = ".txt";

/// High-water mark of the newest report the user has already been shown, so a
/// crash is announced once rather than on every launch after it.
///
/// It shares the logs directory with the things it tracks and is retired by
/// neither sweep: it carries neither [`PREFIX`] nor [`SUFFIX`], so
/// [`timestamp_of`] rejects it, and `flexi_logger`'s own cleanup matches its
/// `netodia` basename and `log` suffix. Two independent gates.
const LAST_SEEN_FILE: &str = "last-seen-crash.json";

/// The timestamp format inside an artifact's file name. Sortable, path-safe,
/// and second-resolution — the process aborts on the first panic, so two
/// reports can only collide across concurrent instances.
///
/// Shared with `services::diagnostics`, whose saved bundle is stamped the same
/// way: the two are read side by side, so they had better agree.
pub const FILE_TS_FORMAT: &str = "%Y%m%d-%H%M%S";

/// Held for one trip through the hook, so two threads panicking at once don't
/// interleave a report write, a prune and a `log::error!`.
///
/// **Not a recursion guard, though it reads like one.** A panic raised *inside*
/// this closure never reaches it a second time: `panic_count::increase` sets a
/// thread-local in-hook flag and `rust_panic_with_hook` aborts on it *before*
/// dispatching, so the only way to lose the exchange is a concurrent panic on
/// another thread. That is why the loser still calls `previous` — one report per
/// crash is plenty, one stderr message per panic is not.
///
/// Cleared on the way out rather than latched for the process: release builds
/// abort on the first panic, but `[profile.dev]` unwinds and a panicking tokio
/// task is swallowed by its `JoinHandle`, so a latch made every panic after the
/// first invisible.
static IN_HOOK: AtomicBool = AtomicBool::new(false);

/// Clears [`IN_HOOK`] when the hook body returns.
struct HookGuard;

impl Drop for HookGuard {
    fn drop(&mut self) {
        IN_HOOK.store(false, Ordering::SeqCst);
    }
}

/// The one definition of the naming scheme; [`timestamp_of`] is its inverse.
fn file_name(now: DateTime) -> String {
    format!("{PREFIX}{}{SUFFIX}", now.strftime(FILE_TS_FORMAT))
}

/// When a report was written, or `None` for a name this module did not write.
/// Pruning is gated on this, so it is the whole of what keeps the sweep off a
/// file that isn't ours — `netodia_rCURRENT.log` shares the directory.
fn timestamp_of(name: &str) -> Option<DateTime> {
    parse_stamp(name.strip_prefix(PREFIX)?.strip_suffix(SUFFIX)?)
}

/// Read a bare [`FILE_TS_FORMAT`] stamp — the half [`timestamp_of`] and the
/// last-seen marker share.
fn parse_stamp(stamp: &str) -> Option<DateTime> {
    DateTime::strptime(FILE_TS_FORMAT, stamp).ok()
}

/// Install the panic hook. Call once, as early as a logs directory exists, so
/// boot panics are covered too.
///
/// Chains rather than replaces: `panic::update_hook` is still unstable on the
/// pinned toolchain, and keeping the previous hook preserves the default stderr
/// message for `cargo run`.
pub fn install_hook(logs_dir: &Path) {
    let logs_dir = logs_dir.to_path_buf();
    let previous = std::panic::take_hook();

    std::panic::set_hook(Box::new(move |info| {
        if IN_HOOK.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst).is_err() {
            previous(info);
            return;
        }
        let _guard = HookGuard;

        let now = ZonedDateTime::now();
        let thread = std::thread::current();
        let location = info.location().map(ToString::to_string);
        let payload = info.payload_as_str().unwrap_or("<non-string panic payload>");
        let backtrace = std::backtrace::Backtrace::force_capture().to_string();

        let report = format_report(&now, thread.name(), location.as_deref(), payload, &backtrace);

        // Plain `fs`, deliberately: a panic raised inside a logging call must
        // not have to go back through the logger to leave an artifact.
        let path = logs_dir.join(file_name(now.datetime()));
        let _ = std::fs::create_dir_all(&logs_dir);
        let _ = std::fs::write(&path, report);
        prune(&logs_dir);

        // Only now the logger, so the panic also closes the rolling log in
        // sequence with whatever led to it.
        log::error!(
            "panic at {}: {payload} — report written to {}",
            location.as_deref().unwrap_or("<unknown location>"),
            path.display()
        );

        previous(info);
    }));
}

/// Render a report. Pure over its inputs so it is testable without panicking.
fn format_report(
    now: &ZonedDateTime,
    thread: Option<&str>,
    location: Option<&str>,
    payload: &str,
    backtrace: &str,
) -> String {
    format!(
        "Netodia crash report\n\
         {facts}\
         thread    : {thread}\n\
         location  : {location}\n\
         panic     : {payload}\n\
         \n\
         backtrace\n\
         {backtrace}\n",
        facts = system_facts(now),
        thread = thread.unwrap_or("<unnamed>"),
        location = location.unwrap_or("<unknown>"),
        payload = redact_home(payload),
        backtrace = redact_home(backtrace),
    )
}

/// The header both this report and the diagnostics bundle open with.
pub fn system_facts(now: &ZonedDateTime) -> String {
    // Absent off Linux, where the pair says nothing worth a line.
    let session_type = std::env::var("XDG_SESSION_TYPE").ok();
    let desktop = std::env::var("XDG_CURRENT_DESKTOP").ok();
    let session = if session_type.is_none() && desktop.is_none() {
        String::new()
    } else {
        format!(
            "session   : {} / {}\n",
            session_type.as_deref().unwrap_or("unknown"),
            desktop.as_deref().unwrap_or("unknown")
        )
    };

    format!(
        "version   : {version}\n\
         timestamp : {timestamp}\n\
         os / arch : {os} / {arch}\n\
         {session}\
         install   : {install}\n",
        version = env!("CARGO_PKG_VERSION"),
        timestamp = format_rfc3339(now.timestamp(), now.offset()),
        os = std::env::consts::OS,
        arch = std::env::consts::ARCH,
        // `None` on macOS and unsupported arches — the install method is
        // unknown there, not absent.
        install = crate::services::platform::install_kind::target::current_target_key()
            .unwrap_or("unknown"),
    )
}

/// The newest report the user hasn't been told about, marking it seen.
///
/// Returns `None` on the second call and on every later launch, so the boot
/// toast fires once per crash. There are no false positives by construction:
/// a report exists only because the hook above wrote one.
pub fn take_unseen(logs_dir: &Path) -> Option<PathBuf> {
    // `existing_reports` is oldest first.
    let (newest, path) = existing_reports(logs_dir).pop()?;

    let marker = logs_dir.join(LAST_SEEN_FILE);
    let seen: LastSeen =
        netodia_core::utils::atomic_file::load_json_or_default_sync(&marker).unwrap_or_default();
    if seen.newest.as_deref().and_then(parse_stamp) >= Some(newest) {
        return None;
    }

    let record = LastSeen { newest: Some(newest.strftime(FILE_TS_FORMAT).to_string()) };
    if let Err(e) = netodia_core::utils::atomic_file::write_json_sync(&marker, &record) {
        // Hand the report over anyway. The hook wrote into this same directory
        // moments before, so a failure here is close to impossible — and a
        // notice that repeats beats one that never arrives.
        log::warn!("crash report marker: {}", describe(&e));
    }
    Some(path)
}

/// What [`take_unseen`] persists, using the same [`FILE_TS_FORMAT`] as report names.
#[derive(Default, serde::Serialize, serde::Deserialize)]
struct LastSeen {
    #[serde(default)]
    newest: Option<String>,
}

/// Reports newest first, at most `limit`.
pub fn recent(logs_dir: &Path, limit: usize) -> Vec<PathBuf> {
    let mut reports = existing_reports(logs_dir);
    reports.reverse();
    reports.truncate(limit);
    reports.into_iter().map(|(_, path)| path).collect()
}

/// Every report in `logs_dir` that parses back into this module's scheme,
/// oldest first. Anything else in the directory — a log file, something the
/// user put there — is not collected and so can't be swept.
fn existing_reports(logs_dir: &Path) -> Vec<(DateTime, PathBuf)> {
    let Ok(entries) = std::fs::read_dir(logs_dir) else {
        return Vec::new();
    };

    let mut reports: Vec<(DateTime, PathBuf)> = entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            let stamp = timestamp_of(path.file_name()?.to_str()?)?;
            Some((stamp, path))
        })
        .collect();

    reports.sort_unstable_by_key(|(stamp, _)| *stamp);
    reports
}

/// Delete all but the newest [`MAX_CRASH_REPORTS`] reports.
fn prune(logs_dir: &Path) {
    let reports = existing_reports(logs_dir);
    let Some(surplus) = reports.len().checked_sub(MAX_CRASH_REPORTS) else {
        return;
    };
    for (_, path) in &reports[..surplus] {
        let _ = std::fs::remove_file(path);
    }
}

#[cfg(test)]
#[path = "tests/crash_report_tests.rs"]
mod tests;
