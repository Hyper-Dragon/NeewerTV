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
    /// Ready to send to the browser: the camera's own JPEG when it gives us
    /// MJPEG, a BMP this daemon assembled when it does not.
    pub preview: Vec<u8>,
    /// Content type matching `preview`.
    pub preview_type: &'static str,
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
    correction: Option<Correction>,
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
            correction: calibration.correction(),
            smoothed: [[0.0; 3]; SEGMENTS],
            primed: false,
        })
    }

    /// Measure every zone as it is, for flat-field calibration.
    ///
    /// Deliberately ignores the sampler's own correction: a field measured through
    /// the last correction could never replace it.
    pub fn measure(&self, frame: &Frame) -> Measurement {
        let mut measurement = Measurement {
            linear: [[0.0; 3]; SEGMENTS],
            clipped: [0.0; SEGMENTS],
        };

        for (index, zone) in self.zones.iter().copied().enumerate() {
            let (linear, clipped) = measure_zone(frame, zone, self.aspect, &self.homography);
            measurement.linear[index] = linear;
            measurement.clipped[index] = clipped;
        }

        measurement
    }

    pub fn sample(&mut self, frame: &Frame) -> Reading {
        let mut reading = Reading::blank();
        let mut clipped_zones = 0;

        for (index, zone) in self.zones.iter().copied().enumerate() {
            let correction = self.correction.map(|correction| correction.zone(index));
            let (raw, clipped) = average_zone(
                frame,
                zone,
                self.aspect,
                &self.homography,
                self.linear,
                correction,
            );

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

/// How bright each zone is when the screen is white, and how much of what it reads
/// when the screen is black is the room rather than the screen.
///
/// Both halves are in linear light, per zone per channel. The gain is relative to
/// the mean across zones, so applying it flattens the camera's own shading without
/// changing the overall exposure.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Correction {
    black: [[f32; 3]; SEGMENTS],
    gain: [[f32; 3]; SEGMENTS],
}

/// One zone's slice of a [`Correction`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ZoneCorrection {
    pub black: [f32; 3],
    pub gain: [f32; 3],
}

/// A white field darker than this, in linear light, is not a white field.
pub const MIN_MEASURED_WHITE: f32 = 0.01;
/// Gains are clamped: a zone that measured almost nothing would otherwise get an
/// enormous multiplier and turn sensor noise into a light show.
const MIN_GAIN: f32 = 0.25;
const MAX_GAIN: f32 = 4.0;

impl Correction {
    /// `None` when a white field is present but unusable — a zone that saw almost
    /// nothing cannot be corrected, because the number it is divided by is noise.
    ///
    /// Either half may stand alone. A white field on its own flattens the camera's
    /// shading; a black field on its own removes the floor the camera reports in
    /// the dark. Measuring one is worth doing before the other.
    pub fn from_fields(
        black: Option<&[[f32; 3]; SEGMENTS]>,
        white: Option<&[[f32; 3]; SEGMENTS]>,
    ) -> Option<Self> {
        let mut gain = [[1.0_f32; 3]; SEGMENTS];

        if let Some(white) = white {
            let mut mean = [0.0_f32; 3];
            for zone in white {
                for (channel, value) in zone.iter().enumerate() {
                    mean[channel] += value;
                }
            }
            for value in &mut mean {
                *value /= SEGMENTS as f32;
            }

            for (index, zone) in white.iter().enumerate() {
                for (channel, value) in zone.iter().enumerate() {
                    if *value <= MIN_MEASURED_WHITE {
                        return None;
                    }
                    gain[index][channel] = (mean[channel] / value).clamp(MIN_GAIN, MAX_GAIN);
                }
            }
        }

        Some(Self {
            black: black.copied().unwrap_or([[0.0; 3]; SEGMENTS]),
            gain,
        })
    }

    pub fn zone(&self, index: usize) -> ZoneCorrection {
        ZoneCorrection {
            black: self.black[index],
            gain: self.gain[index],
        }
    }

    /// How far apart the zones measured, as a fraction of the mean — 0.2 means
    /// the brightest zone read twenty percent above the average.
    pub fn spread(&self) -> f32 {
        let mut lowest = f32::MAX;
        let mut highest = 0.0_f32;
        for zone in &self.gain {
            for value in zone {
                lowest = lowest.min(*value);
                highest = highest.max(*value);
            }
        }
        if highest <= 0.0 {
            0.0
        } else {
            1.0 - lowest / highest
        }
    }
}

/// Raw per-zone numbers from one frame, before any correction or smoothing.
#[derive(Debug, Clone, PartialEq)]
pub struct Measurement {
    /// Mean linear light per zone per channel.
    pub linear: [[f32; 3]; SEGMENTS],
    /// Fraction of each zone's samples that hit the clip level. A white field
    /// with any of this is unusable: the value it would divide by is unknown.
    pub clipped: [f32; SEGMENTS],
}

impl Measurement {
    /// The worst clipping any zone saw.
    pub fn worst_clip(&self) -> f32 {
        self.clipped.iter().copied().fold(0.0, f32::max)
    }

    /// The brightest thing any zone saw, for telling an empty screen from a lit one.
    pub fn brightest(&self) -> f32 {
        self.linear
            .iter()
            .flat_map(|zone| zone.iter())
            .copied()
            .fold(0.0, f32::max)
    }
}

/// Everything one zone's samples added up to.
struct Totals {
    all_linear: [f32; 3],
    all_encoded: [f32; 3],
    clean_linear: [f32; 3],
    clean_encoded: [f32; 3],
    total: usize,
    clean: usize,
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
    correction: Option<ZoneCorrection>,
) -> ([f32; 3], bool) {
    let totals = collect(frame, zone, aspect, homography, correction);
    if totals.total == 0 {
        return ([0.0; 3], false);
    }

    // Drop the clipped samples only while they are a small minority. A zone that
    // is genuinely bright has to keep its energy, otherwise it reads as its own
    // dark parts — a half-black, half-white zone averages to black.
    let threshold = ((totals.total as f32) * CLEAN_SAMPLE_FRACTION).ceil() as usize;
    let use_clean = totals.clean >= CLEAN_SAMPLE_MINIMUM.max(threshold);
    let divisor = if use_clean {
        totals.clean
    } else {
        totals.total
    } as f32;

    // A correction only exists in linear light, so asking for one implies it.
    let linear = linear || correction.is_some();

    let mut rgb = [0.0_f32; 3];
    for (channel, slot) in rgb.iter_mut().enumerate() {
        let encoded_sum = if use_clean {
            totals.clean_encoded
        } else {
            totals.all_encoded
        }[channel];
        let linear_sum = if use_clean {
            totals.clean_linear
        } else {
            totals.all_linear
        }[channel];
        *slot = if linear {
            to_srgb(linear_sum / divisor).into()
        } else {
            (encoded_sum / divisor).round().clamp(0.0, 255.0)
        };
    }

    let clipped_fraction = 1.0 - totals.clean as f32 / totals.total as f32;
    (rgb, clipped_fraction > CLIP_SAMPLE_FRACTION)
}

/// Measure one zone exactly as it is: mean linear light, and how much of it was
/// clipped.
///
/// No correction, no clip rejection, no smoothing — a flat field is being
/// measured, and every one of those would answer a different question.
pub fn measure_zone(
    frame: &Frame,
    zone: Rect,
    aspect: f32,
    homography: &Homography,
) -> ([f32; 3], f32) {
    let totals = collect(frame, zone, aspect, homography, None);
    if totals.total == 0 {
        return ([0.0; 3], 0.0);
    }

    let count = totals.total as f32;
    let clipped = 1.0 - totals.clean as f32 / count;
    (
        [
            totals.all_linear[0] / count,
            totals.all_linear[1] / count,
            totals.all_linear[2] / count,
        ],
        clipped,
    )
}

/// Walk one zone's sample grid.
fn collect(
    frame: &Frame,
    zone: Rect,
    aspect: f32,
    homography: &Homography,
    correction: Option<ZoneCorrection>,
) -> Totals {
    let mut totals = Totals {
        all_linear: [0.0; 3],
        all_encoded: [0.0; 3],
        clean_linear: [0.0; 3],
        clean_encoded: [0.0; 3],
        total: 0,
        clean: 0,
    };
    if !zone.is_usable() {
        return totals;
    }

    let projected = project_outline(zone, aspect, homography);
    if projected.iter().any(|point| !point[0].is_finite()) {
        return totals;
    }

    let width = frame.width as f32;
    let height = frame.height as f32;
    let pixels_across = distance(projected[0], projected[1], width, height);
    let pixels_down = distance(projected[0], projected[3], width, height);
    let nx = samples_along(pixels_across);
    let ny = samples_along(pixels_down);

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
            let mut linear_pixel = [
                TO_LINEAR[pixel[0] as usize],
                TO_LINEAR[pixel[1] as usize],
                TO_LINEAR[pixel[2] as usize],
            ];

            // The flat-field correction is affine with a clamp, so it has to be
            // applied per sample rather than to the average. The clip test below
            // deliberately still looks at the raw byte: a saturated sensor is
            // saturated whatever is done with it afterwards.
            if let Some(correction) = correction {
                for (channel, value) in linear_pixel.iter_mut().enumerate() {
                    *value = ((*value - correction.black[channel]).max(0.0)
                        * correction.gain[channel])
                        .clamp(0.0, 1.0);
                }
            }

            for (channel, value) in linear_pixel.iter().enumerate() {
                totals.all_linear[channel] += value;
                totals.all_encoded[channel] += f32::from(pixel[channel]);
            }
            totals.total += 1;

            // A channel at or above the clip level has no usable hue: its true
            // value is unknown, so the ratio between channels is wrong.
            if pixel.iter().any(|&value| value >= CLIP_LEVEL) {
                continue;
            }
            for (channel, value) in linear_pixel.iter().enumerate() {
                totals.clean_linear[channel] += value;
                totals.clean_encoded[channel] += f32::from(pixel[channel]);
            }
            totals.clean += 1;
        }
    }

    totals
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
            preview: Vec::new(),
            preview_type: crate::ambient::camera::TYPE_JPEG,
        }
    }

    fn whole_screen() -> Rect {
        Rect::new(0.0, 0.0, SQUARE, 1.0)
    }

    #[test]
    fn a_flat_grey_survives_the_linear_round_trip_exactly() {
        let frame = frame_with(|_, _| [128, 128, 128]);
        let (rgb, clipped) = average_zone(&frame, whole_screen(), SQUARE, &identity(), true, None);
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

        let (linear, _) = average_zone(&frame, whole_screen(), SQUARE, &identity(), true, None);
        let (encoded, _) = average_zone(&frame, whole_screen(), SQUARE, &identity(), false, None);

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

        let (rgb, clipped) = average_zone(&frame, whole_screen(), SQUARE, &identity(), true, None);
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

    #[test]
    fn a_white_field_on_its_own_flattens_the_zones() {
        // The camera's own shading: one corner sees twice what its neighbour does.
        let mut white = [[0.5_f32; 3]; SEGMENTS];
        white[0] = [1.0; 3];

        let correction =
            Correction::from_fields(None, Some(&white)).expect("a white field is usable");

        let apply = |value: f32, zone: ZoneCorrection| (value - zone.black[0]) * zone.gain[0];
        let bright = apply(1.0, correction.zone(0));
        let dim = apply(0.5, correction.zone(1));

        assert!(
            (bright - dim).abs() < 1e-5,
            "both zones should land on the same value, got {bright} and {dim}"
        );
        assert_eq!(
            correction.zone(0).black,
            [0.0; 3],
            "no black field measured, so no floor claimed"
        );
    }

    #[test]
    fn a_black_field_on_its_own_removes_the_floor_and_leaves_the_gain_alone() {
        let mut black = [[0.02_f32; 3]; SEGMENTS];
        black[6] = [0.2; 3];

        let correction =
            Correction::from_fields(Some(&black), None).expect("a black field is usable");
        let zone = correction.zone(6);

        assert_eq!(zone.gain, [1.0; 3], "there is nothing to flatten");
        assert!((zone.black[0] - 0.2).abs() < 1e-6, "the floor is the floor");
    }

    #[test]
    fn a_white_field_that_measured_nothing_is_refused() {
        // Dividing by a number that is sensor noise would turn the strip into a
        // light show, so an unusable white field has to be refused outright.
        let dark = [[0.0_f32; 3]; SEGMENTS];
        assert!(Correction::from_fields(None, Some(&dark)).is_none());
    }
}
