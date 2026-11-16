use super::*;
use netodia_testkit::{block_after, strip_line_comments};

// Every mouse wheel on every platform, and a touchpad on X11 and Win32.
#[test]
fn a_wheel_is_left_to_slint() {
    assert_eq!(route_wheel(false, false, TouchPhase::Moved, 0.0, -60.0), WheelRoute::Native);
    assert_eq!(route_wheel(false, false, TouchPhase::Ended, 0.0, 0.0), WheelRoute::Native);
}

// Only the phase decides. Gating on the delta too would leave the horizontal
// half, a card strip under Search's vertical outer scroller, captured and
// ignored exactly as before.
#[test]
fn a_touchpad_gesture_start_is_unphased_on_both_axes() {
    assert_eq!(route_wheel(false, false, TouchPhase::Started, 0.0, -8.0), WheelRoute::Unphased);
    assert_eq!(route_wheel(false, false, TouchPhase::Started, -12.0, 0.0), WheelRoute::Unphased);
}

// A composite view drives its own math off the raw delta and wants the gesture
// whole, phases and all.
#[test]
fn a_composite_region_still_takes_the_gesture_start() {
    assert_eq!(route_wheel(true, false, TouchPhase::Started, 0.0, -8.0), WheelRoute::Composite);
    assert_eq!(route_wheel(true, false, TouchPhase::Moved, 0.0, -60.0), WheelRoute::Composite);
}

// The overlay has scrollers of its own, nested the same way, so a released
// gesture still owes normalizing.
#[test]
fn an_overlay_releases_the_composite_arm_without_releasing_the_gesture() {
    assert_eq!(route_wheel(true, true, TouchPhase::Started, 0.0, -8.0), WheelRoute::Unphased);
    assert_eq!(route_wheel(true, true, TouchPhase::Moved, 0.0, -60.0), WheelRoute::Native);
}

#[test]
fn a_horizontal_wheel_is_not_composite() {
    assert_eq!(route_wheel(true, false, TouchPhase::Moved, -60.0, 0.0), WheelRoute::Native);
    assert_eq!(route_wheel(true, false, TouchPhase::Moved, -60.0, 20.0), WheelRoute::Native);
}

/// The filter's own source, comments stripped — what the arm walks below read.
fn filter_source() -> String {
    strip_line_comments(include_str!("../winit_filter.rs"))
}

/// The guard the resize press arm is found by.
const RESIZE_ARM: &str = "if let Some(direction) = resize.get() =>";

/// The drag and resize cells move in separate `changed` handlers, and the miniplayer's drag region
/// runs right up to the edge, so the drag cell can still read true on a press that lands on a grab.
/// Matched first, that press moves the window the user meant to resize.
#[test]
fn the_resize_press_arm_is_matched_before_the_drag_arm() -> Result<(), Box<dyn std::error::Error>> {
    let code = filter_source();
    let (before_resize, _) =
        code.split_once(RESIZE_ARM).ok_or("no resize press arm found: the walk is broken")?;

    let drag_arms_before = before_resize.matches("drag_region.get()").count();

    assert_eq!(drag_arms_before, 0, "the drag arm is matched ahead of the resize arm");
    Ok(())
}

#[test]
fn the_resize_press_arm_starts_the_os_resize() {
    let code = filter_source();

    let arm = block_after(&code, RESIZE_ARM);

    assert!(
        arm.contains("drag_resize_window(direction)"),
        "the resize press arm no longer starts a resize:\n{arm}"
    );
}

/// Propagated, the press reaches the ring's `TouchArea` under the pointer, which grabs the mouse
/// while the OS takes the pointer for the resize, and the release never comes back to free it.
#[test]
fn the_resize_press_arm_keeps_the_press_from_slint() {
    let code = filter_source();

    let arm = block_after(&code, RESIZE_ARM);

    assert!(
        arm.contains("EventResult::PreventDefault") && !arm.contains("EventResult::Propagate"),
        "the resize press arm hands its press on to Slint:\n{arm}"
    );
}

/// Every cover tier sizes its capacity and its decode size against the window, and both are read
/// once after `app.show()`. This arm is the only thing that re-derives them, so without the
/// signal a window maximized after launch mounts more cards than its tier can hold and the
/// overflow paints placeholders until something else moves the visible set. A source walk
/// because the handler at the other end needs a live window and all seven view handles.
#[test]
fn the_resize_arm_retunes_the_cover_tiers() {
    const ARM: &str = "fn on_resized(";
    let code = filter_source();
    let arm = block_after(&code, ARM);

    assert!(!arm.is_empty(), "no `{ARM}` block found — the walk is broken, not the code");
    assert!(
        arm.contains("invoke_display_changed()"),
        "a resize moves every answer the cover tiers derive from the window, so this arm owes \
         the signal that re-derives them:\n{arm}"
    );
}

/// Win32 parks winit's loop in a modal message loop for a whole resize drag, and `NewEvents` —
/// the only place the backend ticks Slint — fires once, at the top of it. Without the pump every
/// `changed` handler and `Timer` waits for the button to come up, which on a shrink past the
/// miniplayer threshold is the swap not happening. A source walk because the Windows jobs compile
/// this arm but nothing can run it, and compiling never proves it ticks anything.
#[test]
fn the_redraw_arm_ticks_the_loop_win32_parked() {
    const ARM: &str = "WindowEvent::RedrawRequested =>";
    let code = filter_source();
    let arm = block_after(&code, ARM);

    assert!(!arm.is_empty(), "no `{ARM}` block found — the walk is broken, not the code");
    assert!(
        arm.contains("parked_loop::pump()"),
        "a paint is what the Win32 modal loop still delivers, so this arm is where the tick it \
         parked winit out of gets run:\n{arm}"
    );
    assert!(
        include_str!("../parked_loop.rs").contains("pub fn pump()"),
        "the arm calls a pump that no longer exists"
    );
}

/// The redraw arm's sibling, and not redundant with it: a **move** drag resizes nothing, so the
/// client area is never invalidated and no `WM_PAINT` ever starts the chain the other arm rides.
/// `Moved` is the only thing the modal loop delivers for the whole of one, which makes this the
/// only tick a window dragged across a monitor edge gets. Pinned separately because deleting it
/// costs nothing the redraw walk can see.
#[test]
fn the_move_arm_ticks_the_loop_win32_parked() {
    const ARM: &str = "WindowEvent::Moved(_) =>";
    let code = filter_source();
    let arm = block_after(&code, ARM);

    assert!(!arm.is_empty(), "no `{ARM}` block found — the walk is broken, not the code");
    assert!(
        arm.contains("parked_loop::pump()"),
        "a move drag invalidates nothing, so this arm is the only tick it gets:\n{arm}"
    );
}

/// The frame reading has to outlive the frame it measured. `frame_allowance` answers `None` on
/// every reading but the one that crossed the decoration change, and an arm writing a default there
/// hands `MiniPlayerSwitch` a zero allowance while the client area is still grown by the real one,
/// so a window parked just inside the threshold bounces straight back out.
#[test]
fn the_resize_arm_writes_the_frame_only_when_one_was_measured() {
    const ARM: &str = "fn on_resized(";
    const GATE: &str = "if let Some(frame) = frame";
    let code = filter_source();
    let arm = block_after(&code, ARM);
    let gated = block_after(arm, GATE);

    assert!(!arm.is_empty(), "no `{ARM}` block found: the walk is broken, not the code");
    assert!(
        gated.contains("set_frame_allowance_w(") && gated.contains("set_frame_allowance_h("),
        "the frame reading is written outside its `Some` gate, so a frameless resize can zero \
         it:\n{arm}"
    );
    assert_eq!(
        arm.matches("set_frame_allowance_").count(),
        2,
        "the arm writes the frame reading somewhere besides the gated pair:\n{arm}"
    );
}

/// The allowance is the edge the size arriving with this very event is measured against, and this
/// filter runs ahead of Slint's own handling, so the write has to be synchronous. Posted, it lands
/// a beat after the decision that needed it: the miniplayer drops the frame, the client grows by it,
/// and the switch leaves against the edge it had before the drop.
#[test]
fn the_resize_arm_writes_the_frame_ahead_of_slint() {
    const ARM: &str = "fn on_resized(";
    const POSTED: &str = "weak.upgrade_in_event_loop(move |ui|";
    const SETTER: &str = "set_frame_allowance_w(";
    let code = filter_source();
    let arm = block_after(&code, ARM);
    let posted = block_after(arm, POSTED);

    assert!(!posted.is_empty(), "no `{POSTED}` block found: the walk is broken, not the code");
    assert!(
        !posted.contains("set_frame_allowance_"),
        "the frame reading is written from the posted closure, so it reaches the exit edge after \
         the size it is the edge for:\n{arm}"
    );
    // Absence from the posted block is not enough on its own: a hop of its own, ahead of it, reads
    // as synchronous and lands just as late.
    assert!(
        matches!((arm.find(SETTER), arm.find(POSTED)), (Some(write), Some(hop)) if write < hop),
        "the frame reading is no longer written before the arm posts anything:\n{arm}"
    );
}

/// The margins have the allowance's reason to outlive their frame, with a worse failure: zeroed by
/// a frameless resize, the native miniplayer paints into the invisible borders again and the
/// window grows by them at its next layout.
#[test]
fn the_resize_arm_writes_the_margins_only_when_they_were_measured() {
    const ARM: &str = "fn on_resized(";
    const GATE: &str = "if let Some(margins) = margins";
    const SETTERS: [&str; 3] =
        ["set_frame_margin_left(", "set_frame_margin_right(", "set_frame_margin_bottom("];
    let code = filter_source();
    let arm = block_after(&code, ARM);
    let gated = block_after(arm, GATE);

    let outside: Vec<&str> = SETTERS.into_iter().filter(|s| !gated.contains(s)).collect();

    assert!(!arm.is_empty(), "no `{ARM}` block found: the walk is broken, not the code");
    assert!(
        outside.is_empty() && arm.matches("set_frame_margin_").count() == SETTERS.len(),
        "the frame margins are written outside their `Some` gate: {outside:?}\n{arm}"
    );
}

/// Nothing paints the two properties the arm writes, so without the request no frame is
/// scheduled and the `changed` handler that applies them waits for an unrelated event —
/// every notch landing one notch late (#64). A source walk because scheduling is what a
/// unit test can't reach; `route_wheel` beside it is the half that can.
#[test]
fn the_composite_wheel_arm_asks_for_a_frame() {
    const ARM: &str = "WheelRoute::Composite =>";
    let code = filter_source();
    let arm = block_after(&code, ARM);

    assert!(!arm.is_empty(), "no `{ARM}` block found — the walk is broken, not the code");
    assert!(
        arm.contains("scroll_composite(&ui, dy)")
            && block_after(&code, "fn scroll_composite(").contains("request_redraw()"),
        "the composite arm writes only properties nothing paints, so it must ask for the \
         frame its `changed` handler runs on:\n{arm}"
    );
}
