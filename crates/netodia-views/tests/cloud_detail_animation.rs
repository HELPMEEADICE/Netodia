//! Exercise the compiled overlay with a deterministic clock and real rendered frames. In
//! particular, a stopped timer in conditional content used to leave the second visit compact.

use std::cell::Cell;
use std::collections::hash_map::DefaultHasher;
use std::error::Error;
use std::hash::{Hash, Hasher};
use std::rc::Rc;
use std::time::Duration;

use netodia_ui::{AppWindow, CloudEntityDetail, Nav};
use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType};
use slint::platform::{Platform, WindowAdapter};
use slint::{ComponentHandle, PhysicalSize, PlatformError, Rgb8Pixel, SharedPixelBuffer};

const WIDTH: u32 = 1000;
const HEIGHT: u32 = 700;

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

struct Frames {
    window: Rc<MinimalSoftwareWindow>,
    elapsed: Rc<Cell<Duration>>,
    pixels: SharedPixelBuffer<Rgb8Pixel>,
}

impl Frames {
    fn advance(&mut self, milliseconds: u64) {
        self.elapsed.set(self.elapsed.get() + Duration::from_millis(milliseconds));
        slint::platform::update_timers_and_animations();
        self.window.request_redraw();
        self.window.draw_if_needed(|renderer| {
            renderer.render(self.pixels.make_mut_slice(), WIDTH as usize);
        });
    }

    fn settle(&mut self) {
        for _ in 0..40 {
            self.advance(16);
        }
    }

    fn header(&self) -> u64 {
        // Compare the content panel's upper band, excluding the sidebar, playback bar and
        // arriving list. No reference image or host-specific font rasterization is assumed.
        let mut hash = DefaultHasher::new();
        for row in self.pixels.as_bytes().chunks_exact(WIDTH as usize * 3).skip(40).take(200) {
            row[200 * 3..].hash(&mut hash);
        }
        hash.finish()
    }
}

#[test]
fn cloud_detail_reopens_and_reverses_without_losing_its_header() -> Result<(), Box<dyn Error>> {
    let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    let elapsed = Rc::new(Cell::new(Duration::ZERO));
    slint::platform::set_platform(Box::new(TestPlatform {
        window: window.clone(),
        elapsed: elapsed.clone(),
    }))?;
    let ui = AppWindow::new()?;
    ui.global::<Nav>().set_selected_index(2);
    window.set_size(PhysicalSize::new(WIDTH, HEIGHT));
    ui.show()?;
    let mut frames = Frames { window, elapsed, pixels: SharedPixelBuffer::new(WIDTH, HEIGHT) };
    frames.settle();

    let detail = ui.global::<CloudEntityDetail>();
    let closed = Rc::new(Cell::new(0));
    let closed_callback = closed.clone();
    detail.on_closed(move || closed_callback.set(closed_callback.get() + 1));
    detail.set_kind("artist".into());
    detail.set_remote_id("1234567890123".into());
    detail.set_title("Animation regression artist".into());
    detail.set_loading(true);
    detail.set_open(true);
    frames.advance(0);
    let compact = frames.header();
    frames.advance(16);
    frames.advance(96);
    let entering = frames.header();
    detail.set_loading(false);
    frames.settle();
    let expanded = frames.header();
    assert_ne!(expanded, compact, "the first visit must expand beyond the compact tab band");
    assert_ne!(entering, compact, "the hero must move during its entrance");
    assert_ne!(entering, expanded, "the hero must ease in rather than appear at full height");

    detail.set_open(false);
    frames.settle();
    assert_eq!(closed.get(), 1, "a completed collapse must hand the source page back once");

    detail.set_loading(true);
    detail.set_open(true);
    frames.advance(0);
    frames.advance(16);
    frames.advance(96);
    let reentering = frames.header();
    detail.set_loading(false);
    frames.settle();
    assert_eq!(frames.header(), expanded, "reopening the same artist must restore the full hero");
    assert_ne!(reentering, expanded, "the second visit must animate too");

    detail.set_open(false);
    frames.advance(0);
    frames.advance(96);
    detail.set_open(true);
    frames.settle();
    assert_eq!(closed.get(), 1, "reopening during collapse must cancel the outgoing teardown");
    assert_eq!(frames.header(), expanded, "a reversed collapse must finish at the full hero");

    detail.set_open(false);
    frames.settle();
    detail.set_open(true);
    frames.advance(0);
    detail.set_open(false);
    frames.advance(0);
    frames.settle();
    assert_eq!(
        closed.get(),
        3,
        "Back before the opening timer fires must still close exactly once"
    );
    detail.set_open(true);
    frames.settle();
    assert_eq!(frames.header(), expanded, "a cancelled entrance must not break the next visit");
    ui.hide()?;
    Ok(())
}
