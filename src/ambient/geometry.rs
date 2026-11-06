//! Rectified space, the homography, and the twenty NS03 zones.
//!
//! One coordinate system is used throughout, called **rect space**: `x` in
//! `[0, aspect]`, `y` in `[0, 1]`, origin at the top-left of the screen, and
//! units physical so that one unit of `x` is one unit of `y` on the glass.
//!
//! Every zone is an axis-aligned rectangle in that space. Perspective lives
//! entirely in the homography, so no zone can be distorted by the angle the
//! camera happens to sit at — which is the whole reason the layout is expressed
//! as rectangles rather than walked around the perimeter. Walking it puts the
//! corner bands on a normal that flips halfway round, and the corners come out
//! visibly L-shaped with no depth value that fixes them.

use super::{DEPTH_MAX, SEGMENTS, SIDE_SECTIONS, TOP_SECTIONS};

/// Below this a pivot counts as zero and the system is treated as singular.
///
/// The solve runs on image coordinates normalised to `0..1`, so every entry of
/// the system is the same order of magnitude and an absolute epsilon means the
/// same thing at any corner position. Solving in raw pixels would not: the terms
/// span five orders of magnitude and this threshold would be arbitrary.
const PIVOT_EPSILON: f64 = 1e-12;

/// How close to singular a solved homography may be before it is refused.
///
/// Pivoting alone is not enough: a quad with three collinear corners still has a
/// *consistent* system, it simply has a singular solution — the matrix sends the
/// four domain points to four collinear image points and squashes everything
/// onto a line. Comparing the determinant against the cube of the largest entry
/// makes the test scale-free, so it means the same thing for a wide quad as for a
/// tall one.
const DEGENERATE: f64 = 1e-6;

/// The unit square, in the corner order every quad in this module uses.
const UNIT_SQUARE: [[f64; 2]; 4] = [[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]];

// Zone indices, derived rather than written out so that changing the section
// counts would move them with it. The numbering itself is fixed by the hardware:
// segment 0 sits just left of top centre and the index runs counter-clockwise.
const TL_CORNER: usize = TOP_SECTIONS / 2;
const LEFT_FIRST: usize = TL_CORNER + 1;
const BL_CORNER: usize = LEFT_FIRST + SIDE_SECTIONS;
const BOTTOM_FIRST: usize = BL_CORNER + 1;
const BR_CORNER: usize = BOTTOM_FIRST + TOP_SECTIONS;
const RIGHT_FIRST: usize = BR_CORNER + 1;
const TR_CORNER: usize = RIGHT_FIRST + SIDE_SECTIONS;

// The layout has to fill the strip exactly: a silent off-by-one would leave one
// segment undriven and another driven twice.
const _: () = assert!(
    TR_CORNER + 1 + (TOP_SECTIONS - TOP_SECTIONS / 2) == SEGMENTS,
    "the zone layout does not cover every segment exactly once"
);

/// An axis-aligned rectangle in rect space.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub x0: f32,
    pub y0: f32,
    pub x1: f32,
    pub y1: f32,
}

impl Rect {
    const EMPTY: Self = Self {
        x0: 0.0,
        y0: 0.0,
        x1: 0.0,
        y1: 0.0,
    };

    pub const fn new(x0: f32, y0: f32, x1: f32, y1: f32) -> Self {
        Self { x0, y0, x1, y1 }
    }

    /// Clockwise from the top-left, matching [`UNIT_SQUARE`].
    pub fn corners(self) -> [[f32; 2]; 4] {
        [
            [self.x0, self.y0],
            [self.x1, self.y0],
            [self.x1, self.y1],
            [self.x0, self.y1],
        ]
    }

    pub fn centre(self) -> [f32; 2] {
        [(self.x0 + self.x1) / 2.0, (self.y0 + self.y1) / 2.0]
    }

    pub fn is_usable(self) -> bool {
        self.x1 > self.x0 && self.y1 > self.y0
    }
}

/// The twenty zone rectangles for an `aspect`-ratio screen.
///
/// `depth` is both the corner square's side and the band thickness — one number
/// on purpose, because making them equal is exactly what makes the corners come
/// out square.
///
/// Index `n` is zone `n + 1` and drives segment `n`.
pub fn zone_rects(aspect: f32, depth: f32) -> [Rect; SEGMENTS] {
    let d = depth.clamp(0.0, DEPTH_MAX);
    let section = (aspect - 2.0 * d) / TOP_SECTIONS as f32;
    let side = (1.0 - 2.0 * d) / SIDE_SECTIONS as f32;

    let mut zones = [Rect::EMPTY; SEGMENTS];

    // The four corner squares.
    zones[TL_CORNER] = Rect::new(0.0, 0.0, d, d);
    zones[BL_CORNER] = Rect::new(0.0, 1.0 - d, d, 1.0);
    zones[BR_CORNER] = Rect::new(aspect - d, 1.0 - d, aspect, 1.0);
    zones[TR_CORNER] = Rect::new(aspect - d, 0.0, aspect, d);

    // Top edge. The sections are laid out left to right, but the numbering runs
    // *outward from the seam*: the half left of centre is zones 3, 2, 1 and the
    // half right of it is zones 20, 19, 18.
    for i in 0..TOP_SECTIONS {
        let x0 = d + i as f32 * section;
        let index = if i < TOP_SECTIONS / 2 {
            TOP_SECTIONS / 2 - 1 - i
        } else {
            SEGMENTS - (i - TOP_SECTIONS / 2) - 1
        };
        zones[index] = Rect::new(x0, 0.0, x0 + section, d);
    }

    // Bottom edge: zones 8..=13, left to right.
    for i in 0..TOP_SECTIONS {
        let x0 = d + i as f32 * section;
        zones[BOTTOM_FIRST + i] = Rect::new(x0, 1.0 - d, x0 + section, 1.0);
    }

    // Left and right edges, top to bottom.
    for j in 0..SIDE_SECTIONS {
        let y0 = d + j as f32 * side;
        zones[LEFT_FIRST + j] = Rect::new(0.0, y0, d, y0 + side);
        zones[RIGHT_FIRST + j] = Rect::new(aspect - d, y0, aspect, y0 + side);
    }

    zones
}

/// The projective map from the unit square to a quadrilateral.
///
/// Both the input and the output are normalised to `0..1` of the captured frame,
/// so a calibration survives a change of capture resolution: the screen is in the
/// same place in the frame whatever the sensor does with its pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Homography {
    m: [f64; 9],
}

impl Homography {
    /// Solve for the map taking the unit square onto `quad`, in the order
    /// top-left, top-right, bottom-right, bottom-left.
    ///
    /// Returns `None` when three of the corners are collinear: that quad has no
    /// perspective to recover from, and the operator needs to click them again
    /// rather than be handed a plausible-looking lie.
    pub fn solve(quad: &[[f32; 2]; 4]) -> Option<Self> {
        if quad.iter().any(|[x, y]| !x.is_finite() || !y.is_finite()) {
            return None;
        }

        let mut a = [[0.0_f64; 8]; 8];
        let mut b = [0.0_f64; 8];

        for (i, [x, y]) in UNIT_SQUARE.iter().copied().enumerate() {
            let big_x = f64::from(quad[i][0]);
            let big_y = f64::from(quad[i][1]);
            a[2 * i] = [x, y, 1.0, 0.0, 0.0, 0.0, -x * big_x, -y * big_x];
            a[2 * i + 1] = [0.0, 0.0, 0.0, x, y, 1.0, -x * big_y, -y * big_y];
            b[2 * i] = big_x;
            b[2 * i + 1] = big_y;
        }

        let h = gauss_jordan(a, b)?;
        let m = [h[0], h[1], h[2], h[3], h[4], h[5], h[6], h[7], 1.0];

        let scale = m
            .iter()
            .fold(0.0_f64, |largest, entry| largest.max(entry.abs()));
        if scale == 0.0 || determinant(&m).abs() <= DEGENERATE * scale.powi(3) {
            return None;
        }

        Some(Self { m })
    }

    /// Project a rect-space point, given as `u = x / aspect` and `v = y`.
    ///
    /// Returns normalised frame coordinates, or `None` where the map is at
    /// infinity.
    pub fn project(&self, u: f32, v: f32) -> Option<[f32; 2]> {
        let (u, v) = (f64::from(u), f64::from(v));
        let m = &self.m;
        let w = m[6] * u + m[7] * v + m[8];
        if w.abs() < PIVOT_EPSILON {
            return None;
        }
        Some([
            ((m[0] * u + m[1] * v + m[2]) / w) as f32,
            ((m[3] * u + m[4] * v + m[5]) / w) as f32,
        ])
    }
}

/// Determinant of a row-major 3×3.
fn determinant(m: &[f64; 9]) -> f64 {
    m[0] * (m[4] * m[8] - m[5] * m[7]) - m[1] * (m[3] * m[8] - m[5] * m[6])
        + m[2] * (m[3] * m[7] - m[4] * m[6])
}

/// Gauss–Jordan elimination with partial pivoting, for a square system.
fn gauss_jordan(mut a: [[f64; 8]; 8], mut b: [f64; 8]) -> Option<[f64; 8]> {
    const N: usize = 8;

    for col in 0..N {
        let mut pivot = col;
        for row in (col + 1)..N {
            if a[row][col].abs() > a[pivot][col].abs() {
                pivot = row;
            }
        }
        if a[pivot][col].abs() < PIVOT_EPSILON {
            return None;
        }
        a.swap(col, pivot);
        b.swap(col, pivot);

        let divisor = a[col][col];
        for cell in a[col].iter_mut().skip(col) {
            *cell /= divisor;
        }
        b[col] /= divisor;

        for row in 0..N {
            if row == col {
                continue;
            }
            let factor = a[row][col];
            if factor == 0.0 {
                continue;
            }
            let pivot_row = a[col];
            for (k, value) in a[row].iter_mut().enumerate().skip(col) {
                *value -= factor * pivot_row[k];
            }
            b[row] -= factor * b[col];
        }
    }

    Some(b)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ambient::DEFAULT_ASPECT;

    /// The published zone table for a 16:9 screen at ten percent depth.
    ///
    /// This is the one check the browser prototype could not make: its test card
    /// was painted with the same zone map it then verified, so a mirrored or
    /// mis-numbered layout would have passed it. These numbers come from the
    /// hardware's numbering, not from this module.
    const PUBLISHED: [(usize, [f32; 4]); 20] = [
        (1, [0.625_926, 0.0, 0.888_889, 0.1]),
        (2, [0.362_963, 0.0, 0.625_926, 0.1]),
        (3, [0.1, 0.0, 0.362_963, 0.1]),
        (4, [0.0, 0.0, 0.1, 0.1]),
        (5, [0.0, 0.1, 0.1, 0.5]),
        (6, [0.0, 0.5, 0.1, 0.9]),
        (7, [0.0, 0.9, 0.1, 1.0]),
        (8, [0.1, 0.9, 0.362_963, 1.0]),
        (9, [0.362_963, 0.9, 0.625_926, 1.0]),
        (10, [0.625_926, 0.9, 0.888_889, 1.0]),
        (11, [0.888_889, 0.9, 1.151_852, 1.0]),
        (12, [1.151_852, 0.9, 1.414_815, 1.0]),
        (13, [1.414_815, 0.9, 1.677_778, 1.0]),
        (14, [1.677_778, 0.9, 1.777_778, 1.0]),
        (15, [1.677_778, 0.1, 1.777_778, 0.5]),
        (16, [1.677_778, 0.5, 1.777_778, 0.9]),
        (17, [1.677_778, 0.0, 1.777_778, 0.1]),
        (18, [1.414_815, 0.0, 1.677_778, 0.1]),
        (19, [1.151_852, 0.0, 1.414_815, 0.1]),
        (20, [0.888_889, 0.0, 1.151_852, 0.1]),
    ];

    #[test]
    fn the_zone_table_matches_the_published_one() {
        let zones = zone_rects(DEFAULT_ASPECT, 0.1);
        for (number, expected) in PUBLISHED {
            let actual = zones[number - 1];
            let got = [actual.x0, actual.y0, actual.x1, actual.y1];
            for (index, (got, want)) in got.iter().zip(expected.iter()).enumerate() {
                assert!(
                    (got - want).abs() < 1e-5,
                    "zone {number} edge {index}: got {got}, want {want}"
                );
            }
        }
    }

    #[test]
    fn every_zone_is_populated_and_distinct() {
        // The prototype built its table by walking the perimeter and could emit
        // the same index twice, leaving a zone with no rectangle at all. Both
        // failure modes are cheap to rule out here.
        let zones = zone_rects(DEFAULT_ASPECT, 0.1);
        for (index, zone) in zones.iter().enumerate() {
            assert!(zone.is_usable(), "zone {} is empty: {zone:?}", index + 1);
            for (other_index, other) in zones.iter().enumerate().skip(index + 1) {
                assert_ne!(
                    zone,
                    other,
                    "zones {} and {} are the same rectangle",
                    index + 1,
                    other_index + 1
                );
            }
        }
    }

    #[test]
    fn the_seam_falls_on_the_boundary_between_zone_one_and_twenty() {
        // Zone 1 must end and zone 20 begin exactly at the middle of the top
        // edge, for any depth: that is where the physical strip joins.
        for depth in [0.05_f32, 0.1, 0.2, DEPTH_MAX] {
            let zones = zone_rects(DEFAULT_ASPECT, depth);
            assert!((zones[0].x1 - DEFAULT_ASPECT / 2.0).abs() < 1e-5, "{depth}");
            assert!(
                (zones[19].x0 - DEFAULT_ASPECT / 2.0).abs() < 1e-5,
                "{depth}"
            );
        }
    }

    #[test]
    fn a_skewed_quad_round_trips_through_the_homography() {
        let quad = [[0.17_f32, 0.15], [0.87, 0.10], [0.93, 0.90], [0.11, 0.87]];
        let h = Homography::solve(&quad).expect("the prototype's quad is solvable");
        let corners = [[0.0_f32, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]];
        for (corner, expected) in corners.iter().zip(quad.iter()) {
            let got = h.project(corner[0], corner[1]).expect("inside the quad");
            assert!(
                (got[0] - expected[0]).abs() < 1e-5,
                "{got:?} vs {expected:?}"
            );
            assert!(
                (got[1] - expected[1]).abs() < 1e-5,
                "{got:?} vs {expected:?}"
            );
        }
    }

    #[test]
    fn three_collinear_corners_are_rejected() {
        let degenerate = [[0.0_f32, 0.0], [0.5, 0.5], [1.0, 1.0], [0.0, 1.0]];
        assert!(Homography::solve(&degenerate).is_none());
    }
}
