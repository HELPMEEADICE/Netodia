//! Touch mode and the chrome handoff. Widget gestures stay in Slint so panning keeps its fling.

use std::cell::Cell;
use std::rc::Rc;

use slint::ComponentHandle;
use slint::winit_030::EventResult;
use slint::winit_030::winit::event::{DeviceId, ElementState, Touch, TouchPhase, WindowEvent};
use slint::winit_030::winit::window::ResizeDirection;

use netodia_core::error::describe;
use netodia_ui::{AppWindow, CompositeScroll, DragRegion, InputMode};

use super::super::drag_region::DoublePress;
use super::{PressTargets, drag_press, release_popup_highlight, scroll_composite, start_os_drag};

pub(super) struct TouchInput {
    chrome_contact: Option<(DeviceId, u64)>,
    drag_region: Rc<Cell<DragRegion>>,
    resize: Rc<Cell<Option<ResizeDirection>>>,
    composite_pan: Option<CompositePan>,
}

impl TouchInput {
    pub(super) fn new(targets: &PressTargets) -> Self {
        Self {
            chrome_contact: None,
            drag_region: targets.drag_region.clone(),
            resize: targets.resize.clone(),
            composite_pan: None,
        }
    }

    pub(super) fn handle(
        &mut self,
        window: &slint::Window,
        weak: &slint::Weak<AppWindow>,
        touch: &Touch,
        double_press: &mut DoublePress,
    ) -> EventResult {
        let contact = (touch.device_id, touch.id);
        if touch.phase == TouchPhase::Started {
            self.chrome_contact = None;
            self.composite_pan = None;
            let p = touch.location.to_logical::<f32>(f64::from(window.scale_factor()));
            let position = slint::LogicalPosition::new(p.x, p.y);
            // Fingers have no preceding hover; the cells still describe the last mouse position.
            if let Err(e) =
                window.try_dispatch_event(slint::platform::WindowEvent::PointerMoved { position })
            {
                log::warn!("touch hover: dispatch: {}", describe(&e));
            } else if let Some(direction) = self.resize.get() {
                self.chrome_contact = Some(contact);
                start_os_drag(window, "touch resize", |ww| ww.drag_resize_window(direction));
                return EventResult::PreventDefault;
            } else if self.drag_region.get() != DragRegion::None {
                self.chrome_contact = Some(contact);
                return drag_press(window, weak, double_press, self.drag_region.get(), position);
            }
            if let Some(ui) = weak.upgrade()
                && ui.global::<CompositeScroll>().get_hovered()
                && !crate::ui::nav_history::overlay_open(&ui)
            {
                self.composite_pan = Some(CompositePan::new(contact, position));
            }
        }
        if self.chrome_contact == Some(contact) {
            if matches!(touch.phase, TouchPhase::Ended | TouchPhase::Cancelled) {
                self.chrome_contact = None;
            }
            return EventResult::PreventDefault;
        }
        if touch.phase == TouchPhase::Moved {
            return self.move_composite(window, weak, touch);
        }
        if matches!(touch.phase, TouchPhase::Ended | TouchPhase::Cancelled) {
            self.composite_pan = None;
            release_popup_highlight(weak);
        }
        EventResult::Propagate
    }

    fn move_composite(
        &mut self,
        window: &slint::Window,
        weak: &slint::Weak<AppWindow>,
        touch: &Touch,
    ) -> EventResult {
        let Some(ui) = weak.upgrade() else { return EventResult::Propagate };
        if ui.global::<InputMode>().get_control_pressed()
            || crate::ui::nav_history::overlay_open(&ui)
        {
            self.composite_pan = None;
        }
        let Some(pan) = &mut self.composite_pan else { return EventResult::Propagate };
        if pan.contact != (touch.device_id, touch.id) {
            return EventResult::PreventDefault;
        }
        let p = touch.location.to_logical::<f32>(f64::from(window.scale_factor()));
        let was_dragging = pan.dragging;
        match pan.moved(slint::LogicalPosition::new(p.x, p.y)) {
            PanStep::Pending => EventResult::Propagate,
            PanStep::Horizontal => {
                self.composite_pan = None;
                EventResult::Propagate
            }
            PanStep::Vertical(dy) => {
                // Cancel the row's press before scrolling destroys or moves it. The native
                // Touch Ended still propagates to clear Slint's contact bookkeeping.
                if !was_dragging {
                    window.dispatch_event(slint::platform::WindowEvent::PointerExited);
                }
                scroll_composite(&ui, dy);
                EventResult::PreventDefault
            }
        }
    }
}

// Nested vertical scrollers otherwise let the outer Flickable take every finger drag, even at
// its end. Drive their existing combined scrollbar math; horizontal strips keep native panning.
struct CompositePan {
    contact: (DeviceId, u64),
    origin: slint::LogicalPosition,
    previous: slint::LogicalPosition,
    dragging: bool,
}

#[derive(Debug, PartialEq)]
enum PanStep {
    Pending,
    Horizontal,
    Vertical(f32),
}

impl CompositePan {
    fn new(contact: (DeviceId, u64), origin: slint::LogicalPosition) -> Self {
        Self { contact, origin, previous: origin, dragging: false }
    }

    fn moved(&mut self, position: slint::LogicalPosition) -> PanStep {
        const SLOP: f32 = 8.0;
        if !self.dragging {
            let dx = (position.x - self.origin.x).abs();
            let dy = (position.y - self.origin.y).abs();
            if dx.max(dy) <= SLOP {
                return PanStep::Pending;
            }
            if dx > dy {
                return PanStep::Horizontal;
            }
            self.dragging = true;
        }
        let dy = position.y - self.previous.y;
        self.previous = position;
        PanStep::Vertical(dy)
    }
}

/// Runs before Slint's press dispatch, where the styled scrollers decide whether they can pan.
pub(super) fn update_mode(weak: &slint::Weak<AppWindow>, event: &WindowEvent) {
    let Some(ui) = weak.upgrade() else { return };
    let mode = ui.global::<InputMode>();
    match event {
        WindowEvent::Touch(touch) => {
            mode.set_touch_active(true);
            if matches!(touch.phase, TouchPhase::Started | TouchPhase::Cancelled) {
                mode.set_control_pressed(false);
            }
        }
        WindowEvent::MouseInput { state: ElementState::Pressed, .. }
        | WindowEvent::MouseWheel { .. } => {
            mode.set_touch_active(false);
            mode.set_control_pressed(false);
        }
        _ => {}
    }
}

#[cfg(test)]
#[path = "../tests/touch_input_tests.rs"]
mod tests;
