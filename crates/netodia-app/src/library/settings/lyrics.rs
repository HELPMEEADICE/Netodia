//! The lyrics feature's three switches. Persist to `settings.json`; the shadows on `AppState` are
//! written synchronously by the UI callback *before* the persist is spawned, so a reader racing the
//! disk write sees the new answer rather than the old file.

use crate::services;
use netodia_core::config::Paths;
use netodia_core::error::AppError;

/// Persist whether lyrics run at all, which is also whether the Now Playing column shows them
/// instead of Up Next.
pub fn set_lyrics_enabled(paths: &Paths, enabled: bool) -> Result<(), AppError> {
    services::settings::mutate_settings(paths, move |settings| {
        settings.lyrics.lyrics_enabled = enabled;
    })
}

/// Persist whether a track with no sheet of its own may be looked up online. Off by default
/// (opt-in), for the reason every other outbound feature is.
pub fn set_lyrics_online_enabled(paths: &Paths, enabled: bool) -> Result<(), AppError> {
    services::settings::mutate_settings(paths, move |settings| {
        settings.lyrics.lyrics_online_enabled = enabled;
    })
}

/// Persist whether the panel draws readings and generates them for sheets with none of their own.
///
/// **The one writer, and it has two callers**: the Settings card and the Now Playing menu. A
/// second `mutate_settings` beside it is how the two would come to disagree about the field's
/// name, which is why each of the three switches has exactly one.
pub fn set_lyrics_romanization_shown(paths: &Paths, shown: bool) -> Result<(), AppError> {
    services::settings::mutate_settings(paths, move |settings| {
        settings.lyrics.lyrics_romanization_shown = shown;
    })
}

#[cfg(test)]
#[path = "tests/lyrics_tests.rs"]
mod tests;
