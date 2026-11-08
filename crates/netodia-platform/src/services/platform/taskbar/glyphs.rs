//! The thumbnail toolbar's transport glyphs, rasterized at whatever size the shell asks for.
//!
//! The outlines sit on Material Symbols' 24-unit grid, so the taskbar reads as the same family as
//! the player bar. Coverage comes from a signed distance rather than a bitmask: the shell draws
//! these from 16 px up, where a stepped edge is most of what the eye sees.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Glyph {
    Previous,
    Play,
    Pause,
    Next,
}

const GRID: f64 = 24.0;

type Point = (f64, f64);

/// A convex outline with its corners rounded inside it.
struct Shape {
    /// The outline pulled in by `radius`, so growing it back by `radius` lands on the outline.
    core: Vec<Point>,
    radius: f64,
}

impl Shape {
    fn rounded(outline: &[Point], radius: f64) -> Self {
        Self { core: inset(outline, radius), radius }
    }

    fn distance(&self, p: Point) -> f64 {
        signed_distance(&self.core, p) - self.radius
    }
}

/// Straight-alpha BGRA, top-down, `size` × `size`, the layout a 32-bit icon's colour bitmap takes.
pub(super) fn render(glyph: Glyph, size: u32, ink: [u8; 3]) -> Vec<u8> {
    let shapes = shapes(glyph);
    let scale = f64::from(size) / GRID;
    let [red, green, blue] = ink;
    let mut pixels = Vec::with_capacity(4 * (size as usize).pow(2));
    for y in 0..size {
        for x in 0..size {
            let centre = ((f64::from(x) + 0.5) / scale, (f64::from(y) + 0.5) / scale);
            let distance =
                shapes.iter().map(|shape| shape.distance(centre)).fold(f64::INFINITY, f64::min);
            // Half a pixel either side of the edge ramps from full to none.
            let coverage = (0.5 - distance * scale).clamp(0.0, 1.0);
            pixels.extend_from_slice(&[blue, green, red, to_byte(coverage)]);
        }
    }
    pixels
}

fn shapes(glyph: Glyph) -> Vec<Shape> {
    // The bars run half a unit wider than Material's 2: at 16 px its own come out under a pixel
    // and a half, and the antialiased edge then carries more of the stroke than the fill does.
    match glyph {
        Glyph::Previous => vec![
            Shape::rounded(&bar(5.5, 6.0, 8.0, 18.0), 1.0),
            Shape::rounded(&[(18.0, 6.0), (18.0, 18.0), (9.5, 12.0)], 1.0),
        ],
        Glyph::Next => vec![
            Shape::rounded(&bar(16.0, 6.0, 18.5, 18.0), 1.0),
            Shape::rounded(&[(6.0, 6.0), (14.5, 12.0), (6.0, 18.0)], 1.0),
        ],
        Glyph::Play => vec![Shape::rounded(&[(8.0, 5.0), (19.0, 12.0), (8.0, 19.0)], 1.5)],
        Glyph::Pause => vec![
            Shape::rounded(&bar(6.0, 5.0, 10.0, 19.0), 1.0),
            Shape::rounded(&bar(14.0, 5.0, 18.0, 19.0), 1.0),
        ],
    }
}

fn bar(left: f64, top: f64, right: f64, bottom: f64) -> [Point; 4] {
    [(left, top), (right, top), (right, bottom), (left, bottom)]
}

/// Moves every edge of a convex outline inward by `by` and joins the moved edges where they meet.
fn inset(outline: &[Point], by: f64) -> Vec<Point> {
    let count = outline.len();
    // The left-hand normal points inward on a positively wound outline, outward on the other.
    let winding = signed_area(outline).signum();
    let moved_edge = |i: usize| {
        let (from, to) = (outline[i], outline[(i + 1) % count]);
        let along = (to.0 - from.0, to.1 - from.1);
        let length = along.0.hypot(along.1);
        let inward = (-along.1 / length * winding, along.0 / length * winding);
        ((from.0 + inward.0 * by, from.1 + inward.1 * by), along)
    };
    (0..count)
        .map(|i| {
            let (start_a, along_a) = moved_edge((i + count - 1) % count);
            let (start_b, along_b) = moved_edge(i);
            let gap = (start_b.0 - start_a.0, start_b.1 - start_a.1);
            let t = cross(gap, along_b) / cross(along_a, along_b);
            (start_a.0 + along_a.0 * t, start_a.1 + along_a.1 * t)
        })
        .collect()
}

/// Distance to the outline, negative inside it.
fn signed_distance(outline: &[Point], p: Point) -> f64 {
    let mut nearest = f64::INFINITY;
    let mut inside = false;
    let mut previous = outline[outline.len() - 1];
    for &vertex in outline {
        let edge = (previous.0 - vertex.0, previous.1 - vertex.1);
        let offset = (p.0 - vertex.0, p.1 - vertex.1);
        let t = (dot(offset, edge) / dot(edge, edge)).clamp(0.0, 1.0);
        let gap = (offset.0 - edge.0 * t, offset.1 - edge.1 * t);
        nearest = nearest.min(dot(gap, gap));

        if (vertex.1 <= p.1) != (previous.1 <= p.1) {
            let crossing = vertex.0 + (p.1 - vertex.1) * edge.0 / edge.1;
            if p.0 < crossing {
                inside = !inside;
            }
        }
        previous = vertex;
    }
    if inside { -nearest.sqrt() } else { nearest.sqrt() }
}

fn signed_area(outline: &[Point]) -> f64 {
    let mut previous = outline[outline.len() - 1];
    let mut twice = 0.0;
    for &vertex in outline {
        twice += cross(previous, vertex);
        previous = vertex;
    }
    twice / 2.0
}

fn dot(a: Point, b: Point) -> f64 {
    a.0 * b.0 + a.1 * b.1
}

fn cross(a: Point, b: Point) -> f64 {
    a.0 * b.1 - a.1 * b.0
}

#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "the coverage is clamped to 0..=1 before it is scaled to a byte"
)]
fn to_byte(coverage: f64) -> u8 {
    (coverage * 255.0).round() as u8
}

#[cfg(test)]
#[path = "tests/glyphs_tests.rs"]
mod tests;
