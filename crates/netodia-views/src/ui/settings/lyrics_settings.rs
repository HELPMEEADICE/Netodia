//! The lyrics feature's three switches, and the one place each is answered.
//!
//! Each has a row on the Settings card **and** a control in Now Playing, and neither view
//! re-reads the other's global after boot — so whichever row is used has to write the twin, or
//! the other opens showing what the flag was at launch. [`mirrored`] is that write, over
//! [`shadow_toggle`]'s shadow-before-persist order: `library::lyrics` reads the feature and online
//! switches on a worker on every track change, so a lookup landing between the click and the disk
//! write has to see the new answer rather than the file's old one.
//!
//! Lyrics and the lookup are off by default, the second being what the shipped package
//! description promises of every online feature. Romanization is on: it reaches nothing, and it
//! draws nothing at all unless the sheet is in a script the reader cannot sound out.

use slint::ComponentHandle;

use crate::ui::settings_bind::shadow_toggle;
use netodia_app::library;
use netodia_app::state::{AppState, SharedFlag};
use netodia_core::config::Paths;
use netodia_core::error::AppError;
use netodia_ui::{AppWindow, Lyrics, Settings};

/// Seed the Settings card's three rows and register their handlers.
pub fn install(ui: &AppWindow, state: &AppState) {
    let g = ui.global::<Settings>();
    // Seeded off the shadow: `AppState` already read the file at boot, and a second read here
    // would answer the same question twice.
    g.set_lyrics_enabled(state.lyrics_enabled.get());
    g.set_lyrics_online_enabled(state.lyrics_online_enabled.get());
    g.set_lyrics_romanization_shown(state.lyrics_romanization_shown.get());

    g.on_lyrics_enabled_changed(enabled_handler(ui, state));
    g.on_lyrics_online_enabled_changed(online_handler(ui, state));
    g.on_lyrics_romanization_shown_changed(romanization_handler(ui, state));
}

/// The handler both controls of the feature switch register.
///
/// The card's row owes Now Playing nothing, for [`online_handler`]'s reason: reaching this page
/// closed that view, and its close already handed the sheet back.
pub(crate) fn enabled_handler(ui: &AppWindow, state: &AppState) -> impl Fn(bool) + 'static {
    mirrored(
        ui,
        state,
        &state.lyrics_enabled,
        "set_lyrics_enabled",
        library::settings::set_lyrics_enabled,
        |ui, on| {
            ui.global::<Settings>().set_lyrics_enabled(on);
            ui.global::<Lyrics>().set_enabled(on);
        },
    )
}

/// The handler both rows of the online-lookup switch register.
///
/// **The card's row owes no re-ask where the menu's does**: reaching this page closed Now Playing,
/// which hands the sheet and its claim back, so the re-open looks up again on its own.
pub(crate) fn online_handler(ui: &AppWindow, state: &AppState) -> impl Fn(bool) + 'static {
    mirrored(
        ui,
        state,
        &state.lyrics_online_enabled,
        "set_lyrics_online_enabled",
        library::settings::set_lyrics_online_enabled,
        |ui, on| {
            ui.global::<Settings>().set_lyrics_online_enabled(on);
            ui.global::<Lyrics>().set_online_enabled(on);
        },
    )
}

/// The handler both rows of the romanization switch register.
///
/// The two views are never on screen together, so nothing has to be redrawn for the card's row;
/// the menu also requests any missing readings and re-lays the sheet it is standing on.
pub(crate) fn romanization_handler(ui: &AppWindow, state: &AppState) -> impl Fn(bool) + 'static {
    mirrored(
        ui,
        state,
        &state.lyrics_romanization_shown,
        "set_lyrics_romanization_shown",
        library::settings::set_lyrics_romanization_shown,
        |ui, shown| {
            ui.global::<Settings>().set_lyrics_romanization_shown(shown);
            ui.global::<Lyrics>().set_romanization_shown(shown);
        },
    )
}

/// [`shadow_toggle`] plus the write that keeps the switch's other row in step.
///
/// `mirror` writes **both** globals, so one handler serves either row and the
/// `(shadow, label, setter)` triple is stated once per switch rather than once per row. Writing a
/// global from inside its own change handler is a value-compared no-op, and the `<=>` a
/// `ToggleSwitch` binds survives a write from Rust.
fn mirrored(
    ui: &AppWindow,
    state: &AppState,
    shadow: &SharedFlag,
    label: &'static str,
    persist: fn(&Paths, bool) -> Result<(), AppError>,
    mirror: fn(&AppWindow, bool),
) -> impl Fn(bool) + 'static {
    let persist = shadow_toggle(state, shadow, label, persist);
    let weak = ui.as_weak();
    move |on| {
        persist(on);
        if let Some(ui) = weak.upgrade() {
            mirror(&ui, on);
        }
    }
}
