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

use crate::protocol::hsi::rgb_to_hsi;

/// One segment colour, in the order the wire wants it.
pub type Entry = (u8, u16, u8);

pub const DEFAULT_SATURATION_GAIN: f32 = 2.0;
pub const DEFAULT_LIGHTNESS_GAIN: f32 = 1.4;
pub const DEFAULT_LIGHTNESS_FLOOR: u8 = 8;
pub const DEFAULT_BLACK_LEVEL: u8 = 10;

/// How a reading is pushed towards what the firmware renders well.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Render {
    /// Every channel at or below this is treated as black, and the rest is
    /// rescaled so white stays white.
    ///
    /// A camera that has not had its exposure locked does not report black as
    /// zero: aimed at a dark scene it raises gain until the screen reads as mid
    /// grey, and the strip then stays lit through the whole film. Set this to the
    /// value the swatches show while the screen is black.
    pub black_level: u8,
    /// Multiplier on saturation. The firmware renders low saturation as white.
    pub saturation_gain: f32,
    /// Multiplier on the part of lightness above [`Self::lightness_floor`].
    pub lightness_gain: f32,
    /// Lightness at or below this passes through untouched, so black stays black.
    pub lightness_floor: u8,
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

    let saturation = scale(u16::from(hsi.saturation), render.saturation_gain);

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
    fn a_dark_zone_is_lifted_out_of_the_dead_zone_and_its_colour_restored() {
        // The sample from the field report: a dim olive that the firmware
        // rendered as near-white.
        let rgb = [98, 94, 69];
        let (lightness, hue, saturation) = one(rgb, lifting());

        // Hue is never touched. What the camera saw is what gets described, and
        // only the two components the firmware renders badly are rescaled.
        assert_eq!(hue, rgb_to_hsi(rgb[0], rgb[1], rgb[2]).hue);
        assert_eq!(saturation, 60, "30 becomes 60");
        assert_eq!(lightness, 50, "38 is lifted to 50");
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
    fn a_gain_of_one_and_no_floor_is_the_identity() {
        let neutral = Render {
            black_level: 0,
            saturation_gain: 1.0,
            lightness_gain: 1.0,
            lightness_floor: 0,
        };
        for rgb in [[98, 94, 69], [255, 128, 0], [12, 200, 40], [0, 0, 0]] {
            let hsi = rgb_to_hsi(rgb[0], rgb[1], rgb[2]);
            assert_eq!(one(rgb, neutral), (hsi.lightness, hsi.hue, hsi.saturation));
        }
    }
}
