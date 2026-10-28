//! Linux tray backend — a `StatusNotifierItem` published over D-Bus via
//! [`ksni`]. ksni runs the SNI service on its own thread (the `blocking`
//! API), so nothing here touches the Slint event loop or the tokio runtime.

use ksni::menu::{MenuItem, StandardItem};
use ksni::{Icon, ToolTip};
use tokio::sync::mpsc;

use netodia_core::error::describe;

use super::{TrayAction, TrayMenuLabels, TraySnapshot};

/// ksni [`Tray`](ksni::Tray) implementation. Holds the latest render snapshot
/// plus the channel back to `ui::shell::tray_bridge`. Menu callbacks `try_send`
/// `TrayAction`s — non-blocking, so the D-Bus thread never stalls on a full
/// channel (it just drops the action, which a human clicker cannot trigger).
struct NetodiaTray {
    snapshot: TraySnapshot,
    icon: Vec<Icon>,
    action_tx: mpsc::Sender<TrayAction>,
}

impl NetodiaTray {
    /// Build one menu row that emits `action` on click.
    fn action_item(label: &str, action: TrayAction) -> MenuItem<Self> {
        Self::action_item_enabled(label, action, true)
    }

    fn action_item_enabled(label: &str, action: TrayAction, enabled: bool) -> MenuItem<Self> {
        StandardItem {
            label: label.to_owned(),
            enabled,
            activate: Box::new(move |t: &mut Self| {
                if let Err(e) = t.action_tx.try_send(action) {
                    log::warn!("tray: dropped {action:?} (channel full): {}", describe(&e));
                }
            }),
            ..Default::default()
        }
        .into()
    }
}

impl ksni::Tray for NetodiaTray {
    fn id(&self) -> String {
        "netodia".to_owned()
    }

    fn title(&self) -> String {
        "Netodia".to_owned()
    }

    fn icon_pixmap(&self) -> Vec<Icon> {
        self.icon.clone()
    }

    fn tool_tip(&self) -> ToolTip {
        ToolTip {
            title: self.snapshot.tooltip(),
            description: String::new(),
            icon_name: String::new(),
            icon_pixmap: Vec::new(),
        }
    }

    /// Left-click on the icon toggles the main window.
    fn activate(&mut self, _x: i32, _y: i32) {
        if let Err(e) = self.action_tx.try_send(TrayAction::ShowHideWindow) {
            log::warn!("tray: dropped activate (channel full): {}", describe(&e));
        }
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        vec![
            // Disabled track-label row.
            StandardItem {
                label: self.snapshot.menu_track_label(),
                enabled: false,
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            Self::action_item(self.snapshot.play_pause_label(), TrayAction::PlayPause),
            Self::action_item_enabled(
                &self.snapshot.labels.next,
                TrayAction::Next,
                self.snapshot.has_next,
            ),
            Self::action_item_enabled(
                &self.snapshot.labels.previous,
                TrayAction::Previous,
                self.snapshot.has_previous,
            ),
            MenuItem::Separator,
            Self::action_item(&self.snapshot.labels.show_hide, TrayAction::ShowHideWindow),
            Self::action_item(&self.snapshot.labels.quit, TrayAction::Quit),
        ]
    }
}

/// Live handle to the running Linux tray. `Send` (ksni's `blocking::Handle`
/// is `Send`), so `ui::shell::tray_bridge` can own it from a background tokio task.
/// Dropping it shuts the SNI service down, removing the icon.
pub struct LinuxTray {
    handle: ksni::blocking::Handle<NetodiaTray>,
}

impl LinuxTray {
    /// Push a fresh snapshot — re-publishes the tooltip + menu over D-Bus.
    pub fn update(&self, snapshot: &TraySnapshot) {
        let snapshot = snapshot.clone();
        self.handle.update(move |tray| {
            tray.snapshot = snapshot;
        });
    }
}

impl Drop for LinuxTray {
    fn drop(&mut self) {
        // Fire-and-forget: the shutdown request is queued to the service
        // thread, which drops the SNI registration and removes the icon.
        let _ = self.handle.shutdown();
    }
}

/// Convert tightly-packed RGBA8 into the ARGB32 (network byte order) layout
/// ksni's `Icon` expects: `[r,g,b,a]` rotated right by one → `[a,r,g,b]`.
fn rgba_to_argb(mut rgba: Vec<u8>) -> Vec<u8> {
    for px in rgba.chunks_exact_mut(4) {
        px.rotate_right(1);
    }
    rgba
}

/// The embedded icon. Empty if the asset won't decode.
fn icon_for() -> Vec<Icon> {
    super::decode_icon()
        .and_then(|(w, h, rgba)| {
            Some(Icon {
                width: i32::try_from(w).ok()?,
                height: i32::try_from(h).ok()?,
                data: rgba_to_argb(rgba),
            })
        })
        .map(|icon| vec![icon])
        .unwrap_or_default()
}

/// Spawn the `StatusNotifierItem` service. Returns `None` when the session has
/// no SNI host (vanilla GNOME) or no D-Bus — the app then runs without a tray.
pub fn init(action_tx: mpsc::Sender<TrayAction>, labels: &TrayMenuLabels) -> Option<LinuxTray> {
    use ksni::blocking::TrayMethods;

    let icon = icon_for();
    let snapshot = TraySnapshot { labels: labels.clone(), ..Default::default() };
    let tray = NetodiaTray { snapshot, icon, action_tx };

    match tray.spawn() {
        Ok(handle) => {
            log::info!("System tray registered (StatusNotifierItem)");
            Some(LinuxTray { handle })
        }
        Err(e) => {
            log::info!(
                "System tray unavailable ({}) — running without a tray icon. \
                 On GNOME this needs the AppIndicator extension.",
                describe(&e)
            );
            None
        }
    }
}
