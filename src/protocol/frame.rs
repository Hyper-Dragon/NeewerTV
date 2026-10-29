//! Packet framing.
//!
//! Every packet is `[0x7A][identifier][length…][data…][checksum]` and exists in
//! two forms, selected by the identifier and not by the payload:
//!
//! ```text
//! short:  7A  id  len        data (len bytes)      sum
//!         0   1   2          3 .. 2+len            3+len
//!
//! long:   7A  id  lenHi lenLo  data (len bytes)    sum
//!         0   1   2     3      4 .. 3+len          4+len
//! ```
//!
//! The checksum is the low 8 bits of the sum of every preceding byte in the
//! frame, header and length field(s) included.
//!
//! Identifiers using the short form: `08 0A 0B 0C 0E 10 16 40 41 42 43`.
//! Identifiers using the long form: `09 0D 0F 11 15`.

use super::ProtocolError;

/// Every frame starts with this byte.
pub const HEADER: u8 = 0x7A;

/// Low 8 bits of the unsigned sum of `bytes`.
pub fn checksum(bytes: &[u8]) -> u8 {
    bytes.iter().fold(0_u8, |sum, byte| sum.wrapping_add(*byte))
}

/// Identifiers the protocol sends in the long form. The form is a property of
/// the identifier, not of the payload.
const fn is_long_form(id: u8) -> bool {
    matches!(id, 0x09 | 0x0D | 0x0F | 0x11 | 0x15)
}

/// Wrap `data` in the short form.
pub fn short(id: u8, data: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(data.len() + 4);
    frame.push(HEADER);
    frame.push(id);
    // A short-form length is one byte; anything longer cannot be expressed and
    // silently truncating would produce a frame the device rejects.
    debug_assert!(
        data.len() <= u8::MAX as usize,
        "short form cannot exceed 255 data bytes"
    );
    frame.push(data.len() as u8);
    frame.extend_from_slice(data);
    frame.push(checksum(&frame));
    frame
}

/// Wrap `data` in the long form: a big-endian `u16` length.
pub fn long(id: u8, data: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(data.len() + 5);
    frame.push(HEADER);
    frame.push(id);
    frame.extend_from_slice(&(data.len() as u16).to_be_bytes());
    frame.extend_from_slice(data);
    frame.push(checksum(&frame));
    frame
}

/// Validate a frame's header, length field and checksum.
///
/// The length is checked before the checksum so that a truncated frame is
/// reported as truncated rather than as a corruption — the two want different
/// responses from a caller.
pub fn verify(frame: &[u8]) -> Result<(), ProtocolError> {
    if frame.len() < 2 {
        return Err(ProtocolError::FrameTooShort(frame.len()));
    }
    if frame[0] != HEADER {
        return Err(ProtocolError::BadHeader(frame[0]));
    }

    let header_len = if is_long_form(frame[1]) { 4 } else { 3 };
    if frame.len() < header_len + 1 {
        return Err(ProtocolError::FrameTooShort(frame.len()));
    }

    let declared = if is_long_form(frame[1]) {
        u16::from_be_bytes([frame[2], frame[3]]) as usize
    } else {
        frame[2] as usize
    };

    // Everything after the header: the declared data plus the checksum.
    let body = &frame[header_len..];
    let expected = declared + 1;
    if body.len() < expected {
        return Err(ProtocolError::FrameTooShort(frame.len()));
    }
    if body.len() > expected {
        return Err(ProtocolError::LengthMismatch {
            declared,
            actual: body.len() - 1,
        });
    }

    let computed = checksum(&frame[..frame.len() - 1]);
    let found = frame[frame.len() - 1];
    if computed != found {
        return Err(ProtocolError::Checksum { computed, found });
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The worked example from the protocol: power on.
    const POWER_ON: [u8; 5] = [0x7A, 0x0A, 0x01, 0x01, 0x86];

    #[test]
    fn checksum_covers_header_and_length() {
        // 0x7A + 0x0A + 0x01 + 0x01 = 0x86
        assert_eq!(checksum(&[0x7A, 0x0A, 0x01, 0x01]), 0x86);
    }

    #[test]
    fn checksum_keeps_only_the_low_byte() {
        // Sums to 0x1FC; the frame carries 0xFC.
        assert_eq!(checksum(&[0xFF, 0xFF, 0xFE]), 0xFC);
        // An empty frame has no bytes to add, so the sum is zero.
        assert_eq!(checksum(&[]), 0x00);
    }

    #[test]
    fn short_form_matches_the_power_on_example() {
        assert_eq!(short(0x0A, &[0x01]), POWER_ON);
    }

    #[test]
    fn short_form_matches_the_state_query_example() {
        assert_eq!(short(0x08, &[0x00]), [0x7A, 0x08, 0x01, 0x00, 0x83]);
    }

    #[test]
    fn short_form_encodes_an_empty_payload_as_length_zero() {
        assert_eq!(short(0x43, &[]), [0x7A, 0x43, 0x00, 0xBD]);
    }

    #[test]
    fn long_form_matches_the_single_colour_example() {
        // Brightness 80 % (800), type 1, constant 0x64, hue 240, sat 100.
        let data = [0x50, 0x00, 0x01, 0x64, 0x00, 0xF0, 0x64, 0x00, 0x01, 0x00];
        assert_eq!(
            long(0x0D, &data),
            [
                0x7A, 0x0D, 0x00, 0x0A, 0x50, 0x00, 0x01, 0x64, 0x00, 0xF0, 0x64, 0x00, 0x01, 0x00,
                0x9B
            ]
        );
    }

    #[test]
    fn long_form_writes_the_length_big_endian() {
        // 0x0102 = 258 bytes of payload: high byte first.
        let frame = long(0x0F, &[0u8; 258]);
        assert_eq!(&frame[2..4], &[0x01, 0x02]);
        assert_eq!(frame.len(), 4 + 258 + 1);
    }

    #[test]
    fn verify_accepts_a_well_formed_frame() {
        assert_eq!(verify(&POWER_ON), Ok(()));
    }

    #[test]
    fn verify_rejects_a_bad_checksum() {
        let mut frame = POWER_ON;
        *frame.last_mut().unwrap() ^= 0xFF;
        assert!(matches!(
            verify(&frame),
            Err(ProtocolError::Checksum { .. })
        ));
    }

    #[test]
    fn verify_rejects_a_bad_header() {
        let mut frame = POWER_ON;
        frame[0] = 0x78;
        assert_eq!(verify(&frame), Err(ProtocolError::BadHeader(0x78)));
    }

    #[test]
    fn verify_rejects_a_truncated_frame() {
        assert_eq!(verify(&POWER_ON[..4]), Err(ProtocolError::FrameTooShort(4)));
        assert_eq!(verify(&[]), Err(ProtocolError::FrameTooShort(0)));
    }

    #[test]
    fn verify_rejects_a_length_field_that_disagrees_with_the_body() {
        // Claims one data byte but carries two.
        let frame = [0x7A, 0x0A, 0x01, 0x01, 0x01, 0x00];
        assert_eq!(
            verify(&frame),
            Err(ProtocolError::LengthMismatch {
                declared: 1,
                actual: 2
            })
        );
    }
}
