//! Wire the Settings page's Diagnostics card, and the toast that follows a crash.
//!
//! Everything `services::{logging, crash_report, diagnostics}` produces lands in
//! one directory and is otherwise unreachable — a user launching from a
//! `.desktop` entry or the Start Menu has no console and no reason to go looking
//! in `~/.local/share`. This module is the whole of what surfaces it: two
//! buttons, a switch and one notice.

use std::rc::Rc;

use async_compat::Compat;
use jiff::ZonedDateTime;
use slint::{ComponentHandle, SharedString, Weak};

use crate::ui::shell::notifications::{Completion, NotificationsUi, RowText};
use crate::ui::{file_dialog, launcher};
use netodia_app::state::AppState;
use netodia_app::{library, services};
use netodia_platform::services::platform;
use netodia_ui::{AppWindow, NotificationKind, NotificationVariant, Settings};

pub fn install(ui: &AppWindow, state: &AppState, notifications: &Rc<NotificationsUi>) {
    wire_open_log_folder(ui, state);
    wire_save_report(ui, state, notifications);
    wire_verbose_logging(ui, state);
}

/// Hand the logs directory to the desktop's file manager. The same callback the
/// crash toast's action button reaches, hence the two buttons and one handler.
fn wire_open_log_folder(ui: &AppWindow, state: &AppState) {
    let runtime = state.runtime.clone();
    let logs_dir = state.paths.logs_dir.clone();
    ui.global::<Settings>().on_open_log_folder(move || {
        runtime.spawn(launcher::open_target(logs_dir.clone(), "open-log-folder"));
    });
}

/// On the UI thread throughout: the picker has to be, and `Rc<NotificationsUi>`
/// is `!Send`. `Compat` supplies the tokio reactor the awaited halves need.
fn wire_save_report(ui: &AppWindow, state: &AppState, notifications: &Rc<NotificationsUi>) {
    let state = state.clone();
    let weak = ui.as_weak();
    let notifications = notifications.clone();
    ui.global::<Settings>().on_save_diagnostics_report(move || {
        let state = state.clone();
        let weak = weak.clone();
        let notifications = notifications.clone();
        let _ = slint::spawn_local(Compat::new(save_report(state, weak, notifications)));
    });
}

/// Ask where the bundle goes, build it, write it, and say which way it went.
/// A cancelled picker is the ordinary case and says nothing.
async fn save_report(state: AppState, weak: Weak<AppWindow>, notifications: Rc<NotificationsUi>) {
    let dialog = file_dialog::parented(&weak, "Save Diagnostics Report")
        .set_file_name(services::diagnostics::suggested_file_name(ZonedDateTime::now().datetime()));
    let Some(target) = dialog.save_file().await else {
        return;
    };
    let path = target.path().to_path_buf();

    // Written on the blocking pool: this future runs on the UI thread.
    let result = match services::diagnostics::build_report(&state).await {
        Ok(text) => netodia_core::utils::atomic_file::write_text(path.clone(), text).await,
        Err(e) => Err(e),
    };

    let Some(ui) = weak.upgrade() else { return };
    let settings = ui.global::<Settings>();
    match result {
        Ok(()) => {
            notifications.show_completion(
                Completion::Complete,
                settings.invoke_diagnostics_saved_title(),
                settings.invoke_diagnostics_saved_message(SharedString::from(
                    path.display().to_string(),
                )),
            );
        }
        Err(e) => {
            log::warn!("save-diagnostics-report: {}", netodia_core::error::describe(&e));
            notifications.show_failure(&ui, |ui| {
                let g = ui.global::<Settings>();
                RowText::plain(
                    g.invoke_diagnostics_failed_title(),
                    g.invoke_diagnostics_failed_message(),
                )
            });
        }
    }
}

/// Apply the Verbose Logging switch live, then persist it.
///
/// Seeded with `if let Ok` rather than [`crate::ui::settings_bind::read_or_default`]
/// so an unreadable file leaves the Slint-declared `false` — the same answer
/// `logging::install` reached from the same read, so the two can't disagree.
fn wire_verbose_logging(ui: &AppWindow, state: &AppState) {
    if let Ok(settings) = services::settings::read_settings(&state.paths) {
        ui.global::<Settings>().set_verbose_logging(settings.diagnostics.verbose_logging);
    }

    let state = state.clone();
    ui.global::<Settings>().on_verbose_logging_changed(move |on| {
        // Live first: a failed disk write must not undo what the user can
        // already see working.
        platform::logging::set_verbose(on);
        state.persist_blocking("persist verbose_logging", move |paths| {
            library::settings::set_verbose_logging(paths, on)
        });
    });
}

/// Announce a panic from the previous run, once.
///
/// Sticky rather than auto-dismissing: a crash notice the user was looking away
/// for is a crash notice that did nothing. Runs synchronously here because it is
/// one `read_dir` over a directory holding at most a handful of entries, and a
/// marker write only when there is actually something to report.
///
/// Called by `boot::ui_setup::install` rather than from [`install`], so a launch that opens the
/// welcome card can hold it until the card closes. It must be **deferred and never skipped**:
/// `take_unseen` consumes the marker, so a suppressed notice is a report the user never hears
/// about.
pub fn notify_previous_crash(ui: &AppWindow, state: &AppState, notifications: &NotificationsUi) {
    let Some(report) = platform::crash_report::take_unseen(&state.paths.logs_dir) else {
        return;
    };
    log::info!("previous run left a crash report: {}", report.display());

    notifications.show_localized(
        ui,
        NotificationVariant::Warning,
        NotificationKind::CrashReport,
        |ui| {
            let g = ui.global::<Settings>();
            RowText {
                title: g.invoke_crash_report_title(),
                message: g.invoke_crash_report_message(),
                action_label: g.invoke_crash_report_action_label(),
            }
        },
    );
}

#[cfg(test)]
#[path = "tests/diagnostics_tests.rs"]
mod tests;
