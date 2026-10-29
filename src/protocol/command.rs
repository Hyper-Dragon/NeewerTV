//! Host-to-device commands.
//!
//! Every builder returns the complete frame, checksum included, so a caller
//! cannot forget to finish it. Values are validated rather than clamped: a
//! brightness of 4000 is a bug in the caller, and silently sending 1000 hides it
//! until somebody notices the slider does not match the strip.
//!
//! Numeric conventions: brightness is 0–1000 sent as `(value / 10, value % 10)`;
//! hue is 0–360 as a big-endian `u16`; saturation and lightness are 0–100.

use super::ProtocolError;
use super::frame;

pub const ID_QUERY_STATE: u8 = 0x08;
pub const ID_STATE_REPORT: u8 = 0x09;
pub const ID_POWER: u8 = 0x0A;
pub const ID_BRIGHTNESS: u8 = 0x0B;
pub const ID_COLOUR_TEMPERATURE: u8 = 0x0C;
pub const ID_COLOUR: u8 = 0x0D;
pub const ID_MUSIC: u8 = 0x0E;
pub const ID_DISPLAY: u8 = 0x0F;
pub const ID_ACK: u8 = 0x10;
pub const ID_SLEEP_TIMER: u8 = 0x40;
pub const ID_TIMER_REPORT: u8 = 0x41;
pub const ID_TIMER_CANCEL: u8 = 0x42;
pub const ID_TIMER_QUERY: u8 = 0x43;

pub const BRIGHTNESS_MAX: u16 = 1000;
pub const HUE_MAX: u16 = 360;
pub const PERCENT_MAX: u8 = 100;
pub const SEGMENT_MAX: usize = 20;

/// Power. `data[0]` is `0x01` on, `0x02` off.
pub fn power(on: bool) -> Vec<u8> {
    frame::short(ID_POWER, &[if on { 0x01 } else { 0x02 }])
}

/// Split a 0–1000 brightness into its tens and units bytes.
fn brightness_pair(level: u16) -> Result<(u8, u8), ProtocolError> {
    if level > BRIGHTNESS_MAX {
        return Err(ProtocolError::Brightness(level));
    }
    Ok(((level / 10) as u8, (level % 10) as u8))
}

fn check_hue(hue: u16) -> Result<(), ProtocolError> {
    if hue > HUE_MAX {
        return Err(ProtocolError::Hue(hue));
    }
    Ok(())
}

fn check_percent(value: u8, make: fn(u8) -> ProtocolError) -> Result<(), ProtocolError> {
    if value > PERCENT_MAX {
        return Err(make(value));
    }
    Ok(())
}

/// Brightness, 0–1000. `data[0]` is reserved and must be zero.
pub fn brightness(level: u16) -> Result<Vec<u8>, ProtocolError> {
    let (tens, units) = brightness_pair(level)?;
    Ok(frame::short(ID_BRIGHTNESS, &[0x00, tens, units]))
}

/// Single colour, hue 0–360 and saturation 0–100, at the given brightness.
///
/// The trailing constant bytes belong to the vendor client's construction; the
/// device ignores `panelIndex` on these models, so it is always zero.
pub fn colour(hue: u16, saturation: u8, level: u16) -> Result<Vec<u8>, ProtocolError> {
    check_hue(hue)?;
    check_percent(saturation, ProtocolError::Saturation)?;
    let (tens, units) = brightness_pair(level)?;

    let data = [
        tens,
        units,
        0x01, // lightColorType: single colour
        0x64, // constant 100, as the vendor client sends it
        (hue >> 8) as u8,
        (hue & 0xFF) as u8,
        saturation,
        0x00,
        0x01,
        0x00, // panelIndex
    ];
    Ok(frame::long(ID_COLOUR, &data))
}

/// One colour per segment: `(lightness, hue, saturation)`.
///
/// Between 1 and 20 entries. Fewer than 20 entries are distributed across the
/// whole strip with the device interpolating between them; 20 maps one to one.
pub fn segments(entries: &[(u8, u16, u8)], level: u16) -> Result<Vec<u8>, ProtocolError> {
    if entries.is_empty() || entries.len() > SEGMENT_MAX {
        return Err(ProtocolError::SegmentCount(entries.len()));
    }
    let (tens, units) = brightness_pair(level)?;
    let count = entries.len() as u16;

    let mut data = Vec::with_capacity(8 + entries.len() * 4);
    data.extend_from_slice(&[tens, units, 0x02]); // lightColorType: segmented
    data.extend_from_slice(&count.to_be_bytes());
    for &(lightness, hue, sat) in entries {
        check_percent(lightness, ProtocolError::Lightness)?;
        check_hue(hue)?;
        check_percent(sat, ProtocolError::Saturation)?;
        data.extend_from_slice(&[lightness, (hue >> 8) as u8, (hue & 0xFF) as u8, sat]);
    }
    data.extend_from_slice(&[0x00, 0x01, 0x00]); // trailing constants, panelIndex

    Ok(frame::long(ID_COLOUR, &data))
}

/// Music-reactive mode. Brightness is sent as `level / 10` only — there is no
/// units digit in this command, so the bottom of the range is lost.
pub fn music(
    active: bool,
    level: u16,
    mode_id: u8,
    sensitivity: u8,
) -> Result<Vec<u8>, ProtocolError> {
    let (tens, _) = brightness_pair(level)?;
    check_percent(sensitivity, ProtocolError::Saturation)?;

    let data = [
        0x01, // constant
        u8::from(active),
        tens,
        mode_id,
        0x64, // constant 100
        sensitivity,
        0x00,
        0x01,
        0x00,
        0x00,
        0x00,
    ];
    Ok(frame::short(ID_MUSIC, &data))
}

/// The display engine: presets, scenes and DIY patterns all go through here.
///
/// `style` is `0x00` full-spectrum or `0x01` segment; `expand` widens the
/// animation and depends on the pattern.
pub fn effect(
    model: u8,
    level: u16,
    speed: u8,
    style: u8,
    direction: u8,
    expand: u8,
    colours: &[(u8, u16, u8)],
) -> Result<Vec<u8>, ProtocolError> {
    if colours.is_empty() || colours.len() > SEGMENT_MAX {
        return Err(ProtocolError::SegmentCount(colours.len()));
    }
    let (tens, units) = brightness_pair(level)?;
    check_percent(speed, ProtocolError::Speed)?;

    let mut data = Vec::with_capacity(13 + colours.len() * 4);
    data.extend_from_slice(&[
        tens,
        units,
        0x01,
        0x01,
        0x00,
        0x00,
        0x00,
        model,
        speed,
        style,
        direction,
        expand,
        colours.len() as u8,
    ]);
    for &(lightness, hue, sat) in colours {
        check_percent(lightness, ProtocolError::Lightness)?;
        check_hue(hue)?;
        check_percent(sat, ProtocolError::Saturation)?;
        data.extend_from_slice(&[lightness, (hue >> 8) as u8, (hue & 0xFF) as u8, sat]);
    }

    Ok(frame::long(ID_DISPLAY, &data))
}

/// A display preset: full spectrum, direction zero, expand 50, one white record.
pub fn preset(model: u8, level: u16, speed: u8) -> Result<Vec<u8>, ProtocolError> {
    // Expand 50 and this exact colour record are what the vendor client sends
    // for a preset; the values are not derived from anything, they are observed.
    effect(model, level, speed, 0x00, 0x00, 0x32, &[(100, 0, 63)])
}

/// Query device state. `all` asks for every mode's parameters rather than only
/// the active one.
pub fn query_state(all: bool) -> Vec<u8> {
    frame::short(ID_QUERY_STATE, &[if all { 0x00 } else { 0x01 }])
}

/// Device-side countdown. The device acts on expiry with or without us.
pub fn sleep_timer(seconds: u32, light_on_expiry: bool) -> Vec<u8> {
    let mut data = Vec::with_capacity(5);
    data.extend_from_slice(&seconds.to_be_bytes());
    data.push(u8::from(light_on_expiry));
    frame::short(ID_SLEEP_TIMER, &data)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 0x7A + 0x0A + 0x01 + 0x01
    #[test]
    fn power_on_matches_the_worked_example() {
        assert_eq!(power(true), [0x7A, 0x0A, 0x01, 0x01, 0x86]);
    }

    #[test]
    fn power_off_is_the_same_frame_with_a_two() {
        assert_eq!(power(false), [0x7A, 0x0A, 0x01, 0x02, 0x87]);
    }

    #[test]
    fn brightness_splits_into_tens_and_units() {
        // 80 % is 800 units -> (0x50, 0x00) behind a zero byte.
        assert_eq!(
            brightness(800).unwrap(),
            [0x7A, 0x0B, 0x03, 0x00, 0x50, 0x00, 0xD8]
        );
    }

    #[test]
    fn brightness_handles_both_ends_of_the_range() {
        assert_eq!(
            brightness(0).unwrap(),
            [0x7A, 0x0B, 0x03, 0x00, 0x00, 0x00, 0x88]
        );
        assert_eq!(
            brightness(1000).unwrap(),
            [0x7A, 0x0B, 0x03, 0x00, 0x64, 0x00, 0xEC]
        );
        // A units digit that is not zero must survive: 999 -> (0x63, 0x09).
        assert_eq!(
            brightness(999).unwrap(),
            [0x7A, 0x0B, 0x03, 0x00, 0x63, 0x09, 0xF4]
        );
    }

    #[test]
    fn brightness_rejects_values_past_the_maximum() {
        assert_eq!(brightness(1001), Err(ProtocolError::Brightness(1001)));
    }

    #[test]
    fn single_colour_matches_the_documented_example() {
        assert_eq!(
            colour(240, 100, 800).unwrap(),
            [
                0x7A, 0x0D, 0x00, 0x0A, 0x50, 0x00, 0x01, 0x64, 0x00, 0xF0, 0x64, 0x00, 0x01, 0x00,
                0x9B
            ]
        );
    }

    #[test]
    fn single_colour_rejects_out_of_range_hue_and_saturation() {
        assert_eq!(colour(361, 100, 800), Err(ProtocolError::Hue(361)));
        assert_eq!(colour(240, 101, 800), Err(ProtocolError::Saturation(101)));
    }

    #[test]
    fn two_segments_match_the_documented_example() {
        let entries = [(100, 0, 100), (100, 240, 100)];
        assert_eq!(
            segments(&entries, 1000).unwrap(),
            [
                0x7A, 0x0D, 0x00, 0x10, 0x64, 0x00, 0x02, 0x00, 0x02, 0x64, 0x00, 0x00, 0x64, 0x64,
                0x00, 0xF0, 0x64, 0x00, 0x01, 0x00, 0x80
            ]
        );
    }

    #[test]
    fn segments_accept_the_full_twenty_and_reject_more() {
        let twenty = [(100u8, 0u16, 100u8); 20];
        let frame = segments(&twenty, 1000).unwrap();
        // 8 fixed bytes plus 4 per colour record, in the long form.
        assert_eq!(frame.len(), 4 + 8 + 80 + 1);

        let twenty_one = [(100u8, 0u16, 100u8); 21];
        assert_eq!(
            segments(&twenty_one, 1000),
            Err(ProtocolError::SegmentCount(21))
        );
        assert_eq!(segments(&[], 1000), Err(ProtocolError::SegmentCount(0)));
    }

    #[test]
    fn music_matches_the_documented_example() {
        assert_eq!(
            music(false, 600, 0, 50).unwrap(),
            [
                0x7A, 0x0E, 0x0B, 0x01, 0x00, 0x3C, 0x00, 0x64, 0x32, 0x00, 0x01, 0x00, 0x00, 0x00,
                0x67
            ]
        );
    }

    #[test]
    fn preset_matches_the_superfluid_example() {
        assert_eq!(
            preset(0xFC, 600, 50).unwrap(),
            [
                0x7A, 0x0F, 0x00, 0x11, 0x3C, 0x00, 0x01, 0x01, 0x00, 0x00, 0x00, 0xFC, 0x32, 0x00,
                0x00, 0x32, 0x01, 0x64, 0x00, 0x00, 0x3F, 0xDC
            ]
        );
    }

    #[test]
    fn effect_carries_every_parameter_in_its_documented_position() {
        let frame = effect(0x01, 1000, 0, 0x01, 0x01, 0x02, &[(50, 360, 25)]).unwrap();
        let data = &frame[4..frame.len() - 1];
        assert_eq!(&data[..2], &[0x64, 0x00], "brightness");
        assert_eq!(data[7], 0x01, "model");
        assert_eq!(data[8], 0x00, "speed");
        assert_eq!(data[9], 0x01, "style");
        assert_eq!(data[10], 0x01, "direction");
        assert_eq!(data[11], 0x02, "expand");
        assert_eq!(data[12], 0x01, "colour count");
        assert_eq!(&data[13..], &[50, 0x01, 0x68, 25], "colour record");
    }

    #[test]
    fn effect_rejects_an_out_of_range_hue_in_a_colour_record() {
        assert_eq!(
            effect(0x01, 800, 0, 0, 0, 1, &[(100, 400, 100)]),
            Err(ProtocolError::Hue(400))
        );
    }

    #[test]
    fn state_query_asks_for_every_mode() {
        assert_eq!(query_state(true), [0x7A, 0x08, 0x01, 0x00, 0x83]);
    }

    #[test]
    fn state_query_can_ask_for_the_active_mode_only() {
        assert_eq!(query_state(false), [0x7A, 0x08, 0x01, 0x01, 0x84]);
    }

    #[test]
    fn one_hour_timer_matches_the_documented_example() {
        assert_eq!(
            sleep_timer(3600, false),
            [0x7A, 0x40, 0x05, 0x00, 0x00, 0x0E, 0x10, 0x00, 0xDD]
        );
    }

    #[test]
    fn timer_sets_the_on_flag_for_light_on_at_expiry() {
        assert_eq!(
            sleep_timer(60, true),
            [0x7A, 0x40, 0x05, 0x00, 0x00, 0x00, 0x3C, 0x01, 0xFC]
        );
    }
}
