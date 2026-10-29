//! What to do with commands that arrive while the link is busy.
//!
//! Dragging a brightness slider produces a message per pixel. The strip accepts
//! one connection and takes tens of milliseconds per write, so sending each one
//! would queue behind itself and the light would finish the drag long after the
//! finger stopped.
//!
//! So while a write is in flight, only the most recent command is kept and the
//! rest are dropped. That is correct rather than merely convenient: each message
//! is a complete statement of the desired state, not an increment, so the last
//! one is the only one whose outcome anyone can observe. Dropping is counted so
//! the observability page can show that it happened.

use crate::app::command::Command;

/// One resolved request.
///
/// Deliberately carries no hint about whether the strip was off: the caller does
/// not know, and guessing from the payload ("no `state` key, so it must have been
/// off") is wrong for the commonest command of all, a brightness drag.
#[derive(Debug, Clone, PartialEq)]
pub struct Batch {
    pub commands: Vec<Command>,
}

/// Holds at most one pending batch. Newer arrivals replace older ones.
#[derive(Debug, Default)]
pub struct Coalescer {
    pending: Option<Batch>,
    dropped: u64,
}

impl Coalescer {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// How many batches have been discarded in favour of a newer one.
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.pending.is_none()
    }

    /// Offer a batch. An older pending batch is discarded and counted.
    pub fn push(&mut self, batch: Batch) {
        if self.pending.replace(batch).is_some() {
            self.dropped += 1;
        }
    }

    /// Take the pending batch, if there is one.
    pub fn take(&mut self) -> Option<Batch> {
        self.pending.take()
    }

    /// Put a batch back after a failed attempt.
    ///
    /// Not counted as a drop: nothing was superseded, the send simply did not
    /// happen. Losing a command because the link happened to be down is not a
    /// coalescing decision, it is a lost instruction.
    pub fn restore(&mut self, batch: Batch) {
        self.pending = Some(batch);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn batch(brightness: u16) -> Batch {
        Batch {
            commands: vec![Command::Brightness(brightness)],
        }
    }

    #[test]
    fn a_single_batch_is_kept() {
        let mut coalescer = Coalescer::new();
        coalescer.push(batch(100));
        assert_eq!(coalescer.take(), Some(batch(100)));
        assert_eq!(coalescer.dropped(), 0);
    }

    #[test]
    fn a_newer_batch_replaces_an_older_one() {
        let mut coalescer = Coalescer::new();
        coalescer.push(batch(100));
        coalescer.push(batch(200));

        assert_eq!(
            coalescer.take(),
            Some(batch(200)),
            "the latest statement of intent wins"
        );
    }

    #[test]
    fn every_replacement_is_counted() {
        // A slider drag is hundreds of these; the counter is the only evidence
        // that coalescing is doing anything.
        let mut coalescer = Coalescer::new();
        for level in 1..=50 {
            coalescer.push(batch(level));
        }
        assert_eq!(coalescer.take(), Some(batch(50)));
        assert_eq!(coalescer.dropped(), 49);
    }

    #[test]
    fn taking_the_batch_empties_the_queue_without_counting_a_drop() {
        let mut coalescer = Coalescer::new();
        coalescer.push(batch(100));
        let _ = coalescer.take();

        assert!(coalescer.is_empty());
        assert_eq!(coalescer.dropped(), 0, "sending is not dropping");
    }

    #[test]
    fn pushing_after_a_take_does_not_count_as_a_replacement() {
        let mut coalescer = Coalescer::new();
        coalescer.push(batch(100));
        let _ = coalescer.take();
        coalescer.push(batch(200));

        assert_eq!(coalescer.dropped(), 0);
        assert_eq!(coalescer.take(), Some(batch(200)));
    }

    #[test]
    fn taking_from_an_empty_queue_yields_nothing() {
        let mut coalescer = Coalescer::new();
        assert_eq!(coalescer.take(), None);
        assert!(coalescer.is_empty());
    }

    #[test]
    fn the_strip_being_off_is_not_the_batchs_business() {
        // It used to travel with the batch, which meant the MQTT and HTTP layers
        // had to guess it from the payload. A brightness drag carries no `state`
        // key, so the guess was always "off" — and an alert then restored a lit
        // strip by switching it off. The worker reads the real state instead.
        let batch = batch(100);
        assert_eq!(batch.commands, vec![Command::Brightness(100)]);
    }
}
