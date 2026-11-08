//! Whether Windows draws light or dark, under Settings ▸ Personalization ▸ Colors. The default app
//! mode is the reading the selected theme's System variant follows.
//!
//! **The app mode fails light**: a value that won't read answers light, the mode Windows applies to
//! apps until the user picks another.

use super::registry::read_user_dword;

const PERSONALIZE_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize";

/// The default app mode, as `"light"` or `"dark"`, the vocabulary `SystemColorState::theme` is
/// written in.
pub fn app_theme() -> &'static str {
    theme_for(read_user_dword(PERSONALIZE_KEY, "AppsUseLightTheme"))
}

/// The Windows mode, which the taskbar and its flyouts draw in, independent of the app mode.
///
/// **Fails dark**, unlike the app mode: builds before 1903 have no value here and a dark taskbar.
pub fn system_theme() -> &'static str {
    if read_user_dword(PERSONALIZE_KEY, "SystemUsesLightTheme") == Some(1) {
        "light"
    } else {
        "dark"
    }
}

/// The decision, apart from the registry: dark only while the value says so outright.
fn theme_for(uses_light_theme: Option<u32>) -> &'static str {
    if uses_light_theme == Some(0) { "dark" } else { "light" }
}

#[cfg(test)]
#[path = "tests/color_mode_tests.rs"]
mod tests;
