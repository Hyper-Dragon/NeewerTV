//! Turning what a caller asked for into what the strip should do.
//!
//! Home Assistant publishes JSON to `{base}/set` and expects it to work. That
//! payload is the compatibility contract: a `json` schema light sends
//! `state`/`brightness`/`color`/`effect`, and we accept a few extensions on top
//! (`segments`, `speed`). Nothing here may change shape without being a
//! deliberate breaking change.
//!
//! # Resolution order
//!
//! A payload can carry several things at once, so they are resolved in a fixed
//! order and the most specific wins: screen sync, then a display preset, then
//! music, then a scene, then a DIY pattern, then explicit segments, then a
//! colour, then brightness alone. `state` is not part of that ordering — it is
//! orthogonal and is always emitted first, so a payload that both turns the
//! strip on and sets a colour does what it says.

use serde::Deserialize;

use crate::homeassistant;
use crate::protocol::ProtocolError;
use crate::protocol::catalogue::{self, Effect, Kind};
use crate::protocol::command as wire;
use crate::protocol::command::SEGMENT_MAX;
use crate::protocol::hsi::{self, Hsi};

/// A resolved instruction, ready to be turned into frames.
#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    Power(bool),
    Brightness(u16),
    Colour {
        colour: Hsi,
        brightness: u16,
    },
    Segments {
        /// `(lightness, hue, saturation)` per segment.
        entries: Vec<(u8, u16, u8)>,
        brightness: u16,
    },
    Effect {
        effect: &'static Effect,
        brightness: u16,
        speed: u8,
    },
    Music {
        mode_id: u8,
        brightness: u16,
        sensitivity: u8,
    },
}

/// Why a payload could not be turned into commands.
///
/// Each variant names the field and the value, because the only way a caller can
/// fix a bad payload is to be told which part was bad.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum CommandError {
    #[error("state {0:?} is not ON or OFF")]
    State(String),

    #[error("brightness {0} is outside 0..=255")]
    Brightness(f64),

    #[error("brightness_pct {0} is outside 0..=100")]
    BrightnessPct(f64),

    #[error("effect {0:?} is not in the catalogue")]
    UnknownEffect(String),

    #[error("speed {0} is outside 0..=100")]
    Speed(f64),

    #[error("{0} segments is outside 1..=20")]
    SegmentCount(usize),

    #[error("segment {index} has {field} {value}, outside its range")]
    SegmentField {
        index: usize,
        field: &'static str,
        value: i64,
    },
}

/// The JSON body of a `{base}/set` message.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct SetPayload {
    pub state: Option<String>,
    pub brightness: Option<f64>,
    pub brightness_pct: Option<f64>,
    pub color: Option<Rgb>,
    pub effect: Option<String>,
    pub speed: Option<f64>,
    pub segments: Option<Segments>,
}

/// RGB as Home Assistant sends it.
#[derive(Debug, Clone, Copy, Deserialize)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

/// Per-segment colour, in either of the two shapes a caller may use.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum Segments {
    /// `[[r, g, b], …]`
    Rgb(Vec<[u8; 3]>),
    /// `[{"hue": h, "sat": s, "lightness": l}, …]`
    Hsi(Vec<HsiEntry>),
}

/// One explicit HSI segment, used verbatim.
#[derive(Debug, Clone, Copy, Deserialize)]
pub struct HsiEntry {
    pub hue: i64,
    pub sat: i64,
    pub lightness: i64,
}

/// Default animation speed when a caller does not supply one: mid-range, which
/// is the only defensible guess for a value with no neutral.
const DEFAULT_SPEED: u8 = 50;

/// Resolve a payload into the commands to send, in order.
///
/// Returns an empty vector for an empty payload rather than an error: Home
/// Assistant occasionally publishes `{}` and the correct response is to do
/// nothing, not to complain.
pub fn resolve(payload: &SetPayload) -> Result<Vec<Command>, CommandError> {
    let mut commands = Vec::new();

    // Power is orthogonal to everything else and always goes first, so that
    // "on, and red" does not briefly show the previous colour.
    if let Some(state) = &payload.state {
        let on = match state.as_str() {
            homeassistant::PAYLOAD_ON => true,
            homeassistant::PAYLOAD_OFF => false,
            other => return Err(CommandError::State(other.to_owned())),
        };
        commands.push(Command::Power(on));
    }

    let brightness = resolve_brightness(payload)?;

    // Most specific instruction wins. An effect that also carried a colour is
    // sent as the effect alone: sending both would be two writes with a visible
    // flicker between them.
    if let Some(name) = &payload.effect {
        let effect =
            catalogue::by_name(name).ok_or_else(|| CommandError::UnknownEffect(name.clone()))?;
        let control = resolve_speed(payload)?;

        commands.push(match effect.kind {
            // Music is its own command, not a display-engine effect.
            Kind::Music => Command::Music {
                mode_id: effect.id,
                brightness,
                sensitivity: control,
            },
            _ => Command::Effect {
                effect,
                brightness,
                speed: control,
            },
        });
        return Ok(commands);
    }

    if let Some(segments) = &payload.segments {
        commands.push(Command::Segments {
            entries: resolve_segments(segments)?,
            brightness,
        });
        return Ok(commands);
    }

    if let Some(rgb) = &payload.color {
        commands.push(Command::Colour {
            colour: hsi::rgb_to_hsi(rgb.r, rgb.g, rgb.b),
            brightness,
        });
        return Ok(commands);
    }

    // Brightness on its own — only when it was actually asked for, so that an
    // empty payload stays empty rather than setting full brightness.
    if payload.brightness.is_some() || payload.brightness_pct.is_some() {
        commands.push(Command::Brightness(brightness));
    }

    Ok(commands)
}

/// Brightness in device units, from whichever form the caller supplied.
///
/// `brightness_pct` is checked by presence rather than by truthiness: an
/// explicit 0 % is a request to be dark, and treating it as absent was the
/// original defect.
fn resolve_brightness(payload: &SetPayload) -> Result<u16, CommandError> {
    if let Some(percent) = payload.brightness_pct {
        return hsi::brightness_pct_to_units(percent).ok_or(CommandError::BrightnessPct(percent));
    }

    if let Some(value) = payload.brightness {
        if !(0.0..=255.0).contains(&value) {
            return Err(CommandError::Brightness(value));
        }
        return Ok(hsi::brightness_255_to_units(value.round() as u8));
    }

    Ok(1000)
}

fn resolve_speed(payload: &SetPayload) -> Result<u8, CommandError> {
    match payload.speed {
        None => Ok(DEFAULT_SPEED),
        Some(value) if (0.0..=100.0).contains(&value) => Ok(value.round() as u8),
        Some(value) => Err(CommandError::Speed(value)),
    }
}

fn resolve_segments(segments: &Segments) -> Result<Vec<(u8, u16, u8)>, CommandError> {
    let entries = match segments {
        Segments::Rgb(triples) => triples
            .iter()
            .map(|[r, g, b]| {
                let colour = hsi::rgb_to_hsi(*r, *g, *b);
                (colour.lightness, colour.hue, colour.saturation)
            })
            .collect(),
        // Already in the device's colour space: used as given, because
        // round-tripping through RGB would lose precision for nothing.
        Segments::Hsi(items) => items
            .iter()
            .enumerate()
            .map(|(index, entry)| {
                let lightness = checked(index, "lightness", entry.lightness, 100)?;
                let hue = checked(index, "hue", entry.hue, 360)?;
                let saturation = checked(index, "sat", entry.sat, 100)?;
                Ok((lightness, hue as u16, saturation))
            })
            .collect::<Result<Vec<_>, CommandError>>()?,
    };

    if entries.is_empty() || entries.len() > SEGMENT_MAX {
        return Err(CommandError::SegmentCount(entries.len()));
    }

    Ok(entries)
}

fn checked(index: usize, field: &'static str, value: i64, max: i64) -> Result<u8, CommandError> {
    if !(0..=max).contains(&value) {
        return Err(CommandError::SegmentField {
            index,
            field,
            value,
        });
    }
    Ok(value as u8)
}

/// Encode a resolved command into the frames to write, in order.
///
/// The frame construction deliberately lives in `protocol`; this only chooses
/// *which* builder to use and supplies the per-family constants.
pub fn frames(command: &Command) -> Result<Vec<Vec<u8>>, ProtocolError> {
    let frame = match command {
        Command::Power(on) => wire::power(*on),

        Command::Brightness(level) => wire::brightness(*level)?,

        Command::Colour { colour, brightness } => {
            wire::colour(colour.hue, colour.saturation, *brightness)?
        }

        Command::Segments {
            entries,
            brightness,
        } => wire::segments(entries, *brightness)?,

        Command::Effect {
            effect,
            brightness,
            speed,
        } => {
            // `expand` is not a free choice: scenes are sent with 1, presets and
            // screen sync with 50. Sending the wrong one produces a perfectly
            // valid frame that shows the wrong animation.
            let expand = match effect.kind {
                Kind::Scene | Kind::Diy => 0x01,
                Kind::Preset | Kind::TwoWay => 0x32,
                // Music never reaches the display engine.
                Kind::Music => 0x01,
            };

            wire::effect(
                effect.id,
                *brightness,
                *speed,
                0x00,
                effect.direction,
                expand,
                // What the vendor client sends alongside every effect. The
                // colour records are what DIY patterns animate over; for scenes
                // and presets the device supplies its own palette.
                &[(100, 0, 63)],
            )?
        }

        Command::Music {
            mode_id,
            brightness,
            sensitivity,
        } => wire::music(true, *brightness, *mode_id, *sensitivity)?,
    };

    Ok(vec![frame])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(json: &str) -> SetPayload {
        serde_json::from_str(json).expect("test payload must be valid JSON")
    }

    fn resolve_json(json: &str) -> Result<Vec<Command>, CommandError> {
        resolve(&parse(json))
    }

    fn frames_json(json: &str) -> Vec<Vec<u8>> {
        resolve_json(json)
            .expect("payload must resolve")
            .iter()
            .flat_map(|command| frames(command).expect("command must encode"))
            .collect()
    }

    #[test]
    fn an_empty_payload_does_nothing() {
        assert_eq!(resolve_json("{}").unwrap(), []);
    }

    #[test]
    fn state_alone_turns_the_strip_on_and_off() {
        assert_eq!(
            resolve_json(r#"{"state":"ON"}"#).unwrap(),
            [Command::Power(true)]
        );
        assert_eq!(
            resolve_json(r#"{"state":"OFF"}"#).unwrap(),
            [Command::Power(false)]
        );
    }

    #[test]
    fn an_unrecognised_state_is_an_error_not_a_silent_no_op() {
        assert_eq!(
            resolve_json(r#"{"state":"on"}"#),
            Err(CommandError::State("on".into()))
        );
    }

    #[test]
    fn brightness_scales_from_255_to_1000_units() {
        assert_eq!(
            resolve_json(r#"{"brightness":128}"#).unwrap(),
            [Command::Brightness(502)]
        );
        assert_eq!(
            resolve_json(r#"{"brightness":255}"#).unwrap(),
            [Command::Brightness(1000)]
        );
    }

    #[test]
    fn brightness_pct_zero_means_off_not_absent() {
        // The original defect: `0.0` is falsy in most languages, so `if not
        // brightness_pct` treated an explicit 0 % as "not supplied" and left the
        // strip at its previous brightness. Zero is a value.
        assert_eq!(
            resolve_json(r#"{"brightness_pct":0}"#).unwrap(),
            [Command::Brightness(0)]
        );
        assert_eq!(
            resolve_json(r#"{"brightness_pct":50}"#).unwrap(),
            [Command::Brightness(500)]
        );
    }

    #[test]
    fn out_of_range_brightness_is_rejected_rather_than_clamped() {
        assert_eq!(
            resolve_json(r#"{"brightness_pct":150}"#),
            Err(CommandError::BrightnessPct(150.0))
        );
        assert_eq!(
            resolve_json(r#"{"brightness":300}"#),
            Err(CommandError::Brightness(300.0))
        );
    }

    #[test]
    fn state_is_emitted_before_the_action_it_accompanies() {
        // Power must be written first, or turning on with a colour leaves the
        // strip briefly showing whatever it had before.
        assert_eq!(
            resolve_json(r#"{"state":"ON","brightness_pct":50}"#).unwrap(),
            [Command::Power(true), Command::Brightness(500)]
        );
    }

    #[test]
    fn an_rgb_colour_is_converted_to_the_devices_colour_space() {
        assert_eq!(
            resolve_json(r#"{"color":{"r":255,"g":0,"b":0}}"#).unwrap(),
            [Command::Colour {
                colour: Hsi {
                    hue: 0,
                    saturation: 100,
                    lightness: 100
                },
                brightness: 1000,
            }]
        );
    }

    #[test]
    fn a_colour_takes_the_brightness_from_the_same_payload() {
        assert_eq!(
            resolve_json(r#"{"color":{"r":0,"g":0,"b":255},"brightness_pct":25}"#).unwrap(),
            [Command::Colour {
                colour: Hsi {
                    hue: 240,
                    saturation: 100,
                    lightness: 100
                },
                brightness: 250,
            }]
        );
    }

    #[test]
    fn per_segment_rgb_is_converted_segment_by_segment() {
        // The three-colour case that exposed the pinned-lightness defect.
        assert_eq!(
            resolve_json(r#"{"segments":[[255,0,0],[0,0,0],[0,0,255]]}"#).unwrap(),
            [Command::Segments {
                entries: vec![(100, 0, 100), (0, 0, 0), (100, 240, 100)],
                brightness: 1000,
            }]
        );
    }

    #[test]
    fn per_segment_hsi_is_used_verbatim() {
        // A caller who has already done the conversion must not have it redone:
        // round-tripping through RGB would lose precision for no benefit.
        assert_eq!(
            resolve_json(r#"{"segments":[{"hue":240,"sat":100,"lightness":50}]}"#).unwrap(),
            [Command::Segments {
                entries: vec![(50, 240, 100)],
                brightness: 1000,
            }]
        );
    }

    #[test]
    fn too_many_segments_is_an_error() {
        let json = format!(r#"{{"segments":[{}]}}"#, vec!["[1,2,3]"; 21].join(","));
        assert_eq!(resolve_json(&json), Err(CommandError::SegmentCount(21)));
    }

    #[test]
    fn an_out_of_range_hsi_segment_names_the_segment_and_the_field() {
        assert_eq!(
            resolve_json(r#"{"segments":[{"hue":400,"sat":100,"lightness":50}]}"#),
            Err(CommandError::SegmentField {
                index: 0,
                field: "hue",
                value: 400
            })
        );
    }

    #[test]
    fn an_effect_is_looked_up_in_the_catalogue() {
        assert_eq!(
            resolve_json(r#"{"effect":"Superfluid"}"#).unwrap(),
            [Command::Effect {
                effect: catalogue::by_name("Superfluid").unwrap(),
                brightness: 1000,
                speed: DEFAULT_SPEED,
            }]
        );
    }

    #[test]
    fn an_effect_can_carry_a_speed() {
        let resolved = resolve_json(r#"{"effect":"DIY Rainbow","speed":80}"#).unwrap();
        assert_eq!(
            resolved,
            [Command::Effect {
                effect: catalogue::by_name("DIY Rainbow").unwrap(),
                brightness: 1000,
                speed: 80,
            }]
        );
    }

    #[test]
    fn an_unknown_effect_is_an_error_naming_it() {
        assert_eq!(
            resolve_json(r#"{"effect":"Definitely Not Real"}"#),
            Err(CommandError::UnknownEffect("Definitely Not Real".into()))
        );
    }

    #[test]
    fn an_out_of_range_speed_is_rejected() {
        assert_eq!(
            resolve_json(r#"{"effect":"Superfluid","speed":150}"#),
            Err(CommandError::Speed(150.0))
        );
    }

    #[test]
    fn the_most_specific_instruction_wins() {
        // A payload carrying an effect and a colour applies the effect: sending
        // both would be two writes and a visible flicker between them.
        let resolved =
            resolve_json(r#"{"effect":"Superfluid","color":{"r":255,"g":0,"b":0}}"#).unwrap();
        assert_eq!(resolved.len(), 1);
        assert!(matches!(resolved[0], Command::Effect { .. }));
    }

    #[test]
    fn music_modes_resolve_to_the_music_command_not_the_display_engine() {
        // `Music Beat` is a music mode, which is a different command entirely
        // from the display engine any other effect uses.
        let resolved = resolve_json(r#"{"effect":"Music Beat"}"#).unwrap();
        assert_eq!(
            resolved,
            [Command::Music {
                mode_id: 2,
                brightness: 1000,
                sensitivity: 50
            }]
        );
    }

    #[test]
    fn brightness_conversion_agrees_with_the_shared_helper() {
        // Guards against the parser growing its own scaling.
        for value in [0_u8, 1, 64, 128, 200, 255] {
            let resolved = resolve_json(&format!(r#"{{"brightness":{value}}}"#)).unwrap();
            assert_eq!(
                resolved,
                [Command::Brightness(hsi::brightness_255_to_units(value))]
            );
        }
    }

    // ── from JSON all the way to bytes ───────────────────────────────────────

    #[test]
    fn power_becomes_the_documented_frame() {
        assert_eq!(
            frames_json(r#"{"state":"ON"}"#),
            vec![[0x7A, 0x0A, 0x01, 0x01, 0x86]]
        );
    }

    #[test]
    fn a_preset_payload_becomes_the_documented_frame() {
        // The whole path in one assertion: JSON in, the exact bytes the vendor
        // client sends for this preset out, checksum included.
        assert_eq!(
            frames_json(r#"{"effect":"Superfluid","brightness_pct":60,"speed":50}"#),
            vec![[
                0x7A, 0x0F, 0x00, 0x11, 0x3C, 0x00, 0x01, 0x01, 0x00, 0x00, 0x00, 0xFC, 0x32, 0x00,
                0x00, 0x32, 0x01, 0x64, 0x00, 0x00, 0x3F, 0xDC
            ]]
        );
    }

    #[test]
    fn a_scene_uses_its_own_expand_value() {
        // Scenes are sent with expand 1, unlike presets which use 50. Getting
        // this wrong sends a valid frame that shows the wrong thing.
        let frames = frames_json(r#"{"effect":"Icelandic Blue","brightness_pct":100,"speed":50}"#);
        assert_eq!(frames.len(), 1);
        let data = &frames[0][4..frames[0].len() - 1];
        assert_eq!(data[7], 0x31, "scene identifier");
        assert_eq!(data[11], 0x01, "scene expand");
    }

    #[test]
    fn a_mirrored_screen_sync_carries_its_direction() {
        let plain = frames_json(r#"{"effect":"TV Screen"}"#);
        let mirrored = frames_json(r#"{"effect":"TV Screen (mirrored)"}"#);
        // Same model, same everything, except direction at data byte 10.
        assert_eq!(plain[0][4 + 7], mirrored[0][4 + 7], "same model");
        assert_eq!(plain[0][4 + 10], 0x00, "direction");
        assert_eq!(mirrored[0][4 + 10], 0x01, "mirrored");
    }

    #[test]
    fn a_music_mode_becomes_the_music_frame_not_a_display_frame() {
        let frames = frames_json(r#"{"effect":"Music Beat","brightness_pct":60}"#);
        assert_eq!(frames.len(), 1);
        let frame = &frames[0];
        assert_eq!(frame[1], 0x0E, "music identifier");
        assert_eq!(frame[2], 0x0B, "short-form length");

        // Music is short-form, so its data starts at index 3 — one earlier than
        // the long-form frames above. Getting this off by one silently reads the
        // wrong field rather than failing.
        let data = &frame[3..frame.len() - 1];
        assert_eq!(data.len(), 11, "eleven data bytes");
        assert_eq!(data[1], 0x01, "music mode active");
        assert_eq!(data[2], 60, "brightness as tens only");
        assert_eq!(data[3], 2, "mode id");
    }

    #[test]
    fn per_segment_colour_becomes_the_segmented_frame() {
        let frames = frames_json(r#"{"segments":[[255,0,0],[0,0,0],[0,0,255]]}"#);
        assert_eq!(frames.len(), 1);
        let frame = &frames[0];
        assert_eq!(frame[1], 0x0D, "colour identifier");
        let data = &frame[4..frame.len() - 1];
        assert_eq!(data[2], 0x02, "segmented form");
        assert_eq!(u16::from_be_bytes([data[3], data[4]]), 3, "three entries");
        // Lightness of each record: red, black, blue.
        assert_eq!(
            [data[5], data[9], data[13]],
            [100, 0, 100],
            "the defect that made black render lit"
        );
    }

    #[test]
    fn a_state_and_an_action_produce_two_frames_in_order() {
        let frames = frames_json(r#"{"state":"ON","brightness_pct":50}"#);
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0][1], 0x0A, "power first");
        assert_eq!(frames[1][1], 0x0B, "brightness second");
    }
}
