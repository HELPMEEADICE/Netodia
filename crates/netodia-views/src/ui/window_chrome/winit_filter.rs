//! Winit `WindowEvent` filter installed by `window_chrome::install`.
//!
//! Ten reasons to subscribe:
//!
//! 1. **`MouseInput { Pressed, Left }`** over a `ResizeRing` grab or a drag region: an
//!    OS-level resize or window move plus `PreventDefault`, bypassing the TouchArea-grab
//!    issue the parent module's docs describe. The titlebar's double press toggles maximize
//!    instead, Slint never seeing the pair (`drag_region`).
//! 2. **`MouseInput { Pressed, Back | Forward }`** — Mouse-4 / Mouse-5 into the nav
//!    history, gated on no overlay being open so the buttons don't fight its input
//!    context.
//! 3. **`Resized`** — re-read `is_maximized`, so the maximize/restore icon stays in sync
//!    with Win+↑ and window-tile shortcuts that bypass our buttons. Also the two frame readings:
//!    the step a decoration change costs the client, which the miniplayer's exit edge allows for
//!    once it drops the frame, and on Win32 the invisible borders it keeps transparent so the
//!    window's edges stay put.
//! 4. **`Focused(bool)`** — mirror OS focus into `Theme.window-focused`, and use the
//!    transition to reconcile the tray-bridge's visibility shadow: raise on focus, ask
//!    the OS about a minimize on focus loss (see [`schedule_minimize_probe`]).
//! 5. **`DroppedFile` / `HoveredFile` / `HoveredFileCancelled`** — Slint 1.16 has no
//!    cross-app `DnD` API, so this is the only path a file drop arrives through.
//! 6. **`MouseWheel`**, for two unrelated `Flickable` defects — see [`route_wheel`].
//!    `CursorMoved` is mirrored for the same arm, a wheel event carrying no position.
//! 7. **`RedrawRequested`** and **`Moved`** — the Slint tick Win32's modal resize-and-move
//!    loop parks winit out of; see `parked_loop`.
//! 8. **`ThemeChanged`**, Windows only: a light/dark flip for a theme on its System
//!    variant, re-read by `ui::appearance` rather than taken from the event, whose theme
//!    also folds in high contrast, and a cue for the tray icon to re-read its taskbar.
//! 9. **`Resized`**, **`CursorMoved`**, **`CursorEntered`** and **`MouseInput`** together, for
//!    where a resize begins and where it is let go, which no one event reports. See
//!    `resize_release`.
//! 10. **`Touch`** — selects finger panning before dispatch and hands chrome presses to the OS
//!     without leaking a Slint grab (`touch_input`).

use std::cell::Cell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use slint::winit_030::winit::error::ExternalError;
use slint::winit_030::winit::event::{
    ElementState, MouseButton, MouseScrollDelta, TouchPhase, WindowEvent,
};
use slint::winit_030::winit::window::{ResizeDirection, Window as WinitWindow};
use slint::winit_030::{EventResult, WinitWindowAccessor};
use slint::{ComponentHandle, SharedString};

use netodia_app::state::AppState;
use netodia_core::error::describe;
use netodia_ui::{
    AppWindow, CompositeScroll, DragRegion, PlaylistDetail, PopupHighlight, Queue, Theme,
    WindowChrome,
};

use super::drag_region::{DoublePress, PressAction};
#[cfg(target_os = "windows")]
use super::parked_loop;
use super::resize_release::ResizeWatch;
use super::{drop_coalescer, geometry};

mod touch_input;

/// How long to wait after a focus loss before asking whether it was a minimize.
///
/// The window manager sets the minimized state and drops focus at about the same time
/// and not reliably in that order — X11 publishes `_NET_WM_STATE_HIDDEN` on its own
/// event stream, Win32 sends `WM_ACTIVATE` and `WM_SIZE` independently. Asking a moment
/// later sidesteps the ordering question, for one deferred timer per focus loss.
const MINIMIZE_PROBE_DELAY: Duration = Duration::from_millis(250);

/// Ask the OS whether a focus loss was really a minimize, and drop the visibility shadow
/// if it was — bringing a minimize we didn't drive (the native titlebar's button, a
/// taskbar click, "show desktop") to the same gates our own button reaches.
///
/// `winit::Window::is_minimized` answers `None` on Wayland, the protocol never telling a
/// client it was minimized, so that case is left as it was and `ui::visualizer::pulse`
/// covers it.
fn schedule_minimize_probe(weak: slint::Weak<AppWindow>) {
    slint::Timer::single_shot(MINIMIZE_PROBE_DELAY, move || {
        let Some(ui) = weak.upgrade() else { return };
        let minimized = ui.window().with_winit_window(WinitWindow::is_minimized).flatten();
        if minimized == Some(true) {
            crate::ui::shell::tray_bridge::set_window_visible(&ui, false);
        }
    });
}

/// What the `MouseWheel` arm does with one event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WheelRoute {
    /// Into the mounted composite view's own scroll math.
    Composite,
    /// Re-send to Slint without the gesture phase, then swallow the original.
    Unphased,
    /// Leave it to Slint.
    Native,
}

/// Decide where one wheel event goes. Two unrelated `Flickable` defects meet here, and the
/// composite view is asked first — it owns its region outright.
///
/// `Composite` is the nested-`ListView`-swallows-the-wheel one, argued at `CompositeScroll`
/// in `globals/shell.slint`. Horizontal-dominant wheel stays native there, that axis
/// belonging to the strips' own scrollers.
///
/// `Unphased` is the touchpad one. `Flickable` intercepts `TouchPhase::Started`
/// unconditionally and sets its capture flag without checking the delta's axis, so the
/// outermost one under the pointer owns the whole gesture where its `Moved` and
/// `Cancelled` arms check direction first — and only a precision device sends the phase,
/// which is why a scroller nested inside another scrolls under a wheel and not two fingers.
/// Re-sending unphased leaves the capture flag unset, costing Slint's kinetic fling.
fn route_wheel(
    composite_hovered: bool,
    overlay_open: bool,
    phase: TouchPhase,
    dx: f32,
    dy: f32,
) -> WheelRoute {
    if composite_hovered && !overlay_open && dy != 0.0 && dy.abs() >= dx.abs() {
        return WheelRoute::Composite;
    }
    if matches!(phase, TouchPhase::Started) {
        return WheelRoute::Unphased;
    }
    WheelRoute::Native
}

/// Hands a left press to the OS as a window move or resize. A failure is logged and left at that:
/// macOS answers `NotSupported` for a resize and resizes a borderless window at its own edges, and
/// an unsupported move leaves the window where it is.
fn start_os_drag(
    w: &slint::Window,
    what: &str,
    op: impl FnOnce(&WinitWindow) -> Result<(), ExternalError>,
) {
    let _ = w.with_winit_window(|ww| {
        if let Err(e) = op(ww) {
            log::debug!("{what} unsupported on this platform: {}", describe(&e));
        }
    });
}

/// Takes down both drop banners, whichever of the two a drop would have landed in.
fn clear_drop_hover(ui: &AppWindow) {
    ui.global::<Queue>().set_is_drop_hovered(false);
    ui.global::<PlaylistDetail>().set_is_drop_hovered(false);
}

/// What the pointer is over that turns a left press into an OS window operation, each kept in
/// step with Slint hover by a `WindowChrome` callback.
pub(super) struct PressTargets {
    /// Which drag region, if any.
    pub(super) drag_region: Rc<Cell<DragRegion>>,
    /// Over one of `ResizeRing`'s grabs, and which way it resizes.
    pub(super) resize: Rc<Cell<Option<ResizeDirection>>>,
}

pub(super) fn install(app: &AppWindow, state: &AppState, targets: PressTargets) {
    let mut touch_input = touch_input::TouchInput::new(&targets);
    let PressTargets { drag_region, resize } = targets;
    let weak = app.as_weak();
    let state = state.clone();
    // Where the pointer is, for the synthetic scroll and the titlebar's double press below.
    let mut cursor_pos = slint::LogicalPosition::default();
    let mut double_press = DoublePress::default();
    let resize_watch = ResizeWatch::new(app.as_weak());
    app.window().on_winit_window_event(move |w, event| {
        touch_input::update_mode(&weak, event);
        match event {
            WindowEvent::Resized(size) => resize_watch.resized(w, *size),
            WindowEvent::CursorMoved { .. }
            | WindowEvent::CursorEntered { .. }
            | WindowEvent::MouseInput { .. } => resize_watch.pointer(w),
            _ => {}
        }
        match event {
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                if let Some(ui) = weak.upgrade() {
                    crate::ui::appearance::ui_scale::sync(&ui, Some(*scale_factor));
                }
                EventResult::PreventDefault
            }
            // Ahead of the drag arm: the miniplayer's drag region runs right up to the edge, and its
            // hover can still read true for a tick after the pointer reaches a grab.
            WindowEvent::MouseInput {
                state: ElementState::Pressed,
                button: MouseButton::Left,
                ..
            } if let Some(direction) = resize.get() => {
                start_os_drag(w, "drag_resize_window", |ww| ww.drag_resize_window(direction));
                EventResult::PreventDefault
            }
            WindowEvent::MouseInput {
                state: ElementState::Pressed,
                button: MouseButton::Left,
                ..
            } if drag_region.get() != DragRegion::None => {
                drag_press(w, &weak, &mut double_press, drag_region.get(), cursor_pos)
            }
            // Above the `Released` cleanup arm because the press is what wants
            // `PreventDefault`.
            WindowEvent::MouseInput {
                state: ElementState::Pressed,
                button: button @ (MouseButton::Back | MouseButton::Forward),
                ..
            } => nav_button(&weak, &state, *button),
            WindowEvent::MouseInput { state: ElementState::Released, .. } => {
                release_popup_highlight(&weak);
                EventResult::Propagate
            }
            WindowEvent::Touch(touch) => touch_input.handle(w, &weak, touch, &mut double_press),
            WindowEvent::Resized(_) => on_resized(w, &weak),
            // X11/Win32/macOS deliver these. Wayland never does, a client there not knowing its
            // own position: position restore is a no-op there anyway, but a drag pressed again
            // inside the double-press interval still toggles maximize.
            WindowEvent::Moved(_) => {
                double_press.moved();
                let _ = w.with_winit_window(|ww| {
                    geometry::record(ww, geometry::WindowReading::take(ww));
                });
                // A move drag resizes nothing, so the client area is never invalidated and no
                // `WM_PAINT` starts the redraw chain the arm below rides.
                #[cfg(target_os = "windows")]
                parked_loop::pump();
                EventResult::Propagate
            }
            WindowEvent::Focused(focused) => on_focus(&weak, *focused),
            #[cfg(target_os = "windows")]
            WindowEvent::ThemeChanged(_) => {
                let _ = weak.upgrade_in_event_loop(|ui| {
                    ui.global::<netodia_ui::WindowChrome>().invoke_recheck_system_theme();
                });
                EventResult::Propagate
            }
            WindowEvent::DroppedFile(path) => on_dropped_file(&state, &weak, path.clone()),
            WindowEvent::HoveredFile(_) => on_hovered_file(&weak),
            WindowEvent::HoveredFileCancelled => {
                let _ = weak.upgrade_in_event_loop(|ui| clear_drop_hover(&ui));
                EventResult::Propagate
            }
            WindowEvent::CloseRequested => on_close_requested(&weak),
            // The same conversion the Slint backend does for its own copy, so the two
            // can't disagree.
            WindowEvent::CursorMoved { position, .. } => {
                let l = position.to_logical::<f32>(f64::from(w.scale_factor()));
                cursor_pos = slint::LogicalPosition::new(l.x, l.y);
                EventResult::Propagate
            }
            WindowEvent::MouseWheel { delta, phase, .. } => {
                on_mouse_wheel(w, &weak, cursor_pos, *delta, *phase)
            }
            // The filter runs ahead of Slint's own dispatch, so the last `Resized` has already
            // landed its size and `draw()` has not run yet: a handler firing here sees the frame
            // it is about to be painted into.
            WindowEvent::RedrawRequested => {
                if let Some(ui) = weak.upgrade() {
                    crate::ui::appearance::ui_scale::sync(&ui, None);
                }
                #[cfg(target_os = "windows")]
                parked_loop::pump();
                EventResult::Propagate
            }
            _ => EventResult::Propagate,
        }
    });
}

/// A left press over a drag region: a window move, or the titlebar's double press.
fn drag_press(
    w: &slint::Window,
    weak: &slint::Weak<AppWindow>,
    double_press: &mut DoublePress,
    region: DragRegion,
    cursor_pos: slint::LogicalPosition,
) -> EventResult {
    match double_press.press(region, Instant::now(), cursor_pos) {
        PressAction::Move => start_os_drag(w, "drag_window", WinitWindow::drag_window),
        PressAction::ToggleMaximize => {
            if let Some(ui) = weak.upgrade() {
                ui.global::<WindowChrome>().invoke_toggle_maximize();
            }
        }
    }
    EventResult::PreventDefault
}

/// Mouse-4 / Mouse-5 into the nav history. With an overlay up we `Propagate` instead, so Slint
/// keeps its own input semantics — Esc and F already close NP and Queue, and the dialog backdrop
/// catches the dismissing click.
fn nav_button(weak: &slint::Weak<AppWindow>, state: &AppState, button: MouseButton) -> EventResult {
    let Some(ui) = weak.upgrade() else {
        return EventResult::Propagate;
    };
    if crate::ui::nav_history::overlay_open(&ui) {
        return EventResult::Propagate;
    }
    let going_back = matches!(button, MouseButton::Back);
    crate::ui::nav_history::replay(state, &ui, going_back);
    EventResult::PreventDefault
}

/// Popup-trigger highlight cleanup. `PopupWindow` has no `closed` callback,
/// and while one is open the dispatcher stops routing mouse events to
/// main-window items, so an `AppWindow`-level scrim `TouchArea` can't catch the
/// dismissing click. Release works because the press has already fired Slint's
/// `close-on-click-outside`; popup-internal areas that should *keep* the
/// highlight re-set `PopupHighlight.id` from their own `pointer-event(up)`,
/// which runs after this filter. The write is sentinel-gated so a random click
/// doesn't churn the property.
///
/// Synchronous rather than `upgrade_in_event_loop`: a deferred clear lands
/// after those `pointer-event(up)` handlers and wipes the re-set.
fn release_popup_highlight(weak: &slint::Weak<AppWindow>) {
    if let Some(ui) = weak.upgrade() {
        let ph = ui.global::<PopupHighlight>();
        if !ph.get_id().is_empty() {
            ph.set_id(SharedString::default());
        }
    }
}

fn on_resized(w: &slint::Window, weak: &slint::Weak<AppWindow>) -> EventResult {
    // Into the live mirror while the winit window is still alive: shutdown
    // reads the mirror, `with_winit_window` answering `None` once
    // the event loop has returned.
    let (maximized, frame, margins) = w
        .with_winit_window(|ww| {
            let reading = geometry::WindowReading::take(ww);
            geometry::record(ww, reading);
            // One-shot, on the first (synthetic, post-map) `Resized` only.
            geometry::ensure_on_screen(ww, reading);
            (
                reading.is_maximized(),
                geometry::frame_allowance(reading),
                geometry::frame_margins(ww, reading),
            )
        })
        .unwrap_or((false, None, None));
    // Synchronous, where everything below is posted: this filter runs ahead of Slint's
    // own handling, so the size that arrives with this event is the first thing
    // `MiniPlayerSwitch` measures against the exit edge. Posted, the allowance for the
    // frame it has just dropped lands a beat after the decision that needed it, and the
    // miniplayer swaps straight back out of the size the drop grew it to.
    if let Some(frame) = frame
        && let Some(ui) = weak.upgrade()
    {
        let chrome = ui.global::<netodia_ui::WindowChrome>();
        let ratio = os_to_ui_ratio(w);
        chrome.set_frame_allowance_w(frame.width * ratio);
        chrome.set_frame_allowance_h(frame.height * ratio);
    }
    let _ = weak.upgrade_in_event_loop(move |ui| {
        let chrome = ui.global::<netodia_ui::WindowChrome>();
        chrome.set_is_maximized(maximized);
        if let Some(margins) = margins {
            let ratio = os_to_ui_ratio(ui.window());
            chrome.set_frame_margin_left(margins.left * ratio);
            chrome.set_frame_margin_right(margins.right * ratio);
            chrome.set_frame_margin_bottom(margins.bottom * ratio);
        }
        // The cover tiers size themselves against the window, so this is the edge
        // that re-derives them — see the handler in `boot::ui_setup`.
        chrome.invoke_display_changed();
    });
    EventResult::Propagate
}

/// Frame measurements use OS logical pixels; Slint layout includes the user's UI zoom.
#[allow(clippy::cast_possible_truncation, reason = "monitor DPI fits in f32")]
fn os_to_ui_ratio(window: &slint::Window) -> f32 {
    window.with_winit_window(|w| w.scale_factor() as f32 / window.scale_factor()).unwrap_or(1.0)
}

fn on_focus(weak: &slint::Weak<AppWindow>, focused: bool) -> EventResult {
    // `Focused(false)` alone is ambiguous, so ask the OS which — but only
    // while we still believe the window is up: a tray hide and our own
    // minimize button both lower the shadow first, and on X11 the answer
    // costs a round-trip on the UI thread.
    if !focused && crate::ui::shell::tray_bridge::is_window_visible() {
        schedule_minimize_probe(weak.clone());
    }
    let _ = weak.upgrade_in_event_loop(move |ui| {
        ui.global::<Theme>().set_window_focused(focused);
        // Focus implies the surface is visible, covering the taskbar-restore
        // path: the WM un-minimizes without going through any of our
        // callbacks, so the shadow would stay stuck `false`.
        if focused {
            crate::ui::shell::tray_bridge::set_window_visible(&ui, true);
            // The theme first: its repaint moves the palette the border's neutral is
            // mixed from.
            #[cfg(target_os = "windows")]
            {
                ui.global::<netodia_ui::WindowChrome>().invoke_recheck_system_theme();
                crate::ui::appearance::window_border::refresh_system_color(&ui);
            }
        }
    });
    EventResult::Propagate
}

/// The coalescer batches multi-file drops and re-checks both gates at flush
/// time. Wayland delivery depends on the vendored winit patch.
fn on_dropped_file(
    state: &AppState,
    weak: &slint::Weak<AppWindow>,
    path: std::path::PathBuf,
) -> EventResult {
    drop_coalescer::schedule_drop_flush(state, path);
    // Both banners, whichever gate accepted — otherwise one sticks until the
    // next pointer event.
    let _ = weak.upgrade_in_event_loop(|ui| clear_drop_hover(&ui));
    EventResult::Propagate
}

/// OS-level file hover, driving the queue sheet's banner or Playlist Detail's.
/// Queue takes precedence, and `drop_coalescer`'s flush-side routing applies
/// the same one, so the indicator always names where the drop will land.
fn on_hovered_file(weak: &slint::Weak<AppWindow>) -> EventResult {
    let queue_open = drop_coalescer::is_queue_sheet_open();
    let _ = weak.upgrade_in_event_loop(move |ui| {
        ui.global::<Queue>().set_is_drop_hovered(true);
        // Only when the queue sheet isn't the active drop target.
        ui.global::<PlaylistDetail>().set_is_drop_hovered(!queue_open);
    });
    EventResult::Propagate
}

/// The OS / WM close path — the custom-titlebar button routes through
/// `WindowChrome.on_close_window` instead. Always `PreventDefault` and act
/// explicitly: the loop runs under `run_event_loop_until_quit`, where
/// `CloseRequested` no longer auto-quits. The close-to-tray hide is deferred,
/// running it inside a `WindowEvent` dispatch tripping Slint's "references to
/// the window still exist" warning; `should_hide_to_tray` is false with no
/// tray active, so this can't strand the user.
fn on_close_requested(weak: &slint::Weak<AppWindow>) -> EventResult {
    if crate::ui::shell::tray_bridge::should_hide_to_tray() {
        if let Err(e) = weak.upgrade_in_event_loop(|ui| {
            crate::ui::shell::tray_bridge::hide_window(&ui);
        }) {
            log::warn!("close-to-tray: schedule hide: {}", describe(&e));
        }
    } else if let Err(e) = slint::quit_event_loop() {
        log::warn!("close-window: quit_event_loop: {}", describe(&e));
    }
    EventResult::PreventDefault
}

/// Routing argued at [`route_wheel`]; the delta conversion mirrors the Slint
/// winit backend exactly.
fn on_mouse_wheel(
    w: &slint::Window,
    weak: &slint::Weak<AppWindow>,
    cursor_pos: slint::LogicalPosition,
    delta: MouseScrollDelta,
    phase: TouchPhase,
) -> EventResult {
    let (dx, dy) = match delta {
        MouseScrollDelta::LineDelta(lx, ly) => (lx * 60.0, ly * 60.0),
        MouseScrollDelta::PixelDelta(p) => {
            let l = p.to_logical::<f32>(f64::from(w.scale_factor()));
            (l.x, l.y)
        }
    };
    let Some(ui) = weak.upgrade() else {
        return EventResult::Propagate;
    };
    let cs = ui.global::<CompositeScroll>();
    // `hovered` can be stale-true where an overlay opened over a composite
    // region with no pointer move, Slint clearing `has-hover` only on the
    // next `MouseEvent::Exit`. Mirrors the Mouse-4/5 arm's own gate.
    let overlay_open = crate::ui::nav_history::overlay_open(&ui);
    match route_wheel(cs.get_hovered(), overlay_open, phase, dx, dy) {
        WheelRoute::Composite => {
            scroll_composite(&ui, dy);
            EventResult::PreventDefault
        }
        // `PointerScrolled` lands as a one-shot `TouchPhase::Cancelled`
        // wheel, the direction-aware arm. Re-sent rather than dropped so
        // the shortest gesture — one event and a stop — still moves.
        WheelRoute::Unphased => {
            let scrolled = slint::platform::WindowEvent::PointerScrolled {
                position: cursor_pos,
                delta_x: dx,
                delta_y: dy,
            };
            if let Err(e) = ui.window().try_dispatch_event(scrolled) {
                log::warn!("touchpad scroll: dispatch: {}", describe(&e));
            }
            EventResult::PreventDefault
        }
        WheelRoute::Native => EventResult::Propagate,
    }
}

fn scroll_composite(ui: &AppWindow, dy: f32) {
    let cs = ui.global::<CompositeScroll>();
    // Several deltas can arrive before the view's changed handler consumes the sum.
    cs.set_wheel_dy(cs.get_wheel_dy() + dy);
    cs.set_wheel_tick(cs.get_wheel_tick().wrapping_add(1));
    // Neither property paints anything, so the changed handler needs an explicit frame.
    ui.window().request_redraw();
}

#[cfg(test)]
#[path = "tests/winit_filter_tests.rs"]
mod tests;
