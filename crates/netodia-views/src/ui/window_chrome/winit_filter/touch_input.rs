//! Touch mode and the chrome handoff. Widget gestures stay in Slint so panning keeps its fling.

use std::cell::Cell;
use std::rc::Rc;

use slint::ComponentHandle;
use slint::winit_030::EventResult;
use slint::winit_030::winit::event::{DeviceId, ElementState, Touch, TouchPhase, WindowEvent};
use slint::winit_030::winit::window::ResizeDirection;

use netodia_core::error::describe;
use netodia_ui::{AppWindow, DragRegion, InputMode};

use super::super::drag_region::DoublePress;
use super::{PressTargets, drag_press, release_popup_highlight, start_os_drag};

pub(super) struct TouchInput {
    chrome_contact: Option<(DeviceId, u64)>,
    drag_region: Rc<Cell<DragRegion>>,
    resize: Rc<Cell<Option<ResizeDirection>>>,
}

impl TouchInput {
    pub(super) fn new(targets: &PressTargets) -> Self {
        Self {
            chrome_contact: None,
            drag_region: targets.drag_region.clone(),
            resize: targets.resize.clone(),
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
        }
        if self.chrome_contact == Some(contact) {
            if matches!(touch.phase, TouchPhase::Ended | TouchPhase::Cancelled) {
                self.chrome_contact = None;
            }
            return EventResult::PreventDefault;
        }
        if matches!(touch.phase, TouchPhase::Ended | TouchPhase::Cancelled) {
            release_popup_highlight(weak);
        }
        EventResult::Propagate
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
