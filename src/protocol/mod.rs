//! The wire protocol: frames in, frames out.
//!
//! This module is deliberately **pure**. No async, no I/O, no logging that needs
//! a runtime, no BLE types. Every function here is a pure transformation of
//! bytes, which is what makes it testable without a strip and what protects the
//! hard-won byte layouts from being invalidated by a change to the transport.
//!
//! The layouts stated in the doc comments are the specification for this code.
//! They are written out in full rather than referenced, because a reader of this
//! repository does not have any other document to look them up in.

pub mod catalogue;
pub mod command;
pub mod frame;
pub mod hsi;
pub mod state;

/// Everything that can go wrong turning intent into bytes, or bytes into state.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProtocolError {
    #[error("brightness {0} is outside 0..=1000")]
    Brightness(u16),

    #[error("hue {0} is outside 0..=360")]
    Hue(u16),

    #[error("saturation {0} is outside 0..=100")]
    Saturation(u8),

    #[error("lightness {0} is outside 0..=100")]
    Lightness(u8),

    #[error("speed {0} is outside 0..=100")]
    Speed(u8),

    #[error("expand {0} is outside its permitted range")]
    Expand(u8),

    #[error("{0} colour entries is outside 1..=20")]
    SegmentCount(usize),

    #[error("frame too short: {0} bytes")]
    FrameTooShort(usize),

    #[error("bad header byte 0x{0:02X}, expected 0x7A")]
    BadHeader(u8),

    #[error("unexpected data identifier 0x{0:02X}")]
    UnexpectedId(u8),

    #[error("length field says {declared} but the body is {actual} bytes")]
    LengthMismatch { declared: usize, actual: usize },

    #[error("checksum mismatch: computed 0x{computed:02X}, frame carries 0x{found:02X}")]
    Checksum { computed: u8, found: u8 },
}
