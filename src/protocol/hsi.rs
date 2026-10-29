//! Colour conversion.
//!
//! The device does not take RGB. It takes a hue in degrees, a saturation and a
//! lightness, each 0–100, and every RGB value a caller supplies has to be
//! converted before it goes on the wire.
//!
//! # The lightness trap
//!
//! The protocol calls the third component *lightness*, but it does not behave
//! like HSL lightness or like HSI intensity. It behaves like **HSV value**: it is
//! the largest channel, not the mean. `(255, 0, 0)` is fully light and fully
//! saturated, which is only true under the value interpretation.
//!
//! An earlier implementation pinned this component to 100 for every segment,
//! which made a black segment render as a lit one and turned any per-segment
//! gradient into a wash. The tests below pin the corrected behaviour, including
//! the exact three-colour case that exposed it.

/// A colour in the device's own terms.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hsi {
    /// Degrees, 0–360.
    pub hue: u16,
    /// 0–100.
    pub saturation: u8,
    /// 0–100. Behaves like HSV value, not HSL lightness.
    pub lightness: u8,
}

/// Convert an RGB triple to the device's colour space.
///
/// Black has no hue and no saturation, so it converts to all zeroes rather than
/// to an arbitrary hue at zero lightness — otherwise a "black" segment would
/// carry a colour it cannot show and would render the moment its lightness rose.
#[must_use]
pub fn rgb_to_hsi(r: u8, g: u8, b: u8) -> Hsi {
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let delta = u32::from(max - min);

    // Nothing to describe: no hue, nothing to saturate, nothing lit.
    if max == 0 {
        return Hsi {
            hue: 0,
            saturation: 0,
            lightness: 0,
        };
    }

    let max_wide = u32::from(max);
    // Rounded, not truncated: a channel at full must report 100, not 99.
    let lightness = ((max_wide * 100 + 127) / 255) as u8;
    let saturation = if delta == 0 {
        0
    } else {
        ((delta * 100 + max_wide / 2) / max_wide) as u8
    };

    Hsi {
        hue: hue_degrees(r, g, b, max, delta),
        saturation,
        lightness,
    }
}

/// Hue in degrees, from which channel dominates.
fn hue_degrees(r: u8, g: u8, b: u8, max: u8, delta: u32) -> u16 {
    if delta == 0 {
        // Every channel equal: a grey has no meaningful hue, and inventing one
        // would make two identical-looking greys send different packets.
        return 0;
    }

    let (rf, gf, bf) = (f64::from(r), f64::from(g), f64::from(b));
    let delta = f64::from(delta);

    let hue = if max == r {
        60.0 * ((gf - bf) / delta).rem_euclid(6.0)
    } else if max == g {
        60.0 * ((bf - rf) / delta + 2.0)
    } else {
        60.0 * ((rf - gf) / delta + 4.0)
    };

    hue.round().clamp(0.0, 360.0) as u16
}

/// Scale a Home Assistant brightness (`0–255`) to the device's units (`0–1000`).
///
/// Rounded, not truncated: truncation would make 255 land on 999 and leave the
/// strip permanently one unit short of full.
#[must_use]
pub fn brightness_255_to_units(brightness: u8) -> u16 {
    // The result never exceeds 1000, so the narrowing cast is safe by
    // construction rather than by luck: 255 * 1000 / 255 = 1000.
    ((u32::from(brightness) * 1000 + 127) / 255) as u16
}

/// Scale a percentage brightness (`0–100`) to device units (`0–1000`).
///
/// Returns `None` for anything outside 0–100, including NaN and infinity — the
/// caller turns that into a rejection rather than a clamp, because a caller
/// sending 150 has a bug that a clamp would hide.
#[must_use]
pub fn brightness_pct_to_units(percent: f64) -> Option<u16> {
    if !(0.0..=100.0).contains(&percent) {
        return None;
    }
    // 33.3 * 10 is 332.99999999999994 in binary floating point, so rounding has
    // to happen on the product rather than by truncating it.
    Some((percent * 10.0).round() as u16)
}

/// Convert the device's colour space back to RGB.
///
/// Needed to report colour to Home Assistant: the daemon sends HSI and the state
/// payload is RGB, so the round trip has to happen somewhere. It is lossy at low
/// lightness — `(hue, sat, 5)` and `(hue, sat, 6)` can land on the same byte —
/// which is a property of the device's 0–100 resolution, not of this function.
#[must_use]
pub fn hsi_to_rgb(colour: Hsi) -> (u8, u8, u8) {
    let value = f64::from(colour.lightness) / 100.0;
    let saturation = f64::from(colour.saturation) / 100.0;
    // 360 is the same direction as 0, and anything larger must wrap rather than
    // index past the end of the last sector.
    let hue = f64::from(colour.hue % 360);

    let chroma = value * saturation;
    let sector = hue / 60.0;
    let second = chroma * (1.0 - ((sector % 2.0) - 1.0).abs());
    let base = value - chroma;

    let (r, g, b) = match sector as u32 {
        0 => (chroma, second, 0.0),
        1 => (second, chroma, 0.0),
        2 => (0.0, chroma, second),
        3 => (0.0, second, chroma),
        4 => (second, 0.0, chroma),
        // 5, and the 6 that a hue of exactly 360 produces before the modulo
        // above — both are the magenta-to-red sector.
        _ => (chroma, 0.0, second),
    };

    let scale = |channel: f64| ((channel + base) * 255.0).round().clamp(0.0, 255.0) as u8;
    (scale(r), scale(g), scale(b))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn black_is_dark_and_colourless() {
        assert_eq!(
            rgb_to_hsi(0, 0, 0),
            Hsi {
                hue: 0,
                saturation: 0,
                lightness: 0
            }
        );
    }

    #[test]
    fn white_is_fully_light_and_has_no_saturation() {
        let white = rgb_to_hsi(255, 255, 255);
        assert_eq!(white.lightness, 100);
        assert_eq!(white.saturation, 0);
    }

    #[test]
    fn greys_carry_no_hue_and_no_saturation() {
        for level in [1_u8, 64, 128, 200, 254] {
            let grey = rgb_to_hsi(level, level, level);
            assert_eq!(grey.saturation, 0, "grey at {level}");
            assert_eq!(grey.hue, 0, "grey at {level}");
        }
    }

    #[test]
    fn the_primaries_land_on_their_textbook_hues() {
        assert_eq!(rgb_to_hsi(255, 0, 0).hue, 0, "red");
        assert_eq!(rgb_to_hsi(0, 255, 0).hue, 120, "green");
        assert_eq!(rgb_to_hsi(0, 0, 255).hue, 240, "blue");
        assert_eq!(rgb_to_hsi(255, 255, 0).hue, 60, "yellow");
        assert_eq!(rgb_to_hsi(0, 255, 255).hue, 180, "cyan");
        assert_eq!(rgb_to_hsi(255, 0, 255).hue, 300, "magenta");
    }

    #[test]
    fn a_primary_is_fully_saturated_and_fully_light() {
        // This is the assertion that pins the component to HSV value rather
        // than a mean: under HSL or HSI, pure red would not be 100 % light.
        let red = rgb_to_hsi(255, 0, 0);
        assert_eq!(red.saturation, 100);
        assert_eq!(red.lightness, 100);
    }

    #[test]
    fn lightness_follows_the_largest_channel() {
        assert_eq!(rgb_to_hsi(255, 0, 0).lightness, 100);
        assert_eq!(rgb_to_hsi(128, 0, 0).lightness, 50);
        assert_eq!(rgb_to_hsi(64, 0, 0).lightness, 25);
    }

    #[test]
    fn hue_is_interpolated_between_primary_axes() {
        // Halfway from red towards green.
        assert_eq!(rgb_to_hsi(255, 128, 0).hue, 30);
        // A colour past the vertical axis still reports a hue in 0..=360.
        assert!(rgb_to_hsi(10, 128, 200).hue <= 360);
    }

    #[test]
    fn the_per_segment_lightness_defect_is_fixed() {
        // The exact case that exposed it: one red, one black, one blue. The
        // buggy implementation produced 100, 100, 100 and rendered the black
        // segment lit. It must produce 100, 0, 100.
        let lightnesses: Vec<u8> = [(255_u8, 0_u8, 0_u8), (0, 0, 0), (0, 0, 255)]
            .into_iter()
            .map(|(r, g, b)| rgb_to_hsi(r, g, b).lightness)
            .collect();
        assert_eq!(lightnesses, [100, 0, 100]);
    }

    #[test]
    fn brightness_reaches_full_at_the_top_of_the_range() {
        assert_eq!(brightness_255_to_units(0), 0);
        assert_eq!(brightness_255_to_units(255), 1000);
    }

    #[test]
    fn brightness_rounds_rather_than_truncating() {
        // 128 * 1000 / 255 = 501.96, which must not become 501.
        assert_eq!(brightness_255_to_units(128), 502);
        // 1 * 1000 / 255 = 3.92.
        assert_eq!(brightness_255_to_units(1), 4);
    }

    #[test]
    fn percentage_brightness_converts_at_the_endpoints() {
        assert_eq!(brightness_pct_to_units(0.0), Some(0));
        assert_eq!(brightness_pct_to_units(50.0), Some(500));
        assert_eq!(brightness_pct_to_units(100.0), Some(1000));
    }

    #[test]
    fn percentage_brightness_handles_fractions() {
        assert_eq!(brightness_pct_to_units(33.3), Some(333));
        assert_eq!(brightness_pct_to_units(99.9), Some(999));
    }

    #[test]
    fn percentage_brightness_outside_the_range_is_rejected_not_clamped() {
        // Clamping here would make "I asked for 150 %" indistinguishable from
        // "I asked for 100 %", which is exactly the kind of quiet lie that takes
        // an afternoon to find.
        assert_eq!(brightness_pct_to_units(150.0), None);
        assert_eq!(brightness_pct_to_units(-1.0), None);
        assert_eq!(brightness_pct_to_units(f64::NAN), None);
        assert_eq!(brightness_pct_to_units(f64::INFINITY), None);
    }

    #[test]
    fn black_round_trips_to_black() {
        assert_eq!(hsi_to_rgb(rgb_to_hsi(0, 0, 0)), (0, 0, 0));
    }

    #[test]
    fn white_round_trips_to_white() {
        assert_eq!(hsi_to_rgb(rgb_to_hsi(255, 255, 255)), (255, 255, 255));
    }

    #[test]
    fn the_primaries_round_trip_exactly() {
        for (r, g, b) in [(255_u8, 0_u8, 0_u8), (0, 255, 0), (0, 0, 255)] {
            assert_eq!(hsi_to_rgb(rgb_to_hsi(r, g, b)), (r, g, b), "{r},{g},{b}");
        }
    }

    #[test]
    fn saturated_colours_round_trip_within_a_byte_or_two() {
        // The device stores percentages, so exact round trips are not promised
        // for anything in between. Close is the contract.
        for (r, g, b) in [(255_u8, 128_u8, 0_u8), (18, 52, 86), (200, 40, 200)] {
            let (r2, g2, b2) = hsi_to_rgb(rgb_to_hsi(r, g, b));
            assert!(
                r.abs_diff(r2) <= 3 && g.abs_diff(g2) <= 3 && b.abs_diff(b2) <= 3,
                "({r},{g},{b}) came back as ({r2},{g2},{b2})"
            );
        }
    }

    #[test]
    fn a_hue_past_the_end_wraps_rather_than_overflowing() {
        // 360 is the same direction as 0, and any larger value must not panic.
        let at_zero = hsi_to_rgb(Hsi {
            hue: 0,
            saturation: 100,
            lightness: 100,
        });
        let at_full = hsi_to_rgb(Hsi {
            hue: 360,
            saturation: 100,
            lightness: 100,
        });
        assert_eq!(at_zero, at_full);
        assert_eq!(
            hsi_to_rgb(Hsi {
                hue: 720,
                saturation: 100,
                lightness: 100
            }),
            at_zero
        );
    }
}
