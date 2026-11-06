//! Reading one colour out of each zone.
//!
//! Four things here, and all four matter:
//!
//! * **average in linear light** — sRGB is gamma-encoded, so averaging encoded
//!   values is arithmetic on a non-linear quantity and under-reports brightness;
//! * **density from the zone's size on screen** — a fixed grid is too sparse for
//!   a corner and wasteful for a side;
//! * **reject clipped samples, but only while they are a minority** — dropping
//!   every clipped sample turns a half-bright zone into its own dark half;
//! * **smooth over time** — sensors are noisy and the strip should not flicker.
//!
//! None of it needs a calibration pass. The tiers that do (flat field, colour
//! matrix, exposure lock) sit on top of this and change none of the structure.

use std::sync::LazyLock;

use super::calibration::FrameSize;
use super::geometry::{Homography, Rect};
use super::{
    CLEAN_SAMPLE_FRACTION, CLEAN_SAMPLE_MINIMUM, CLIP_LEVEL, CLIP_SAMPLE_FRACTION, CORNERS,
    INSET_HEIGHT, INSET_WIDTH, INSET_X, INSET_Y, SAMPLE_PIXELS, SAMPLES_MAX, SAMPLES_MIN, SEGMENTS,
    SMOOTHING,
};

/// One colour, sRGB encoded.
pub type Rgb = [u8; 3];

/// A captured frame: the pixels the sampler reads and the JPEG the preview
/// forwards.
///
/// Both halves are kept because they want different things: sampling needs
/// decoded RGB, the browser needs a compressible image, and the camera hands us
/// a JPEG that can be forwarded byte for byte rather than re-encoded.
#[derive(Clone, PartialEq, Eq)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    /// Three bytes per pixel, row-major, no padding.
    pub rgb: Vec<u8>,
    /// The frame as the camera produced it.
    pub jpeg: Vec<u8>,
}

impl Frame {
    pub const fn size(&self) -> FrameSize {
        FrameSize::new(self.width, self.height)
    }

    fn pixel(&self, x: u32, y: u32) -> Rgb {
        let offset = (y as usize * self.width as usize + x as usize) * 3;
        [self.rgb[offset], self.rgb[offset + 1], self.rgb[offset + 2]]
    }
}

/// What one frame produced.
#[derive(Debug, Clone, PartialEq)]
pub struct Reading {
    /// Index `n` drives segment `n`.
    pub colours: [Rgb; SEGMENTS],
    /// True where the camera is over-exposing the zone badly enough that hue is
    /// no longer trustworthy.
    pub clipped: [bool; SEGMENTS],
    /// Each zone's four corners, projected into the frame and normalised.
    ///
    /// Sent to the browser so that the overlay is drawn from the same numbers
    /// the sampler used: the page never has to implement the geometry, and the
    /// two cannot drift apart.
    pub outlines: [[[f32; 2]; CORNERS]; SEGMENTS],
    pub clipped_zones: usize,
}

impl Reading {
    fn blank() -> Self {
        Self {
            colours: [[0; 3]; SEGMENTS],
            clipped: [false; SEGMENTS],
            outlines: [[[0.0; 2]; CORNERS]; SEGMENTS],
            clipped_zones: 0,
        }
    }
}

/// sRGB byte to linear light. A table because it is evaluated millions of times
/// a minute; the inverse is only needed once per channel per zone per frame.
static TO_LINEAR: LazyLock<[f32; 256]> = LazyLock::new(|| {
    let mut table = [0.0_f32; 256];
    for (value, entry) in table.iter_mut().enumerate() {
        let c = value as f32 / 255.0;
        *entry = if c <= 0.040_45 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        };
    }
    table
});

/// Linear light back to an sRGB byte.
///
/// Rounded, not truncated: a flat 128 averaged in linear light comes back as
/// 127.9999 and truncation would report 127, which makes the round trip look
/// lossy when it is not.
fn to_srgb(value: f32) -> u8 {
    if value.is_nan() || value <= 0.0 {
        return 0;
    }
    if value >= 1.0 {
        return 255;
    }
    let encoded = if value <= 0.003_130_8 {
        value * 12.92
    } else {
        1.055 * value.powf(1.0 / 2.4) - 0.055
    };
    (encoded * 255.0).round().clamp(0.0, 255.0) as u8
}

/// Holds the geometry for one calibration and the running smoothed colours.
pub struct Sampler {
    zones: [Rect; SEGMENTS],
    homography: Homography,
    aspect: f32,
    linear: bool,
    smoothed: [[f32; 3]; SEGMENTS],
    primed: bool,
}

impl Sampler {
    /// `None` when the calibration's corners do not define a usable quad.
    pub fn new(calibration: &super::Calibration) -> Option<Self> {
        Some(Self {
            zones: super::geometry::zone_rects(calibration.aspect, calibration.depth),
            homography: calibration.homography()?,
            aspect: calibration.aspect,
            linear: calibration.linear,
            smoothed: [[0.0; 3]; SEGMENTS],
            primed: false,
        })
    }

    pub fn sample(&mut self, frame: &Frame) -> Reading {
        let mut reading = Reading::blank();
        let mut clipped_zones = 0;

        for (index, zone) in self.zones.iter().copied().enumerate() {
            let (raw, clipped) =
                average_zone(frame, zone, self.aspect, &self.homography, self.linear);

            if clipped {
                clipped_zones += 1;
            }
            reading.clipped[index] = clipped;

            if self.primed {
                for (channel, value) in raw.iter().enumerate() {
                    self.smoothed[index][channel] +=
                        (*value - self.smoothed[index][channel]) * SMOOTHING;
                }
            } else {
                self.smoothed[index] = raw;
            }
            // Smoothing happens on the encoded value, which is what the strip
            // will be sent and what the eye is looking at. It is the last
            // arithmetic on the signal, so nothing downstream has to know.
            reading.colours[index] = [
                round_byte(self.smoothed[index][0]),
                round_byte(self.smoothed[index][1]),
                round_byte(self.smoothed[index][2]),
            ];
            reading.outlines[index] = project_outline(zone, self.aspect, &self.homography);
        }

        self.primed = true;
        reading.clipped_zones = clipped_zones;
        reading
    }
}

fn project_outline(zone: Rect, aspect: f32, homography: &Homography) -> [[f32; 2]; CORNERS] {
    let mut outline = [[f32::NAN; 2]; CORNERS];
    for (position, [x, y]) in zone.corners().iter().copied().enumerate() {
        if let Some(point) = homography.project(x / aspect, y) {
            outline[position] = point;
        }
    }
    outline
}

/// Average one zone, in linear light unless told otherwise.
///
/// Returns the colour and whether the zone is over-exposed.
pub fn average_zone(
    frame: &Frame,
    zone: Rect,
    aspect: f32,
    homography: &Homography,
    linear: bool,
) -> ([f32; 3], bool) {
    if !zone.is_usable() {
        return ([0.0; 3], false);
    }

    let projected = project_outline(zone, aspect, homography);
    if projected.iter().any(|point| !point[0].is_finite()) {
        return ([0.0; 3], false);
    }

    let width = frame.width as f32;
    let height = frame.height as f32;
    let pixels_across = distance(projected[0], projected[1], width, height);
    let pixels_down = distance(projected[0], projected[3], width, height);
    let nx = samples_along(pixels_across);
    let ny = samples_along(pixels_down);

    let mut all_linear = [0.0_f32; 3];
    let mut all_encoded = [0.0_f32; 3];
    let mut clean_linear = [0.0_f32; 3];
    let mut clean_encoded = [0.0_f32; 3];
    let mut total = 0_usize;
    let mut clean = 0_usize;

    for i in 0..nx {
        let fx = INSET_X + INSET_WIDTH * (i as f32 / (nx - 1) as f32);
        let u = (zone.x0 + (zone.x1 - zone.x0) * fx) / aspect;

        for j in 0..ny {
            let fy = INSET_Y + INSET_HEIGHT * (j as f32 / (ny - 1) as f32);
            let v = zone.y0 + (zone.y1 - zone.y0) * fy;

            let Some(point) = homography.project(u, v) else {
                continue;
            };

            // Truncate rather than round: either is a half-pixel choice, and
            // truncation keeps the sample inside the zone at the low edge.
            let x = (point[0] * width) as i32;
            let y = (point[1] * height) as i32;
            if x < 0 || y < 0 || x >= frame.width as i32 || y >= frame.height as i32 {
                continue;
            }

            let pixel = frame.pixel(x as u32, y as u32);
            let linear_pixel = [
                TO_LINEAR[pixel[0] as usize],
                TO_LINEAR[pixel[1] as usize],
                TO_LINEAR[pixel[2] as usize],
            ];

            for channel in 0..3 {
                all_linear[channel] += linear_pixel[channel];
                all_encoded[channel] += f32::from(pixel[channel]);
            }
            total += 1;

            // A channel at or above the clip level has no usable hue: its true
            // value is unknown, so the ratio between channels is wrong.
            if pixel.iter().any(|&value| value >= CLIP_LEVEL) {
                continue;
            }
            for channel in 0..3 {
                clean_linear[channel] += linear_pixel[channel];
                clean_encoded[channel] += f32::from(pixel[channel]);
            }
            clean += 1;
        }
    }

    if total == 0 {
        return ([0.0; 3], false);
    }

    // Drop the clipped samples only while they are a small minority. A zone that
    // is genuinely bright has to keep its energy, otherwise it reads as its own
    // dark parts — a half-black, half-white zone averages to black.
    let threshold = ((total as f32) * CLEAN_SAMPLE_FRACTION).ceil() as usize;
    let use_clean = clean >= CLEAN_SAMPLE_MINIMUM.max(threshold);
    let divisor = if use_clean { clean } else { total } as f32;

    let mut rgb = [0.0_f32; 3];
    for (channel, slot) in rgb.iter_mut().enumerate() {
        let encoded_sum = if use_clean {
            clean_encoded
        } else {
            all_encoded
        }[channel];
        let linear_sum = if use_clean { clean_linear } else { all_linear }[channel];
        *slot = if linear {
            to_srgb(linear_sum / divisor).into()
        } else {
            (encoded_sum / divisor).round().clamp(0.0, 255.0)
        };
    }

    let clipped_fraction = 1.0 - clean as f32 / total as f32;
    (rgb, clipped_fraction > CLIP_SAMPLE_FRACTION)
}

/// One sample per this many pixels on screen, clamped to a sane range.
fn samples_along(extent: f32) -> usize {
    if !extent.is_finite() {
        return SAMPLES_MIN;
    }
    let wanted = (extent / SAMPLE_PIXELS).round();
    (wanted as usize).clamp(SAMPLES_MIN, SAMPLES_MAX)
}

fn round_byte(value: f32) -> u8 {
    value.round().clamp(0.0, 255.0) as u8
}

fn distance(a: [f32; 2], b: [f32; 2], width: f32, height: f32) -> f32 {
    let dx = (a[0] - b[0]) * width;
    let dy = (a[1] - b[1]) * height;
    (dx * dx + dy * dy).sqrt()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity() -> Homography {
        Homography::solve(&[[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]])
            .expect("the unit square maps to itself")
    }

    /// A square screen, so that rect space and the frame line up exactly and the
    /// expected averages can be reasoned about by eye.
    const SQUARE: f32 = 1.0;
    const SIZE: u32 = 64;

    fn frame_with(pixel: impl Fn(u32, u32) -> Rgb) -> Frame {
        let mut rgb = Vec::with_capacity((SIZE * SIZE * 3) as usize);
        for y in 0..SIZE {
            for x in 0..SIZE {
                rgb.extend_from_slice(&pixel(x, y));
            }
        }
        Frame {
            width: SIZE,
            height: SIZE,
            rgb,
            jpeg: Vec::new(),
        }
    }

    fn whole_screen() -> Rect {
        Rect::new(0.0, 0.0, SQUARE, 1.0)
    }

    #[test]
    fn a_flat_grey_survives_the_linear_round_trip_exactly() {
        let frame = frame_with(|_, _| [128, 128, 128]);
        let (rgb, clipped) = average_zone(&frame, whole_screen(), SQUARE, &identity(), true);
        assert!(!clipped);
        for channel in rgb {
            assert!(
                (channel - 128.0).abs() < 0.6,
                "a flat 128 must come back as 128, got {rgb:?}"
            );
        }
    }

    #[test]
    fn a_half_black_half_white_zone_is_brighter_in_linear_light() {
        let frame = frame_with(|x, _| {
            if x < SIZE / 2 {
                [0, 0, 0]
            } else {
                [255, 255, 255]
            }
        });

        let (linear, _) = average_zone(&frame, whole_screen(), SQUARE, &identity(), true);
        let (encoded, _) = average_zone(&frame, whole_screen(), SQUARE, &identity(), false);

        // The physically correct answer for half black and half white is ~188;
        // averaging the encoded bytes instead lands near 126 and under-reports
        // the brightness by about sixty levels.
        assert!(linear[0] > 180.0, "linear gave {linear:?}");
        assert!(encoded[0] < 140.0, "encoded gave {encoded:?}");
        assert!(
            linear[0] - encoded[0] > 40.0,
            "linear {linear:?} should be well above encoded {encoded:?}"
        );
    }

    #[test]
    fn a_mostly_clipped_zone_keeps_its_brightness_and_is_flagged() {
        // White everywhere except a strip down the right-hand side, so nearly
        // every sample is clipped and the clean average cannot be used.
        let frame = frame_with(|x, _| {
            if x >= (SIZE as f32 * 0.85) as u32 {
                [0, 0, 0]
            } else {
                [255, 255, 255]
            }
        });

        let (rgb, clipped) = average_zone(&frame, whole_screen(), SQUARE, &identity(), true);
        assert!(clipped, "an over-exposed zone must be reported");
        assert!(
            rgb[0] > 200.0,
            "a bright zone must stay bright, got {rgb:?}"
        );
    }

    #[test]
    fn a_sample_grid_never_collapses_to_a_single_point() {
        assert_eq!(samples_along(0.0), SAMPLES_MIN);
        assert_eq!(samples_along(-5.0), SAMPLES_MIN);
        assert_eq!(samples_along(f32::NAN), SAMPLES_MIN);
        assert_eq!(samples_along(10_000.0), SAMPLES_MAX);
    }
}
