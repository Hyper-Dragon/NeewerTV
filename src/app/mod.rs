//! Application layer: what a request means, as opposed to how it is encoded.
//!
//! Nothing here touches a socket or a radio. The pieces are deliberately kept
//! pure so that the behaviour Home Assistant depends on can be tested without
//! a broker or a strip.

pub mod alerts;
pub mod command;
pub mod queue;
