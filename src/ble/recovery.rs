//! What to do when the link is not up.
//!
//! This is a pure state machine on purpose. Deciding *when* to try again is the
//! part that gets rewritten in a panic at 2 a.m. during a reconnect storm, and
//! it is the one part of the Bluetooth layer that can be tested exhaustively
//! without a radio. The rules it encodes were measured, not guessed — they come
//! from a spike that held one link for 77 queries and recovered from a power
//! cycle in 7.3 seconds, and from the same spike failing 2 times in 8 when it
//! connected and disconnected per query.
//!
//! Two of those measurements are load-bearing here:
//!
//! * **Retry once, immediately, before backing off.** The first connect attempt
//!   after a disconnect frequently fails with `Software caused connection abort`
//!   and the very next one succeeds. Backing off before trying again would turn a
//!   one-second recovery into a twelve-second one.
//! * **Never reuse the peripheral handle.** After a drop the device may have been
//!   powered off and on, and its advertisement is the only way back. A rescan is
//!   not a fallback, it is the recovery.

use std::time::Duration;

/// How many consecutive failures before the log names the host-side remedy.
pub const WEDGE_WARNING_AFTER: u32 = 5;

/// Longest we ever wait between attempts.
pub const MAX_BACKOFF: Duration = Duration::from_secs(60);

/// What just happened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkEvent {
    /// Connect, discover and subscribe all succeeded.
    Connected,
    /// A connect attempt against a device we had already found failed.
    ConnectFailed,
    /// A scan finished without seeing the device at all.
    NotFound,
    /// The link was up and then went away.
    Dropped,
}

/// What to do next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// The link is up. Nothing to do.
    Idle,
    /// Try the peripheral we already hold, with no delay and no rescan.
    RetrySame,
    /// Sleep, then rescan by name and reattach from scratch.
    RescanAfter(Duration),
}

/// The decision, plus anything the operator should be told.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Outcome {
    pub step: Step,
    /// Set when the failure count suggests the host adapter is wedged rather
    /// than the strip being away. The caller logs this **once** per episode and
    /// names the host remedy; the daemon must not try to restart the host
    /// service itself.
    pub warn_host_remedy: bool,
}

/// Consecutive-failure tracker driving the reconnect decision.
#[derive(Debug, Default)]
pub struct Recovery {
    consecutive_failures: u32,
}

impl Recovery {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// How many attempts have failed in a row. Zero after a success.
    #[must_use]
    pub fn consecutive_failures(&self) -> u32 {
        self.consecutive_failures
    }

    /// Feed in what happened, get back what to do.
    pub fn on(&mut self, event: LinkEvent) -> Outcome {
        if event == LinkEvent::Connected {
            self.consecutive_failures = 0;
            return Outcome {
                step: Step::Idle,
                warn_host_remedy: false,
            };
        }

        self.consecutive_failures = self.consecutive_failures.saturating_add(1);

        let step = match (self.consecutive_failures, event) {
            // The one retry we get for free. A fresh attempt after an abort
            // usually fails and the next one succeeds, and that retry is worth
            // more than any delay: it is the difference between a one-second
            // recovery and a twelve-second one.
            (1, LinkEvent::ConnectFailed) => Step::RetrySame,
            // Nothing is worth retrying after a drop (the handle now points at a
            // destroyed object) or after a scan that saw nothing at all.
            (1, _) => Step::RescanAfter(Duration::ZERO),
            (failures, _) => Step::RescanAfter(backoff(failures)),
        };

        Outcome {
            step,
            // `==` rather than `>=`: the doc above promises this is named once per
            // episode. With `>=` it re-printed on every attempt after the fifth,
            // which at the sixty-second cap means a warning every minute for as
            // long as the strip is unplugged.
            warn_host_remedy: self.consecutive_failures == WEDGE_WARNING_AFTER,
        }
    }
}

/// 2, 4, 8 … seconds, capped at [`MAX_BACKOFF`].
///
/// Called with at least two consecutive failures; the first failure is handled
/// by the retry-immediately rule rather than by waiting.
fn backoff(failures: u32) -> Duration {
    let exponent = failures.saturating_sub(1).min(31);
    let seconds = 2_u64.saturating_pow(exponent);
    Duration::from_secs(seconds).min(MAX_BACKOFF)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn after(events: &[LinkEvent]) -> (Recovery, Outcome) {
        let mut recovery = Recovery::new();
        let mut outcome = recovery.on(LinkEvent::Connected);
        for event in events {
            outcome = recovery.on(*event);
        }
        (recovery, outcome)
    }

    #[test]
    fn a_successful_connection_has_nothing_to_do() {
        let (recovery, outcome) = after(&[LinkEvent::Connected]);
        assert_eq!(outcome.step, Step::Idle);
        assert!(!outcome.warn_host_remedy);
        assert_eq!(recovery.consecutive_failures(), 0);
    }

    #[test]
    fn the_first_connect_failure_retries_immediately() {
        // Measured: the attempt after an abort usually fails, the one after that
        // succeeds. Waiting first turns a fast recovery into a slow one.
        let (_, outcome) = after(&[LinkEvent::ConnectFailed]);
        assert_eq!(outcome.step, Step::RetrySame);
        assert!(!outcome.warn_host_remedy);
    }

    #[test]
    fn the_second_connect_failure_backs_off_and_rescans() {
        let (_, outcome) = after(&[LinkEvent::ConnectFailed, LinkEvent::ConnectFailed]);
        assert_eq!(outcome.step, Step::RescanAfter(Duration::from_secs(2)));
    }

    #[test]
    fn backoff_doubles_with_each_further_failure() {
        use LinkEvent::ConnectFailed;
        let cases = [
            (2, Duration::from_secs(2)),
            (3, Duration::from_secs(4)),
            (4, Duration::from_secs(8)),
            (5, Duration::from_secs(16)),
            (6, Duration::from_secs(32)),
        ];
        for (failures, expected) in cases {
            let (_, outcome) = after(&vec![ConnectFailed; failures]);
            assert_eq!(
                outcome.step,
                Step::RescanAfter(expected),
                "after {failures} failures"
            );
        }
    }

    #[test]
    fn backoff_is_capped_rather_than_growing_without_bound() {
        let (recovery, outcome) = after(&[LinkEvent::ConnectFailed; 20]);
        assert_eq!(outcome.step, Step::RescanAfter(MAX_BACKOFF));
        assert_eq!(recovery.consecutive_failures(), 20);
    }

    #[test]
    fn every_failure_after_the_second_rescans_rather_than_reusing_the_handle() {
        // The handle is dead after a drop, so only the very first failure is
        // allowed to retry it.
        let (_, outcome) = after(&[LinkEvent::ConnectFailed; 7]);
        assert!(matches!(outcome.step, Step::RescanAfter(_)));
    }

    #[test]
    fn a_drop_rescans_immediately_without_retrying_the_old_handle() {
        // The device may have been power-cycled: its handle refers to an object
        // BlueZ has destroyed, so retrying it would fail forever.
        let (_, outcome) = after(&[LinkEvent::Dropped]);
        assert_eq!(outcome.step, Step::RescanAfter(Duration::ZERO));
    }

    #[test]
    fn repeated_drops_back_off_rather_than_spinning() {
        let (_, outcome) = after(&[LinkEvent::Dropped, LinkEvent::Dropped]);
        assert_eq!(outcome.step, Step::RescanAfter(Duration::from_secs(2)));
    }

    #[test]
    fn a_missing_device_is_waited_for_rather_than_abandoned() {
        // A strip can be unplugged for a day. There is no point at which giving
        // up is correct, so NotFound never stops rescanning.
        let (_, outcome) = after(&[LinkEvent::NotFound; 30]);
        assert_eq!(outcome.step, Step::RescanAfter(MAX_BACKOFF));
    }

    #[test]
    fn success_clears_the_count_and_the_backoff() {
        use LinkEvent::{ConnectFailed, Connected};
        let (recovery, outcome) = after(&[
            ConnectFailed,
            ConnectFailed,
            ConnectFailed,
            Connected,
            ConnectFailed,
        ]);
        // Back to the first-failure behaviour: retry immediately again.
        assert_eq!(outcome.step, Step::RetrySame);
        assert_eq!(recovery.consecutive_failures(), 1);
    }

    #[test]
    fn the_host_remedy_is_flagged_only_once_the_wedge_is_likely() {
        use LinkEvent::ConnectFailed;
        let (_, early) = after(&vec![ConnectFailed; (WEDGE_WARNING_AFTER - 1) as usize]);
        assert!(!early.warn_host_remedy, "should not cry wolf yet");

        let (_, late) = after(&vec![ConnectFailed; WEDGE_WARNING_AFTER as usize]);
        assert!(late.warn_host_remedy, "should name the host remedy now");

        // Dropping the strip out of range is not a wedge, but it is
        // indistinguishable from one after this many failures, so it also warns.
        let (_, missing) = after(&vec![LinkEvent::NotFound; WEDGE_WARNING_AFTER as usize]);
        assert!(missing.warn_host_remedy);
    }

    #[test]
    fn the_host_remedy_is_named_once_per_episode() {
        use LinkEvent::ConnectFailed;
        let mut recovery = Recovery::new();
        for _ in 0..WEDGE_WARNING_AFTER - 1 {
            assert!(!recovery.on(ConnectFailed).warn_host_remedy);
        }
        assert!(recovery.on(ConnectFailed).warn_host_remedy);
        assert!(
            !recovery.on(ConnectFailed).warn_host_remedy,
            "and not again on every attempt after it, which at the sixty-second \
             cap is a warning a minute for as long as the strip is unplugged"
        );

        // A connection ends the episode, so the next run of failures earns its
        // own warning rather than being silenced by the last one.
        recovery.on(LinkEvent::Connected);
        for _ in 0..WEDGE_WARNING_AFTER - 1 {
            assert!(!recovery.on(ConnectFailed).warn_host_remedy);
        }
        assert!(recovery.on(ConnectFailed).warn_host_remedy);
    }
}
