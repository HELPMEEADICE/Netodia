use super::{Glyph, render};

const INK: [u8; 3] = [0x20, 0x40, 0x60];

fn alpha_at(pixels: &[u8], size: u32, x: u32, y: u32) -> u8 {
    pixels[((y * size + x) * 4 + 3) as usize]
}

#[test]
fn every_glyph_fills_a_square_of_bgra_pixels() {
    for glyph in [Glyph::Previous, Glyph::Play, Glyph::Pause, Glyph::Next] {
        assert_eq!(render(glyph, 20, INK).len(), 20 * 20 * 4, "{glyph:?}");
    }
}

#[test]
fn the_ink_is_written_in_bgr_order_on_every_pixel() {
    let pixels = render(Glyph::Play, 16, INK);

    assert!(pixels.chunks_exact(4).all(|px| px[..3] == [0x60, 0x40, 0x20]));
}

#[test]
fn a_glyph_is_solid_inside_and_clear_outside() {
    // 24 px puts one pixel per grid unit, so the play triangle's interior and the empty corner
    // both land on a pixel centre.
    let pixels = render(Glyph::Play, 24, INK);

    assert_eq!(alpha_at(&pixels, 24, 11, 11), 255);
    assert_eq!(alpha_at(&pixels, 24, 1, 1), 0);
}

#[test]
fn the_edges_are_antialiased_rather_than_stepped() {
    let pixels = render(Glyph::Play, 16, INK);

    assert!(pixels.chunks_exact(4).any(|px| px[3] > 0 && px[3] < 255));
}

#[test]
fn rounding_leaves_the_corners_of_a_bar_softer_than_its_middle() {
    // The pause bars span x 6..10, y 5..19: the top-left pixel sits on a rounded corner, the one
    // below it on the straight left edge.
    let pixels = render(Glyph::Pause, 24, INK);

    assert!(alpha_at(&pixels, 24, 6, 5) < alpha_at(&pixels, 24, 6, 11));
}

/// Within a step of rounding: the two outlines are mirrored on paper, and the float path through
/// the inset need not land on the same last bit for both.
#[test]
fn next_mirrors_previous() {
    let size = 24;
    let previous = render(Glyph::Previous, size, INK);
    let next = render(Glyph::Next, size, INK);

    let mirrored = (0..size)
        .flat_map(|y| (0..size).map(move |x| (y, size - 1 - x)))
        .map(|(y, x)| alpha_at(&previous, size, x, y));
    let alphas = next.chunks_exact(4).map(|px| px[3]);
    assert!(alphas.zip(mirrored).all(|(a, b)| a.abs_diff(b) <= 1));
}
