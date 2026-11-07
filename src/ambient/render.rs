//! Mapping a camera reading onto something the strip can actually show.
//!
//! [`rgb_to_hsi`] is honest: it reports what the camera saw. It is not, however,
//! what the firmware renders *well*, and two properties of the hardware make the
//! raw mapping look broken:
//!
//! * **It brushes off low saturation.** A dim, desaturated sample — exactly what
//!   an averaged, low-light camera frame produces — comes back as near-white
//!   instead of as the colour it describes.
//! * **It has a dead zone at the bottom of the lightness range.** Measured on an
//!   NS03: segments do not light at all below about L=20.
//!
//! So the sample is lifted on the way out: saturation is amplified, and lightness
//! is scaled above a small floor. The floor exists so that a genuinely dark scene
//! stays dark — lifting every zone by a constant would make a black film glow,
//! which is the defect this whole feature exists to avoid.
//!
//! These are firmware constants, not facts about the camera, and they are guesses
//! until somebody measures the device's transfer curve with the camera pointed at
//! its own light. They are exposed through the environment for that reason.

use serde::{Deserialize, Serialize};

use crate::protocol::hsi::rgb_to_hsi;

/// One segment colour, in the order the wire wants it.
pub type Entry = (u8, u16, u8);

pub const DEFAULT_SATURATION_GAIN: f32 = 2.0;
pub const DEFAULT_LIGHTNESS_GAIN: f32 = 1.4;
pub const DEFAULT_LIGHTNESS_FLOOR: u8 = 8;
pub const DEFAULT_BLACK_LEVEL: u8 = 10;
/// Above this a black level is a darkening knob rather than a correction.
pub const MAX_BLACK_LEVEL: u8 = 64;
/// Gains beyond this are almost certainly a mistake rather than a setting.
pub const MAX_GAIN: f32 = 8.0;

/// A zone is allowed its full saturation from this lightness up.
///
/// Below it the allowance falls away quadratically, because what sits there is
/// the camera's noise floor rather than a colour. 45 is above the firmware's own
/// dead zone (about L=20), so a zone dim enough to be dark is also a zone that
/// carries no colour.
pub const FULL_COLOUR_LIGHTNESS: u8 = 45;

/// How a reading is pushed towards what the firmware renders well.
///
/// Part of the calibration file, not the environment: every one of these is a
/// judgement made while looking at the screen and the strip together, which is a
/// thing the calibration page can offer and a compose file cannot.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Render {
    /// Every channel at or below this is treated as black, and the rest is
    /// rescaled so white stays white.
    ///
    /// A camera that has not had its exposure locked does not report black as
    /// zero: aimed at a dark scene it raises gain until the screen reads as mid
    /// grey, and the strip then stays lit through the whole film. Set this to the
    /// value the swatches show while the screen is black.
    #[serde(default = "default_black_level")]
    pub black_level: u8,
    /// Multiplier on saturation. The firmware renders low saturation as white.
    #[serde(default = "default_saturation_gain")]
    pub saturation_gain: f32,
    /// Multiplier on the part of lightness above [`Self::lightness_floor`].
    #[serde(default = "default_lightness_gain")]
    pub lightness_gain: f32,
    /// Lightness at or below this passes through untouched, so black stays black.
    #[serde(default = "default_lightness_floor")]
    pub lightness_floor: u8,
}

fn default_black_level() -> u8 {
    DEFAULT_BLACK_LEVEL
}

fn default_saturation_gain() -> f32 {
    DEFAULT_SATURATION_GAIN
}

fn default_lightness_gain() -> f32 {
    DEFAULT_LIGHTNESS_GAIN
}

fn default_lightness_floor() -> u8 {
    DEFAULT_LIGHTNESS_FLOOR
}

impl Render {
    /// Reject values that would turn a setting into a mistake.
    pub fn is_sane(&self) -> bool {
        self.black_level <= MAX_BLACK_LEVEL
            && self.lightness_floor <= 100
            && (0.0..=MAX_GAIN).contains(&self.saturation_gain)
            && (0.0..=MAX_GAIN).contains(&self.lightness_gain)
    }
}

impl Default for Render {
    fn default() -> Self {
        Self {
            black_level: DEFAULT_BLACK_LEVEL,
            saturation_gain: DEFAULT_SATURATION_GAIN,
            lightness_gain: DEFAULT_LIGHTNESS_GAIN,
            lightness_floor: DEFAULT_LIGHTNESS_FLOOR,
        }
    }
}

impl Render {
    /// Every knob turned off: what the camera said, changed as little as the
    /// firmware allows.
    ///
    /// Used by the calibration page's raw view. If the strip matches the picture
    /// with this on and does not with it off, the transfer is the problem; if it
    /// matches neither, the camera never saw the picture properly.
    #[must_use]
    pub fn identity() -> Self {
        Self {
            black_level: 0,
            saturation_gain: 1.0,
            lightness_gain: 1.0,
            lightness_floor: 0,
        }
    }
}

/// Convert a whole reading into segment entries.
pub fn entries(colours: &[[u8; 3]], render: Render) -> Vec<Entry> {
    colours
        .iter()
        .map(|&[red, green, blue]| entry(red, green, blue, render))
        .collect()
}

fn entry(red: u8, green: u8, blue: u8, render: Render) -> Entry {
    let hsi = rgb_to_hsi(
        black_point(red, render.black_level),
        black_point(green, render.black_level),
        black_point(blue, render.black_level),
    );

    // A dark zone has a hue, and it is the hue of camera noise. Multiplying that
    // by the saturation gain is what turns a black screen into a rainbow: three
    // channels a few levels apart describe a fully saturated direction, and the
    // strip renders it at whatever lightness the zone has. So the colour a zone
    // is allowed to have scales with how lit it is — nothing at black, all of it
    // by the time the zone is properly bright.
    let saturation = scale(u16::from(hsi.saturation), render.saturation_gain);
    let saturation = u16::from(saturation) * u16::from(colour_allowed(hsi.lightness)) / 100;
    let saturation = saturation as u8;

    let lightness = if hsi.lightness <= render.lightness_floor {
        hsi.lightness
    } else {
        let above = u16::from(hsi.lightness - render.lightness_floor);
        let lifted =
            u16::from(render.lightness_floor) + u16::from(scale(above, render.lightness_gain));
        lifted.min(100) as u8
    };

    (lightness, hsi.hue, saturation)
}

/// How much of its saturation a zone at this lightness is allowed, 0–100.
///
/// Squared rather than linear: the noise band just above black has to be
/// suppressed hard, while a zone that is merely dim keeps its colour.
fn colour_allowed(lightness: u8) -> u8 {
    let full = u32::from(FULL_COLOUR_LIGHTNESS);
    let lit = u32::from(lightness).min(full);
    (lit * lit * 100 / (full * full)) as u8
}

/// Reset the black point: at or below `black` is zero, and the rest is stretched
/// back so that 255 stays 255.
fn black_point(value: u8, black: u8) -> u8 {
    if black == 0 {
        return value;
    }
    if value <= black {
        return 0;
    }
    let span = u32::from(255 - black);
    (((u32::from(value) - u32::from(black)) * 255 + span / 2) / span) as u8
}

/// Multiply, round, clamp to the device's 0–100.
fn scale(value: u16, gain: f32) -> u8 {
    let scaled = f32::from(value) * gain;
    if scaled.is_nan() {
        return 100;
    }
    scaled.round().clamp(0.0, 100.0) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one(rgb: [u8; 3], render: Render) -> Entry {
        entries(&[rgb], render)[0]
    }

    /// The lift on its own, with the black point out of the way.
    fn lifting() -> Render {
        Render {
            black_level: 0,
            ..Default::default()
        }
    }

    #[test]
    fn black_stays_black() {
        // The whole point of the floor: a dark scene must not be lifted into a glow.
        for render in [Render::default(), lifting()] {
            assert_eq!(one([0, 0, 0], render), (0, 0, 0));
        }
    }

    #[test]
    fn a_camera_that_never_reads_black_is_pulled_down_to_it() {
        // What a camera with auto-exposure does to a black scene: it raises gain
        // until "black" is comfortably above zero, and the strip then never dims.
        for level in [12, 20, 35] {
            let render = Render {
                black_level: level,
                ..Default::default()
            };
            let nearly_black = [level, level, level];
            assert_eq!(
                one(nearly_black, render),
                (0, 0, 0),
                "{level} should be treated as black"
            );
        }
    }

    #[test]
    fn the_black_point_does_not_dim_what_is_bright() {
        // White has to survive, or every field would lose brightness to the fixing
        // of the darkest one.
        let render = Render {
            black_level: 30,
            ..Default::default()
        };
        assert_eq!(one([255, 255, 255], render), (100, 0, 0));
    }

    #[test]
    fn a_dark_zone_keeps_its_colour_but_is_not_given_a_bright_one() {
        // The sample from the field report: a dim olive that the firmware
        // rendered as near-white. The colour comes back, but not at the full
        // multiplier: at a lightness of 38 the zone is dim enough that a fully
        // saturated colour reads as a glow rather than as part of a picture.
        let rgb = [98, 94, 69];
        let (lightness, hue, saturation) = one(rgb, lifting());

        // Hue is never touched. What the camera saw is what gets described, and
        // only the two components the firmware renders badly are rescaled.
        assert_eq!(hue, rgb_to_hsi(rgb[0], rgb[1], rgb[2]).hue);
        assert!(
            saturation > 30,
            "the lift restores colour, got {saturation}"
        );
        assert!(saturation < 60, "but stops short of the full multiplier");
        assert_eq!(lightness, 50, "38 is lifted to 50");
    }

    #[test]
    fn a_nearly_black_zone_is_given_no_colour_at_all() {
        // What a black screen actually measures: three channels a few levels
        // apart, so the hue is the hue of noise. Boosted by the saturation gain
        // and rendered at the zone's lightness, that is exactly the defect this
        // whole feature exists to avoid — a rainbow in a dark room.
        for noise in [[22, 19, 26], [18, 24, 20], [30, 22, 22]] {
            let (lightness, _, saturation) = one(noise, lifting());
            assert!(lightness > 0, "the zone keeps the lightness it measured");
            assert!(
                saturation <= 10,
                "{noise:?} must not become a colour, got saturation {saturation}"
            );
        }
    }

    #[test]
    fn a_saturated_primary_comes_out_unchanged() {
        assert_eq!(one([255, 0, 0], lifting()), (100, 0, 100));
        assert_eq!(one([0, 255, 0], lifting()), (100, 120, 100));
        assert_eq!(one([0, 0, 255], lifting()), (100, 240, 100));
    }

    #[test]
    fn greys_stay_grey() {
        // Saturation has nothing to amplify, so the lift cannot invent a colour.
        let (_, hue, saturation) = one([128, 128, 128], lifting());
        assert_eq!((hue, saturation), (0, 0));
    }

    #[test]
    fn a_gain_of_one_and_no_floor_leaves_hue_and_lightness_alone() {
        let neutral = Render {
            black_level: 0,
            saturation_gain: 1.0,
            lightness_gain: 1.0,
            lightness_floor: 0,
        };
        for rgb in [[98, 94, 69], [255, 128, 0], [12, 200, 40], [0, 0, 0]] {
            let hsi = rgb_to_hsi(rgb[0], rgb[1], rgb[2]);
            let (lightness, hue, _) = one(rgb, neutral);
            // Saturation is not part of this promise: a dark zone is held back
            // whatever the tuning says, because that rule is about noise rather
            // than about taste.
            assert_eq!((lightness, hue), (hsi.lightness, hsi.hue));
        }
    }
}
