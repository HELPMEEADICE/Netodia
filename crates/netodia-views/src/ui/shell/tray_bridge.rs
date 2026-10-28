//! Glue between the system tray ([`netodia_platform::services::platform::tray`]) and the rest of
//! the app.
//!
//! Three pieces:
//!
//! 1. **Action receiver** — a tracked tokio task draining the tray's
//!    `mpsc::Receiver<TrayAction>`. Playback actions feed the same [`EventSink`] the OS
//!    media controls use; window actions hop to the UI thread.
//! 2. **State subscriber** — watches `sinks.view_model` and pushes a [`TraySnapshot`]
//!    into the tray. On Linux a tokio task owning the `Send` `LinuxTray`; on Windows and
//!    macOS the handle is `!Send`, so a `spawn_local` future on the UI thread.
//! 3. **Close-to-tray state** — process-global atomics `window_chrome` reads to decide
//!    whether a close hides or quits, plus the visibility shadow the tray's Show / Hide
//!    toggles against, `winit::Window::is_visible()` being `None` on Wayland.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use slint::ComponentHandle;
use tokio::sync::watch;

use crate::ui::locale_refresh::on_locale_changed;
use crate::ui::shell::event_sink::SlintEventSink;
use netodia_app::state::AppState;
use netodia_app::tasks::TaskSpawner;
use netodia_core::error::describe;
use netodia_engine::player::engine::event_sink::{EventSink, PlayerEvent};
use netodia_engine::player::engine::state::PlayerViewModelLight;
use netodia_platform::services::platform::tray::{
    self, TRAY_ACTION_CHANNEL_CAP, TrayAction, TrayMenuLabels, TraySnapshot,
};
use netodia_ui::{AppWindow, Settings, Visualizer, WindowChrome};

/// Mirrors `settings.tray.close_to_tray`, so `window_chrome`'s close handlers
/// read the preference without touching disk.
static CLOSE_TO_TRAY: AtomicBool = AtomicBool::new(false);

/// Shadows window visibility for the tray's Show / Hide toggle:
/// `winit::Window::is_visible()` is `None` on Wayland.
static WINDOW_VISIBLE: AtomicBool = AtomicBool::new(true);

/// `true` once a tray icon exists. Close-to-tray falls back to quitting when it doesn't
/// — on a session with no tray the setting would otherwise strand the user with a hidden
/// window and no way back.
static TRAY_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Captured the instant the window is hidden, re-applied on the next show: hiding destroys
/// the winit window, and Slint's first layout pass on the recreated one snaps to the
/// content-preferred width and lets the compositor re-place it.
static SAVED_WINDOW_GEOM: Mutex<Option<(slint::PhysicalSize, slint::PhysicalPosition)>> =
    Mutex::new(None);

/// How many times a tray-show re-asserts the saved geometry before giving up, one timer
/// tick each.
const RESTORE_ATTEMPTS: u8 = 8;

/// Update the close-to-tray preference — the Settings toggle, and one seed at startup.
pub fn set_close_to_tray(on: bool) {
    CLOSE_TO_TRAY.store(on, Ordering::Relaxed);
}

/// `true` when a window-close should hide to tray rather than quit: the user
/// enabled the setting *and* a tray icon is actually active.
pub fn should_hide_to_tray() -> bool {
    CLOSE_TO_TRAY.load(Ordering::Relaxed) && TRAY_ACTIVE.load(Ordering::Relaxed)
}

/// Update the visibility shadow from outside this module. Three callers keep it honest —
/// the titlebar minimize handler, the winit filter's deferred `is_minimized` probe, and
/// the `Focused(true)` listener. Let it desynchronise and the next tray click hides an
/// already-minimized surface, so restoring takes two.
///
/// `Visualizer.window-shown` moves with it — the strip's Timer gates on the Slint half
/// while its tick reads the atomic, so one writer rather than two. That gate is also why
/// hiding hands the visualizer its own notice: lowering `window-shown` can stop the Timer
/// that would have delivered it.
pub fn set_window_visible(ui: &AppWindow, visible: bool) {
    WINDOW_VISIBLE.store(visible, Ordering::Relaxed);
    let viz = ui.global::<Visualizer>();
    viz.set_window_shown(visible);
    if visible {
        // The strip may have gone dormant off screen, leaving its Timer at the polling
        // rate. This *is* the notice; don't make it infer the same thing.
        viz.set_dormant(false);
    } else {
        // The tap can't wait for the strip's next tick: `window-shown` gates that Timer
        // too, so the call above may have just stopped it.
        viz.invoke_window_hidden();
    }
}

/// Read the visibility shadow, for UI work that keeps running off a `Timer` while the
/// window is hidden — Slint timers fire off the event loop, which deliberately survives
/// a close-to-tray hide. A cheap gate rather than a guarantee: Wayland tells a client
/// nothing about being minimized, and `ui::visualizer::pulse` covers the rest.
#[must_use]
pub fn is_window_visible() -> bool {
    WINDOW_VISIBLE.load(Ordering::Relaxed)
}

/// Hide the main window to the tray, snapshotting geometry for `show_window`. UI thread.
///
/// A caller inside a winit `WindowEvent` dispatch must defer through
/// `slint::invoke_from_event_loop`: hiding mid-dispatch leaves the window `Arc` borrowed
/// by the dispatcher and Slint logs "references to the window still exist".
pub fn hide_window(ui: &AppWindow) {
    let window = ui.window();
    if let Ok(mut slot) = SAVED_WINDOW_GEOM.lock() {
        *slot = Some((window.size(), window.position()));
    }
    match window.hide() {
        Ok(()) => set_window_visible(ui, false),
        Err(e) => log::warn!("tray: failed to hide the window: {}", describe(&e)),
    }
}

/// Show the main window and restore its pre-hide geometry. UI thread. Two hidden-states,
/// told apart by whether `SAVED_WINDOW_GEOM` is populated: **`Some`** means `hide_window`
/// unmapped the surface and `show()` re-maps it; **`None`** means the titlebar button
/// minimized it, and Wayland has no client-side un-minimize — `xdg_toplevel::set_minimized`
/// has no inverse and winit's `focus_window` is a no-op there — so the only way back is
/// `hide()` + `show()`, geometry snapshotted first.
///
/// Both converge on `set_size` + `reschedule_geometry_restore`: the synchronous resize sets
/// `has_explicit_size` against Slint's first layout pass snapping to the layout minimum,
/// and the tick re-asserts until it sticks. That correction has to run from a timer,
/// between frames — resizing mid-dispatch desyncs the renderer.
fn show_window(ui: &AppWindow) {
    let window = ui.window();
    let from_tray_hide = SAVED_WINDOW_GEOM.lock().ok().and_then(|mut slot| slot.take());

    let geom = if let Some(g) = from_tray_hide {
        if let Err(e) = window.show() {
            log::warn!("tray: failed to show the window: {}", describe(&e));
            return;
        }
        g
    } else {
        // Button-minimize: snapshot before the hide tears the surface down, the
        // recreate losing both otherwise.
        let g = (window.size(), window.position());
        if let Err(e) = window.hide() {
            log::warn!("tray: hide-for-restore failed: {}", describe(&e));
            return;
        }
        if let Err(e) = window.show() {
            log::warn!("tray: failed to show the window: {}", describe(&e));
            return;
        }
        g
    };

    let (size, position) = geom;
    window.set_size(size);
    window.set_position(position);
    log::debug!(
        "tray: restoring window geometry {}x{} @ {},{}",
        size.width,
        size.height,
        position.x,
        position.y
    );
    reschedule_geometry_restore(ui.as_weak(), size, position, RESTORE_ATTEMPTS);

    set_window_visible(ui, true);
}

/// Bring the window to the user, whatever put it out of reach. Hidden — tray or minimize —
/// goes through [`show_window`], the only path that knows about `SAVED_WINDOW_GEOM`.
/// Merely buried is the window server's business, and Wayland gives a client no say.
pub fn raise_window(ui: &AppWindow) {
    use slint::winit_030::{WinitWindowAccessor, winit::window::Window as WinitWindow};

    if !is_window_visible() {
        show_window(ui);
        return;
    }
    ui.window().with_winit_window(WinitWindow::focus_window);
}

/// Re-assert `size` and `position` from a single-shot timer, rescheduling until the size
/// matches or `tries` runs out. The winit window is recreated asynchronously after
/// `show()`, so the geometry has to be re-applied once it settles, from a timer between
/// frames. Position rides along on every tick, being idempotent; only the size has the
/// layout-snap race the "settled" check is for.
fn reschedule_geometry_restore(
    weak: slint::Weak<AppWindow>,
    size: slint::PhysicalSize,
    position: slint::PhysicalPosition,
    tries: u8,
) {
    slint::Timer::single_shot(std::time::Duration::from_millis(16), move || {
        let Some(ui) = weak.upgrade() else { return };
        let window = ui.window();
        let current = window.size();
        if current.width == size.width && current.height == size.height {
            return; // settled
        }
        window.set_size(size);
        window.set_position(position);
        if tries > 1 {
            reschedule_geometry_restore(weak, size, position, tries - 1);
        }
    });
}

/// Create the tray and spawn the action receiver and state subscriber. Call once during
/// startup, before `app.show()`.
pub fn install(spawner: &TaskSpawner, state: &AppState, ui: &AppWindow) {
    let (tx, rx) = tokio::sync::mpsc::channel::<TrayAction>(TRAY_ACTION_CHANNEL_CAP);
    spawn_action_receiver(spawner, state, ui.as_weak(), rx);
    let (labels_tx, labels_rx) = watch::channel(menu_labels(ui));
    if let Err(e) = on_locale_changed(state, ui.as_weak(), move |ui| {
        labels_tx.send_replace(menu_labels(ui));
    }) {
        log::warn!("tray: locale subscriber failed to spawn: {}", describe(&e));
    }

    #[cfg(target_os = "linux")]
    {
        // Linux: ksni runs on its own thread — create eagerly.
        let linux_tray = tray::init_tray(tx, &labels_rx.borrow());
        match linux_tray {
            Some(linux_tray) => {
                TRAY_ACTIVE.store(true, Ordering::Relaxed);
                ui.global::<Settings>().set_tray_active(true);
                spawn_state_subscriber_linux(spawner, state, linux_tray, labels_rx);
            }
            None => log::info!("tray: not active — close-to-tray will fall back to quitting"),
        }
    }
    #[cfg(any(target_os = "windows", target_os = "macos"))]
    {
        // `tray-icon` wants the UI thread with the event loop already running, so defer
        // the way boot defers the SMTC attach.
        let sinks = state.sinks.clone();
        let weak = ui.as_weak();
        if let Err(e) = slint::invoke_from_event_loop(move || {
            let active = tray::init_tray(tx, &labels_rx.borrow());
            if active {
                TRAY_ACTIVE.store(true, Ordering::Relaxed);
                if let Some(ui) = weak.upgrade() {
                    ui.global::<Settings>().set_tray_active(true);
                }
                spawn_state_subscriber_local(&sinks, labels_rx);
            } else {
                log::info!("tray: not active — close-to-tray will fall back to quitting");
            }
        }) {
            log::warn!("tray: failed to schedule creation: {}", describe(&e));
        }
    }
}

/// Tear the tray down, on the UI thread and before `process::exit` skips every
/// destructor. No-op on Linux, whose `LinuxTray` its subscriber drops.
pub fn shutdown() {
    #[cfg(any(target_os = "windows", target_os = "macos"))]
    tray::shutdown_tray();
}

/// Drain `TrayAction`s: playback actions into the shared `EventSink`, window
/// actions onto the UI thread.
fn spawn_action_receiver(
    spawner: &TaskSpawner,
    state: &AppState,
    ui_weak: slint::Weak<AppWindow>,
    mut rx: tokio::sync::mpsc::Receiver<TrayAction>,
) {
    let sink: Arc<dyn EventSink> = Arc::new(SlintEventSink { state: state.clone() });
    spawner.spawn_cancellable(move |shutdown| async move {
        loop {
            tokio::select! {
                biased;
                () = shutdown.cancelled() => break,
                maybe = rx.recv() => match maybe {
                    Some(action) => dispatch_action(action, &sink, &ui_weak),
                    None => break,
                },
            }
        }
        log::info!("tray: action receiver stopped");
    });
}

/// Route one `TrayAction`. Playback actions reuse the OS-media-controls path;
/// `ShowHideWindow` / `Quit` need the UI thread.
fn dispatch_action(
    action: TrayAction,
    sink: &Arc<dyn EventSink>,
    ui_weak: &slint::Weak<AppWindow>,
) {
    match action {
        TrayAction::PlayPause => sink.handle(PlayerEvent::PlayPause),
        TrayAction::Next => sink.handle(PlayerEvent::Next),
        TrayAction::Previous => sink.handle(PlayerEvent::Previous),
        TrayAction::ShowHideWindow => {
            let weak = ui_weak.clone();
            if let Err(e) = slint::invoke_from_event_loop(move || toggle_window(&weak)) {
                log::warn!("tray: show/hide invoke failed: {}", describe(&e));
            }
        }
        TrayAction::Quit => {
            if let Err(e) = slint::invoke_from_event_loop(|| {
                if let Err(e) = slint::quit_event_loop() {
                    log::warn!("tray: quit_event_loop: {}", describe(&e));
                }
            }) {
                log::warn!("tray: quit invoke failed: {}", describe(&e));
            }
        }
    }
}

/// Toggle the main window. UI thread, reached via `invoke_from_event_loop` so it runs in
/// `user_event` context rather than a `WindowEvent` dispatch — see `hide_window`. Slint's
/// `Window::show`/`hide` is the toolkit-level hide that actually unmaps the Wayland
/// surface; winit's `set_visible` is a documented no-op there.
fn toggle_window(ui_weak: &slint::Weak<AppWindow>) {
    let Some(ui) = ui_weak.upgrade() else { return };
    if WINDOW_VISIBLE.load(Ordering::Relaxed) {
        hide_window(&ui);
    } else {
        show_window(&ui);
    }
}

/// Resolve the native tray's labels through Slint's bundled translations on the UI thread.
fn menu_labels(ui: &AppWindow) -> TrayMenuLabels {
    let chrome = ui.global::<WindowChrome>();
    TrayMenuLabels {
        play: chrome.get_tray_play_label().to_string(),
        pause: chrome.get_tray_pause_label().to_string(),
        next: chrome.get_tray_next_label().to_string(),
        previous: chrome.get_tray_previous_label().to_string(),
        show_hide: chrome.get_tray_show_hide_label().to_string(),
        quit: chrome.get_tray_quit_label().to_string(),
    }
}

/// Build a `TraySnapshot` from the latest light view-model and translated labels.
///
/// Through `source()`, so the tooltip uses the same title and artist as the OS media controls.
fn snapshot_from_vm(vm: Option<&PlayerViewModelLight>, labels: &TrayMenuLabels) -> TraySnapshot {
    let Some(vm) = vm else {
        return TraySnapshot { labels: labels.clone(), ..Default::default() };
    };
    let source = vm.source();
    TraySnapshot {
        labels: labels.clone(),
        track_title: source.as_ref().map(|s| s.title.to_owned()),
        track_artist: source.as_ref().and_then(|s| s.secondary.map(str::to_owned)),
        is_playing: vm.status == "playing",
        has_next: vm.has_next,
        has_previous: vm.has_previous,
    }
}

/// Linux state subscriber: a tokio task owning the `Send` `LinuxTray`. Dropping the tray
/// when the task ends removes the icon, so no explicit teardown is needed.
#[cfg(target_os = "linux")]
fn spawn_state_subscriber_linux(
    spawner: &TaskSpawner,
    state: &AppState,
    tray: tray::LinuxTray,
    mut labels_rx: watch::Receiver<TrayMenuLabels>,
) {
    let mut rx = state.sinks.view_model.subscribe();
    spawner.spawn_cancellable(move |shutdown| async move {
        // `update` is a blocking D-Bus round trip to ksni's service thread, so
        // `block_in_place` lends the worker out rather than stalling the runtime. The
        // channel emits on *every* state change where the tray renders only title, artist
        // and play/pause, so diff against the last snapshot to keep traffic per-track.
        let mut last =
            snapshot_from_vm(rx.borrow_and_update().as_ref(), &labels_rx.borrow_and_update());
        tokio::task::block_in_place(|| tray.update(&last));
        loop {
            tokio::select! {
                () = shutdown.cancelled() => break,
                changed = rx.changed() => {
                    if changed.is_err() {
                        break;
                    }
                }
                changed = labels_rx.changed() => {
                    if changed.is_err() {
                        break;
                    }
                }
            }
            let snapshot =
                snapshot_from_vm(rx.borrow_and_update().as_ref(), &labels_rx.borrow_and_update());
            if snapshot == last {
                continue;
            }
            tokio::task::block_in_place(|| tray.update(&snapshot));
            last = snapshot;
        }
        log::info!("tray: state subscriber stopped");
    });
}

/// Windows / macOS state subscriber: a `spawn_local` future on the UI thread, the
/// `tray-icon` handle being `!Send`. Must be spawned from there — `install` calls it
/// inside `invoke_from_event_loop`.
#[cfg(any(target_os = "windows", target_os = "macos"))]
fn spawn_state_subscriber_local(
    sinks: &Arc<netodia_engine::player::engine::event_sink::PlayerSinks>,
    mut labels_rx: watch::Receiver<TrayMenuLabels>,
) {
    let mut rx = sinks.view_model.subscribe();
    let res = slint::spawn_local(async_compat::Compat::new(async move {
        // Diff as the Linux subscriber does.
        let mut last =
            snapshot_from_vm(rx.borrow_and_update().as_ref(), &labels_rx.borrow_and_update());
        tray::update_tray(&last);
        loop {
            tokio::select! {
                changed = rx.changed() => {
                    if changed.is_err() {
                        break;
                    }
                }
                changed = labels_rx.changed() => {
                    if changed.is_err() {
                        break;
                    }
                }
            }
            let snapshot =
                snapshot_from_vm(rx.borrow_and_update().as_ref(), &labels_rx.borrow_and_update());
            if snapshot == last {
                continue;
            }
            tray::update_tray(&snapshot);
            last = snapshot;
        }
        log::debug!("tray: state subscriber stopped");
    }));
    if let Err(e) = res {
        log::warn!("tray: state subscriber failed to spawn: {}", describe(&e));
    }
}
