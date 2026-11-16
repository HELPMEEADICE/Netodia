//! Deterministic input checks against the real shared gesture, scroller and queue row.

use std::cell::Cell;
use std::error::Error;
use std::rc::Rc;
use std::time::Duration;

use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType};
use slint::platform::{Platform, WindowAdapter, WindowEvent};
use slint::{ComponentHandle, LogicalPosition, PhysicalSize, PlatformError, Rgb8Pixel};

#[allow(warnings, unsafe_code, clippy::all, clippy::unwrap_used)]
mod fixture {
    slint::slint! {
        import { TouchControlsWindow, InputMode, Queue } from "touch-controls.slint";
        export { TouchControlsWindow, InputMode, Queue }
    }
}

use fixture::{InputMode, Queue, TouchControlsWindow};

struct TestPlatform {
    window: Rc<MinimalSoftwareWindow>,
    elapsed: Rc<Cell<Duration>>,
}

impl Platform for TestPlatform {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, PlatformError> {
        Ok(self.window.clone())
    }

    fn duration_since_start(&self) -> Duration {
        self.elapsed.get()
    }
}

fn advance(elapsed: &Cell<Duration>, milliseconds: u64) {
    elapsed.set(elapsed.get() + Duration::from_millis(milliseconds));
    slint::platform::update_timers_and_animations();
}

fn press(window: &slint::Window, x: f32, y: f32) {
    window.dispatch_event(WindowEvent::PointerPressed {
        position: LogicalPosition::new(x, y),
        button: slint::platform::PointerEventButton::Left,
    });
}

fn release(window: &slint::Window, x: f32, y: f32) {
    window.dispatch_event(WindowEvent::PointerReleased {
        position: LogicalPosition::new(x, y),
        button: slint::platform::PointerEventButton::Left,
    });
}

fn move_to(window: &slint::Window, x: f32, y: f32) {
    window.dispatch_event(WindowEvent::PointerMoved { position: LogicalPosition::new(x, y) });
}

#[test]
fn touch_taps_holds_and_pans_do_not_become_each_other() -> Result<(), Box<dyn Error>> {
    let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    let elapsed = Rc::new(Cell::new(Duration::ZERO));
    slint::platform::set_platform(Box::new(TestPlatform {
        window: window.clone(),
        elapsed: elapsed.clone(),
    }))?;
    let ui = TouchControlsWindow::new()?;
    window.set_size(PhysicalSize::new(400, 300));
    ui.show()?;
    ui.global::<InputMode>().set_touch_active(true);
    let mut pixels = vec![Rgb8Pixel::default(); 400 * 300];
    window.draw_if_needed(|renderer| renderer.render(&mut pixels, 400));

    press(ui.window(), 40.0, 40.0);
    advance(&elapsed, 599);
    assert_eq!(ui.get_menus(), 0);
    release(ui.window(), 40.0, 40.0);
    advance(&elapsed, 601);
    assert_eq!(ui.get_clicks(), 1, "a tap must click once and disarm the hold");
    assert_eq!(ui.get_menus(), 0);

    press(ui.window(), 40.0, 40.0);
    advance(&elapsed, 600);
    assert_eq!(ui.get_menus(), 1, "the hold fires at its boundary");
    advance(&elapsed, 1200);
    release(ui.window(), 40.0, 40.0);
    assert_eq!(ui.get_menus(), 1, "a held finger must open only one menu");
    assert_eq!(ui.get_clicks(), 1, "releasing a hold must not activate the row");

    for (dx, dy) in [(9.0, 0.0), (-9.0, 0.0), (0.0, 9.0), (0.0, -9.0)] {
        press(ui.window(), 40.0, 40.0);
        move_to(ui.window(), 40.0 + dx, 40.0 + dy);
        advance(&elapsed, 600);
        release(ui.window(), 40.0 + dx, 40.0 + dy);
    }
    assert_eq!(ui.get_menus(), 1, "motion on either axis must disarm the hold");
    assert_eq!(ui.get_clicks(), 1, "a swipe over non-scrolling content must not click");

    press(ui.window(), 40.0, 40.0);
    ui.window().dispatch_event(WindowEvent::PointerExited);
    advance(&elapsed, 600);
    release(ui.window(), 40.0, 40.0);
    assert_eq!(ui.get_menus(), 1, "a cancelled contact must disarm the hold");
    assert_eq!(ui.get_clicks(), 1);

    ui.global::<InputMode>().set_touch_active(false);
    press(ui.window(), 40.0, 40.0);
    advance(&elapsed, 800);
    release(ui.window(), 40.0, 40.0);
    assert_eq!(ui.get_menus(), 1, "mouse holds must keep their existing semantics");
    assert_eq!(ui.get_clicks(), 2);

    ui.global::<InputMode>().set_touch_active(true);
    let plays = Rc::new(Cell::new(0));
    let played = plays.clone();
    ui.global::<Queue>().on_skip_to(move |_| played.set(played.get() + 1));
    ui.global::<Queue>().set_drag_source(-1);
    press(ui.window(), 290.0, 24.0);
    advance(&elapsed, 120);
    release(ui.window(), 290.0, 24.0);
    assert_eq!(plays.get(), 1, "one touch tap must play a queue row once");

    press(ui.window(), 290.0, 155.0);
    advance(&elapsed, 120);
    move_to(ui.window(), 290.0, 110.0);
    advance(&elapsed, 16);
    move_to(ui.window(), 290.0, 65.0);
    advance(&elapsed, 16);
    release(ui.window(), 290.0, 65.0);
    assert!(ui.get_offset() > 20.0, "the finger must scroll a real ListView");
    assert_eq!(plays.get(), 1, "a swipe must not play its starting or ending row");
    assert_eq!(ui.global::<Queue>().get_drag_source(), -1, "a finger must not arm reordering");

    ui.hide()?;
    Ok(())
}
