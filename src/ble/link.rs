//! The seam between the daemon and a Bluetooth stack.
//!
//! Everything above this trait — the worker, recovery, alerts, coalescing — is
//! written against these four methods and nothing else. That is what lets the
//! behaviour be tested without a radio, and it is why swapping `btleplug` for
//! `bluer` later would touch one file rather than the daemon.
//!
//! The methods return boxed futures so the trait stays object-safe: the daemon
//! owns exactly one link and one worker, so a trait object is fine and far less
//! noisy than threading a generic parameter through every layer.

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use crate::protocol::ProtocolError;
use crate::protocol::state::StateReport;

/// A future that is safe to move between tasks.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Everything that can go wrong on the link.
///
/// The distinction between `Connect` and `Write` matters: a connect failure is
/// expected and retried, while a write failure means the link went away
/// underneath a command that may or may not have been applied.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LinkError {
    #[error("connect: {0}")]
    Connect(String),

    #[error("write: {0}")]
    Write(String),

    #[error("no state report within {0:?}")]
    NoReply(Duration),

    #[error("protocol: {0}")]
    Protocol(#[from] ProtocolError),
}

/// One connection to one strip.
pub trait Link: Send {
    /// Connect, discover services and subscribe to notifications.
    ///
    /// Called again after every drop. Implementations MUST rebuild from the
    /// adapter rather than reusing a cached handle: after a power cycle the old
    /// handle refers to an object the Bluetooth stack has already destroyed.
    fn attach(&mut self) -> BoxFuture<'_, Result<(), LinkError>>;

    /// Write one complete frame.
    fn write<'a>(&'a mut self, frame: &'a [u8]) -> BoxFuture<'a, Result<(), LinkError>>;

    /// Wait for the next `0x09` state report, ignoring anything else.
    fn next_report<'a>(
        &'a mut self,
        timeout: Duration,
    ) -> BoxFuture<'a, Result<StateReport, LinkError>>;

    /// Whether the link currently believes it is up.
    fn is_up(&mut self) -> BoxFuture<'_, bool>;

    /// Release the device.
    ///
    /// Best effort and never fatal: this runs at shutdown, and failing to
    /// disconnect is not a reason to refuse to exit. It exists because the strip
    /// accepts one connection — a process that exits while still holding the
    /// link leaves the device unreachable, and not advertising, until BlueZ gets
    /// round to dropping it.
    fn detach(&mut self) -> BoxFuture<'_, ()>;

    /// The address in use, for logging. Empty when not attached.
    fn address(&self) -> String;
}
