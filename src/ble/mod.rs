//! The Bluetooth link to one strip.
//!
//! Only one task ever owns the link. Everything else observes it through a
//! snapshot, because the device accepts exactly one connection and a second
//! writer is a defect rather than a race.

pub mod bluetooth;
pub mod discovery;
pub mod link;
pub mod recovery;
pub mod worker;
