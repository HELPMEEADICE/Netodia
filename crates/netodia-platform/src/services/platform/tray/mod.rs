//! Cross-platform system tray icon.
//!
//! Linux uses `ksni` (a pure-Rust `StatusNotifierItem` implementation over
//! D-Bus — no GTK). Windows / macOS use `tray_icon`. Neither is linked: each is
//! a platform-gated dependency, so a link resolves only on the host that has
//! it. The two backends are
//! `cfg`-split; this module is the shared façade — action / snapshot types,
//! the embedded icon, and `init_tray`.
//!
//! ## GNOME caveat
//!
//! GNOME Shell ships no native `StatusNotifier` *host*, so on a vanilla GNOME
//! session the icon simply will not appear (the user needs the `AppIndicator`
//! and `KStatusNotifierItem` Support extension). `init_tray` returns `None` in
//! that case — and on any platform / session without a working tray — and the
//! app stays fully usable: every tray action duplicates an in-app control.
//!
//! ## Menu strings
//!
//! The UI resolves bundled translations and supplies plain menu labels at startup
//! and after every language switch. Neither backend depends on Slint.

#[cfg(target_os = "linux")]
mod ksni_backend;
#[cfg(any(target_os = "windows", target_os = "macos"))]
mod tray_icon_backend;

#[cfg(target_os = "linux")]
pub use ksni_backend::{LinuxTray, init as init_tray};
#[cfg(any(target_os = "windows", target_os = "macos"))]
pub use tray_icon_backend::{init as init_tray, shutdown as shutdown_tray, update as update_tray};

/// Bounded capacity of the `TrayAction` channel. The tray callback thread
/// (ksni's D-Bus thread / tray-icon's event handler) `try_send`s into it; a
/// human clicking menu items can never outrun 16 buffered actions.
pub const TRAY_ACTION_CHANNEL_CAP: usize = 16;

/// A menu action emitted by the tray, drained by `ui::shell::tray_bridge`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayAction {
    /// Toggle play / pause.
    PlayPause,
    /// Skip to the next track.
    Next,
    /// Skip to the previous track.
    Previous,
    /// Toggle the main window between shown and hidden.
    ShowHideWindow,
    /// Quit the application.
    Quit,
}

/// Translated menu labels supplied by the UI thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrayMenuLabels {
    pub play: String,
    pub pause: String,
    pub next: String,
    pub previous: String,
    pub show_hide: String,
    pub quit: String,
}

impl Default for TrayMenuLabels {
    fn default() -> Self {
        Self {
            play: "Play".to_owned(),
            pause: "Pause".to_owned(),
            next: "Next track".to_owned(),
            previous: "Previous".to_owned(),
            show_hide: "Show / Hide Window".to_owned(),
            quit: "Quit Netodia".to_owned(),
        }
    }
}

/// Minimal render state the tray needs: tooltip text, transport state and menu labels.
/// Derived from `PlayerViewModelLight` by `ui::shell::tray_bridge`. `PartialEq`
/// lets the state subscribers skip the blocking tray round-trip when an
/// emit (volume step, seek, queue edit) didn't change anything the tray
/// renders.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TraySnapshot {
    /// Included in equality so a language switch updates an otherwise idle tray.
    pub labels: TrayMenuLabels,
    /// The current track's title, or `None` when nothing is loaded.
    pub track_title: Option<String>,
    /// The current track's artist, or `None` when unknown or nothing is loaded.
    pub track_artist: Option<String>,
    /// `true` while playback status is "playing".
    pub is_playing: bool,
    /// Whether the transport can move off the current track.
    pub has_next: bool,
    pub has_previous: bool,
}

impl TraySnapshot {
    /// Single-line tooltip: `"Title — Artist"`, `"Title"`, or `"Netodia"`.
    fn tooltip(&self) -> String {
        match (&self.track_title, &self.track_artist) {
            (Some(title), Some(artist)) if !artist.is_empty() => {
                format!("{title} — {artist}")
            }
            (Some(title), _) => title.clone(),
            (None, _) => "Netodia".to_owned(),
        }
    }

    /// Label for the disabled track-name row at the top of the menu.
    fn menu_track_label(&self) -> String {
        self.track_title.clone().unwrap_or_else(|| "Netodia".to_owned())
    }

    /// Label for the play/pause menu row.
    fn play_pause_label(&self) -> &str {
        if self.is_playing { &self.labels.pause } else { &self.labels.play }
    }
}

/// The tray icon, baked into the binary and derived from the supplied Netodia logo.
const TRAY_PNG: &[u8] = include_bytes!("../../../../../../assets/icons/tray.png");

/// Decode the embedded tray PNG into `(width, height, rgba8)`. Returns `None`
/// if the embedded asset somehow fails to decode — callers then build the
/// tray without an icon rather than aborting.
fn decode_icon() -> Option<(u32, u32, Vec<u8>)> {
    match image::load_from_memory_with_format(TRAY_PNG, image::ImageFormat::Png) {
        Ok(img) => {
            let rgba = img.to_rgba8();
            let (w, h) = rgba.dimensions();
            Some((w, h, rgba.into_raw()))
        }
        Err(e) => {
            log::warn!(
                "tray: failed to decode embedded icon: {}",
                netodia_core::error::describe(&e)
            );
            None
        }
    }
}

#[cfg(test)]
#[path = "tests/tray_tests.rs"]
mod tests;
