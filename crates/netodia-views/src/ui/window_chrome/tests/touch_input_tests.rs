use super::{CompositePan, PanStep};
use slint::LogicalPosition;
use slint::winit_030::winit::event::DeviceId;

fn pan() -> CompositePan {
    CompositePan::new((DeviceId::dummy(), 1), LogicalPosition::new(50.0, 50.0))
}

#[test]
fn composite_pan_waits_for_motion_past_the_slop_on_either_axis() {
    for (dx, dy) in [(0.0, 0.0), (8.0, 0.0), (-8.0, 8.0), (0.0, -8.0)] {
        assert_eq!(pan().moved(LogicalPosition::new(50.0 + dx, 50.0 + dy)), PanStep::Pending);
    }
}

#[test]
fn a_horizontal_pan_stays_with_the_native_card_strip() {
    for (dx, dy) in [(9.0, 0.0), (-9.0, 8.0), (10.0, -9.0)] {
        assert_eq!(pan().moved(LogicalPosition::new(50.0 + dx, 50.0 + dy)), PanStep::Horizontal);
    }
}

#[test]
fn a_vertical_pan_keeps_the_first_delta_and_can_reverse() {
    let mut pan = pan();
    assert_eq!(pan.moved(LogicalPosition::new(50.0, 46.0)), PanStep::Pending);
    for (x, y, expected) in [(50.0, 41.0, -9.0), (52.0, 30.0, -11.0), (52.0, 35.0, 5.0)] {
        let step = pan.moved(LogicalPosition::new(x, y));
        assert!(
            matches!(step, PanStep::Vertical(dy) if (dy - expected).abs() < f32::EPSILON),
            "expected vertical delta {expected}, got {step:?}"
        );
    }
}
