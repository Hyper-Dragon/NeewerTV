//! Parsing the `0x09` state report.
//!
//! The device answers a `0x08` query with a long-form frame whose body is:
//!
//! ```text
//! data[0]     mode (see the mode registry)
//! data[1..]   zero or more segments, back to back
//! ```
//!
//! Each segment is a 16-bit big-endian length, a one-byte type, and
//! `length - 1` bytes of payload:
//!
//! ```text
//! length:2 BE   byte count of (type + payload)
//! type:1        which mode this segment describes
//! payload       length - 1 bytes
//! ```
//!
//! **The segment set is not fixed.** A real device reported types
//! `0, 9, 2, 3, 4, 5` in one frame, where `9` was undocumented. So parsing must
//! walk by length and keep types it does not understand, rather than assuming a
//! count or an order. Anything that indexes segments positionally will break on
//! a firmware build that reports one more than the author saw.

use super::command::ID_STATE_REPORT;
use super::{ProtocolError, frame};

/// The mode the device is currently in, plus every segment it chose to report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateReport {
    /// `data[0]` of the report: the device's current mode identifier.
    pub mode: u8,
    pub segments: Vec<Segment>,
}

/// One reported mode's parameters, kept as raw bytes until a caller knows what
/// the type means.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Segment {
    pub kind: u8,
    pub payload: Vec<u8>,
}

impl StateReport {
    /// Parse a complete `0x09` frame, checksum included.
    pub fn parse(bytes: &[u8]) -> Result<Self, ProtocolError> {
        if bytes.len() < 2 {
            return Err(ProtocolError::FrameTooShort(bytes.len()));
        }
        if bytes[0] != frame::HEADER {
            return Err(ProtocolError::BadHeader(bytes[0]));
        }
        // The identifier is checked before the checksum so that "this is not a
        // state report" is not reported as "this report is corrupt".
        if bytes[1] != ID_STATE_REPORT {
            return Err(ProtocolError::UnexpectedId(bytes[1]));
        }
        frame::verify(bytes)?;

        let Some((&mode, mut rest)) = body(bytes).split_first() else {
            return Err(ProtocolError::FrameTooShort(bytes.len()));
        };

        let mut segments = Vec::new();
        while rest.len() >= 3 {
            let length = u16::from_be_bytes([rest[0], rest[1]]) as usize;
            // A segment must be at least the type byte, and must fit.
            if length < 1 || rest.len() < length + 2 {
                break;
            }
            segments.push(Segment {
                kind: rest[2],
                payload: rest[3..length + 2].to_vec(),
            });
            rest = &rest[length + 2..];
        }

        Ok(Self { mode, segments })
    }

    /// Power state from the type `0` segment: `0x01` means on.
    pub fn power(&self) -> Option<bool> {
        let payload = &self.segment(0)?.payload;
        Some(*payload.first()? == 0x01)
    }

    /// The selected scene or effect from the type `3` segment.
    pub fn scene_id(&self) -> Option<u8> {
        self.segment(3)?.payload.get(1).copied()
    }

    /// The colour the strip is showing, from the type `2` segment.
    ///
    /// `[hi][lo][subtype]` then, for subtype 1 — single colour — a record laid out
    /// exactly like the write path's: `[lightness, hueHi, hueLo, saturation]`.
    /// Only subtype 1 is decoded: subtype 2 is a per-segment gradient and guessing
    /// at it would produce a colour nobody asked for.
    ///
    /// The hue and saturation are range-checked rather than trusted. A firmware
    /// that lays this out differently must produce `None`, not a colour invented
    /// from bytes that happen to be in range — and a wrong colour restored after
    /// an alert is worse than the daemon admitting it does not know.
    pub fn colour(&self) -> Option<(u16, u8)> {
        let payload = &self.segment(2)?.payload;
        if payload.get(2) != Some(&1) {
            return None;
        }

        let record = payload.get(3..7)?;
        let hue = u16::from_be_bytes([record[1], record[2]]);
        let saturation = record[3];

        (hue <= 360 && saturation <= 100).then_some((hue, saturation))
    }

    /// DIY brightness as a percentage, read from the type `5` segment.
    ///
    /// The payload is two bytes and only the first is understood: it reads as a
    /// 0–100 percentage, not the 0–1000 units the write path uses. The second
    /// byte is padding as far as this implementation can tell, so it is ignored
    /// rather than multiplied in — inventing a scale here would be a guess
    /// presented as a measurement.
    pub fn diy_brightness_pct(&self) -> Option<u8> {
        self.segment(5)?.payload.first().copied()
    }

    fn segment(&self, kind: u8) -> Option<&Segment> {
        self.segments.iter().find(|segment| segment.kind == kind)
    }
}

/// The body of a verified long-form frame: everything between the length field
/// and the checksum.
fn body(frame: &[u8]) -> &[u8] {
    &frame[4..frame.len() - 1]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Captured from a real strip in mode 2 (colour), 58 bytes.
    ///
    /// Note the type `9` segment: it is in the frame but was not in any
    /// documentation until this frame produced it. It must survive parsing.
    const CAPTURED: [u8; 58] = [
        0x7A, 0x09, 0x00, 0x35, 0x02, // header, long length 0x35, mode 2
        0x00, 0x03, 0x00, 0x01, 0x00, // type 0  switch, on
        0x00, 0x0A, 0x09, 0x64, 0x00, 0x01, 0x00, 0x01, 0x00, 0x32, 0x00, 0x00, // type 9
        0x00, 0x08, 0x02, 0x2F, 0x00, 0x01, 0x19, 0x00, 0x77, 0x64, // type 2  colour
        0x00, 0x05, 0x03, 0x00, 0x31, 0x64, 0x00, // type 3  scene 0x31
        0x00, 0x0B, 0x04, 0x01, 0x00, 0x64, 0x00, 0x32, 0x32, 0x01, 0x00, 0x40,
        0x64, // type 4  music
        0x00, 0x03, 0x05, 0x64, 0x00, // type 5  DIY brightness 100
        0x1D, // checksum
    ];

    #[test]
    fn parses_the_mode_and_every_segment() {
        let report = StateReport::parse(&CAPTURED).unwrap();
        assert_eq!(report.mode, 2);
        assert_eq!(report.segments.len(), 6);
        assert_eq!(
            report.segments.iter().map(|s| s.kind).collect::<Vec<_>>(),
            [0, 9, 2, 3, 4, 5]
        );
        assert_eq!(
            report
                .segments
                .iter()
                .map(|s| s.payload.len())
                .collect::<Vec<_>>(),
            [2, 9, 7, 4, 10, 2]
        );
    }

    #[test]
    fn keeps_the_undocumented_segment_instead_of_failing() {
        let report = StateReport::parse(&CAPTURED).unwrap();
        let unknown = report.segment(9).expect("type 9 must be retained");
        assert_eq!(
            unknown.payload,
            [0x64, 0x00, 0x01, 0x00, 0x01, 0x00, 0x32, 0x00, 0x00]
        );
    }

    #[test]
    fn reads_power_state() {
        assert_eq!(StateReport::parse(&CAPTURED).unwrap().power(), Some(true));
    }

    #[test]
    fn reads_the_selected_scene() {
        // 0x31 is the first "Natural" scene, Icelandic Blue.
        assert_eq!(
            StateReport::parse(&CAPTURED).unwrap().scene_id(),
            Some(0x31)
        );
    }

    #[test]
    fn reads_diy_brightness_as_a_percentage() {
        // The payload is [0x64, 0x00]; 0x64 is 100 %, and the second byte is
        // unverified padding rather than a units digit.
        assert_eq!(
            StateReport::parse(&CAPTURED).unwrap().diy_brightness_pct(),
            Some(100)
        );
    }

    #[test]
    fn reads_the_colour_the_strip_is_showing() {
        // Type 2, subtype 1: [0x2F 0x00] [0x01] then [lightness, hueHi, hueLo,
        // saturation] = [0x19, 0x00, 0x77, 0x64]. Hue 119° at full saturation.
        // Without this the daemon could not put the strip back after an alert had
        // painted it, which is precisely what someone notices.
        assert_eq!(
            StateReport::parse(&CAPTURED).unwrap().colour(),
            Some((119, 100))
        );
    }

    #[test]
    fn a_gradient_segment_is_not_read_as_a_single_colour() {
        // Subtype 2 is per-segment; its first four bytes are not one colour, and
        // reporting them as one would restore the wrong thing with confidence.
        let mut report = StateReport::parse(&CAPTURED).unwrap();
        for segment in &mut report.segments {
            if segment.kind == 2 {
                segment.payload[2] = 2;
            }
        }
        assert_eq!(report.colour(), None);
    }

    #[test]
    fn a_colour_outside_the_protocols_range_is_not_believed() {
        // A firmware that lays the record out differently must produce nothing
        // rather than a colour invented from bytes that happen to parse.
        let mut report = StateReport::parse(&CAPTURED).unwrap();
        for segment in &mut report.segments {
            if segment.kind == 2 {
                segment.payload[4] = 0xFF; // hue 0xFF77
                segment.payload[5] = 0xFF;
            }
        }
        assert_eq!(report.colour(), None);
    }

    #[test]
    fn accessors_return_none_when_the_segment_is_absent() {
        // Mode 0 plus a single switch segment carrying "off".
        let frame = [
            0x7A, 0x09, 0x00, 0x06, 0x00, 0x00, 0x03, 0x00, 0x00, 0x00, 0x8C,
        ];
        let report = StateReport::parse(&frame).unwrap();
        assert_eq!(report.mode, 0);
        assert_eq!(report.segments.len(), 1);
        assert_eq!(report.power(), Some(false));
        assert_eq!(report.scene_id(), None);
        assert_eq!(report.diy_brightness_pct(), None);
    }

    #[test]
    fn rejects_a_frame_with_the_wrong_identifier() {
        let mut frame = CAPTURED;
        frame[1] = 0x10;
        assert_eq!(
            StateReport::parse(&frame),
            Err(ProtocolError::UnexpectedId(0x10))
        );
    }

    #[test]
    fn rejects_a_corrupt_checksum() {
        let mut frame = CAPTURED;
        frame[57] = 0x00;
        assert!(matches!(
            StateReport::parse(&frame),
            Err(ProtocolError::Checksum { .. })
        ));
    }

    #[test]
    fn rejects_a_truncated_frame_rather_than_guessing() {
        assert_eq!(
            StateReport::parse(&CAPTURED[..10]),
            Err(ProtocolError::FrameTooShort(10))
        );
    }

    #[test]
    fn stops_cleanly_on_a_trailing_partial_segment() {
        // Mode, then a segment claiming ten bytes that are simply not there.
        // A half-received segment must not be treated as a parse failure: the
        // mode is still readable, and that is what a caller needs.
        let body = [0x02_u8, 0x00, 0x0A, 0x09];
        let frame = frame::long(0x09, &body);
        let report = StateReport::parse(&frame).expect("a partial segment is not fatal");
        assert_eq!(report.mode, 2);
        assert!(report.segments.is_empty());
    }
}
