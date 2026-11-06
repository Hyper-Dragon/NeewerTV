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

/// How a reading is pushed towards what the firmware renders well.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Render {
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
    let hsi = rgb_to_hsi(red, green, blue);

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

    #[test]
    fn black_stays_black() {
        // The whole point of the floor: a dark scene must not be lifted into a glow.
        let entry = one([0, 0, 0], Render::default());
        assert_eq!(entry, (0, 0, 0));
    }

    #[test]
    fn a_dark_zone_is_lifted_out_of_the_dead_zone_and_its_colour_restored() {
        // The sample from the field report: a dim olive that the firmware
        // rendered as near-white.
        let rgb = [98, 94, 69];
        let (lightness, hue, saturation) = one(rgb, Render::default());

        // Hue is never touched. What the camera saw is what gets described, and
        // only the two components the firmware renders badly are rescaled.
        assert_eq!(hue, rgb_to_hsi(rgb[0], rgb[1], rgb[2]).hue);
        assert_eq!(saturation, 60, "30 becomes 60");
        assert_eq!(lightness, 50, "38 is lifted to 50");
    }

    #[test]
    fn a_saturated_primary_comes_out_unchanged() {
        assert_eq!(one([255, 0, 0], Render::default()), (100, 0, 100));
        assert_eq!(one([0, 255, 0], Render::default()), (100, 120, 100));
        assert_eq!(one([0, 0, 255], Render::default()), (100, 240, 100));
    }

    #[test]
    fn greys_stay_grey() {
        // Saturation has nothing to amplify, so the lift cannot invent a colour.
        let (lightness, hue, saturation) = one([128, 128, 128], Render::default());
        assert_eq!((hue, saturation), (0, 0));
        assert_eq!(lightness, 67);
    }

    #[test]
    fn a_gain_of_one_and_no_floor_is_the_identity() {
        let neutral = Render {
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
