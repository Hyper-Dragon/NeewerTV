//! Alerts: do something loud, then put the strip back.
//!
//! This is the reason the daemon exists. A doorbell rings, or the dishwasher
//! finishes, and the strip should react for a few seconds — and then go back to
//! being whatever it was being before, without Home Assistant having to remember
//! what that was.
//!
//! # Why the bookkeeping lives here
//!
//! The obvious implementation puts it in the automation: read the current state,
//! set the alert, wait, set the state back. It breaks in three ways that are all
//! boring and all real:
//!
//! * two alerts overlapping each other leave the strip restored to a snapshot
//!   taken mid-alert, so it ends up stuck showing one of them;
//! * a person changing the colour during the alert gets their change reverted
//!   when the timer expires, which feels like the light is fighting them;
//! * a Home Assistant restart in the middle loses the snapshot entirely.
//!
//! So the daemon owns one piece of state — *what to put back* — and the
//! automation sends one message. Everything below is about keeping that
//! bookkeeping honest.
//!
//! # The invariant that is easy to get wrong
//!
//! **An alert must never record its own frames as the thing to restore.** Doing
//! so is self-erasing: the strip ends up restored to the alert, and after two
//! alerts it is permanently stuck flashing. The method that records state is
//! therefore reachable only from a user command, never from [`Alerts::fire`], so
//! the bug cannot be written by accident.

use crate::protocol::command as wire;

/// What to display, and for how long.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AlertSpec {
    /// Names the alert, for logs and for `alert/{id}` triggers.
    pub id: String,
    /// Higher wins. Two alerts of equal priority do not pre-empt each other.
    pub priority: u8,
    pub duration_ms: u64,
    /// Whether this alert may light a strip that is not already on.
    ///
    /// A doorbell is worth waking the room for; a dishwasher notification at 2 am
    /// may not be. When this is `false` the alert is dropped by a strip that is
    /// off or whose state has never been established — refusing to disturb is the
    /// only defensible reading of "do not turn it on for this".
    pub force_on: bool,
    /// The frames that display the alert.
    pub frames: Vec<Vec<u8>>,
}

/// The result of asking for an alert.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fire {
    /// Write these frames.
    Fired { frames: Vec<Vec<u8>> },
    /// Refused. Counted, not silently ignored: either something at least as
    /// important is already showing, or this alert was not allowed to light a
    /// strip that is not already on.
    Rejected,
}

/// An alert currently on screen.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Active {
    id: String,
    priority: u8,
    until_ms: u64,
    /// Exactly what to replay when this ends.
    restore: Vec<Vec<u8>>,
}

/// The alert bookkeeper.
#[derive(Debug, Default)]
pub struct Alerts {
    active: Option<Active>,
    /// Frames of the last *user* command.
    last_frames: Vec<Vec<u8>>,
    /// Whether the strip was off at the time of that command.
    ///
    /// `None` means nobody has established it: no command has been sent and the
    /// strip has not been asked. That is deliberately distinct from `Some(false)`,
    /// because "I do not know" and "I know it is lit" justify different restores.
    last_was_off: Option<bool>,
    fired: u64,
    dropped: u64,
}

impl Alerts {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn is_active(&self) -> bool {
        self.active.is_some()
    }

    /// `(fired, dropped)` since start, for the observability page.
    #[must_use]
    pub fn counters(&self) -> (u64, u64) {
        (self.fired, self.dropped)
    }

    /// The id of the alert currently showing, if any. Reported on `/api/state`.
    #[must_use]
    pub fn active_id(&self) -> Option<&str> {
        self.active.as_ref().map(|active| active.id.as_str())
    }

    /// Record what a *user* command did, and cancel any alert without replaying.
    ///
    /// A person changing the light during an alert wins: their command is
    /// applied and the alert's restore is discarded, because restoring it would
    /// undo something they just asked for.
    ///
    /// Returns whether an alert was cancelled.
    pub fn user_command(&mut self, frames: Vec<Vec<u8>>, was_off: bool) -> bool {
        self.last_frames = frames;
        self.last_was_off = Some(was_off);
        self.active.take().is_some()
    }

    /// Whether anything is known about what the strip was doing.
    #[must_use]
    pub fn baseline_known(&self) -> bool {
        self.last_was_off.is_some() || !self.last_frames.is_empty()
    }

    /// Adopt the strip's own account of its power state.
    ///
    /// Called when the device answers a state query on connect. It only takes
    /// effect while nothing has been commanded: once the user has asked for
    /// something, the daemon's record of what it sent is better evidence than a
    /// re-read, and a reconnect must not overwrite it.
    pub fn observe_power(&mut self, on: bool) {
        if !self.baseline_known() {
            self.last_was_off = Some(!on);
        }
    }

    /// Show an alert, if it outranks whatever is on screen.
    pub fn fire(&mut self, spec: &AlertSpec, now_ms: u64) -> Fire {
        // An alert whose deadline has passed but which has not been ticked out
        // yet must not block a new one: the worker ticks on a timer, and a
        // priority check against a dead alert is a coin flip on scheduling.
        if self
            .active
            .as_ref()
            .is_some_and(|active| now_ms >= active.until_ms)
        {
            self.active = None;
        }

        if let Some(active) = &self.active
            && spec.priority <= active.priority
        {
            self.dropped += 1;
            return Fire::Rejected;
        }

        // An alert that does not ask to force the strip on is only shown on a
        // strip already known to be lit. A state that has never been established
        // counts as dark: refusing to disturb is the conservative reading, and
        // an alert that matters sets `force_on`.
        if !spec.force_on && self.last_was_off != Some(false) {
            self.dropped += 1;
            return Fire::Rejected;
        }

        // Captured *before* anything else changes, and only ever from a user
        // command — never from whatever happens to be on screen, which may be
        // another alert's frames.
        let restore = match self.last_was_off {
            // Known to be off, so putting it back means switching it off. Replaying
            // a colour to a dark room would leave it lit.
            Some(true) => vec![power_off()],
            // Known to be lit, but nothing was ever commanded — the strip was on
            // when we met it, and the state query is the only thing that said so.
            //
            // There is no brightness to replay, and the alert has just set the
            // strip's brightness to zero, so power alone comes back to a dark
            // strip: from the sofa, indistinguishable from the alert switching the
            // light off. Come back *visible* instead. Full is a guess, but it is a
            // guess in the direction of "the light is on", and it is the only
            // honest one available — the device does not echo brightness back, so
            // the daemon genuinely has nothing better.
            Some(false) if self.last_frames.is_empty() => {
                vec![wire::power(true), bright()]
            }
            // Known lit, or never established. There is no honest way to rebuild a
            // colour the daemon never saw, so replay what it last commanded — which
            // is empty, and therefore a no-op, when it has commanded nothing.
            _ => self.last_frames.clone(),
        };

        self.active = Some(Active {
            id: spec.id.clone(),
            priority: spec.priority,
            until_ms: now_ms.saturating_add(spec.duration_ms),
            restore,
        });
        self.fired += 1;

        Fire::Fired {
            frames: spec.frames.clone(),
        }
    }

    /// Advance time. Returns the frames to write when an alert has just ended.
    pub fn tick(&mut self, now_ms: u64) -> Option<Vec<Vec<u8>>> {
        if now_ms < self.active.as_ref()?.until_ms {
            return None;
        }
        self.active.take().map(|active| active.restore)
    }

    /// Cancel explicitly, optionally putting the strip back.
    ///
    /// `replay = false` is the "stop that noise" path: cancel and leave the
    /// strip showing whatever the alert left, because the caller is about to
    /// send their own command.
    pub fn cancel(&mut self, replay: bool) -> Option<Vec<Vec<u8>>> {
        let active = self.active.take()?;
        replay.then_some(active.restore)
    }
}

/// The frames that turn the strip off.
fn power_off() -> Vec<u8> {
    wire::power(false)
}

/// Full brightness, for the one restore case where nothing was ever observed.
fn bright() -> Vec<u8> {
    wire::brightness(1000).expect("1000 is inside the protocol's range")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(id: &str, priority: u8) -> AlertSpec {
        AlertSpec {
            id: id.to_owned(),
            priority,
            duration_ms: 1_000,
            force_on: true,
            frames: vec![vec![0xAA, 0x01]],
        }
    }

    fn normal(id: &str, frames: Vec<Vec<u8>>) -> AlertSpec {
        AlertSpec {
            frames,
            ..spec(id, 100)
        }
    }

    #[test]
    fn an_alert_fires_when_nothing_is_showing() {
        let mut alerts = Alerts::new();
        let alert = normal("doorbell", vec![vec![0x01], vec![0x02]]);
        assert_eq!(
            alerts.fire(&alert, 0),
            Fire::Fired {
                frames: vec![vec![0x01], vec![0x02]]
            }
        );
        assert!(alerts.is_active());
        assert_eq!(alerts.counters(), (1, 0));
    }

    #[test]
    fn the_strip_returns_to_exactly_what_it_was_doing() {
        let mut alerts = Alerts::new();
        alerts.user_command(vec![vec![0x10], vec![0x11]], false);
        alerts.fire(&normal("doorbell", vec![vec![0xAA]]), 0);

        let restore = alerts.tick(1_000).expect("the alert should have ended");
        assert_eq!(
            restore,
            vec![vec![0x10], vec![0x11]],
            "in order, exactly as recorded"
        );
        assert!(!alerts.is_active());
    }

    #[test]
    fn an_alert_that_forced_the_strip_on_restores_it_off() {
        let mut alerts = Alerts::new();
        // The strip was off when the alert arrived.
        alerts.user_command(vec![power_off()], true);

        let mut alert = normal("doorbell", vec![vec![0xAA]]);
        alert.force_on = true;
        alerts.fire(&alert, 0);

        assert_eq!(
            alerts.tick(1_000).expect("ended"),
            vec![power_off()],
            "restoring a colour to an off strip would leave the room lit"
        );
    }

    #[test]
    fn an_alert_that_does_not_force_on_is_refused_by_a_dark_strip() {
        // The whole point of the flag: a dishwasher notification is not worth
        // lighting the room for at night, and the caller says so.
        let mut alerts = Alerts::new();
        alerts.user_command(vec![power_off()], true);

        let mut alert = normal("dishwasher", vec![vec![0xAA]]);
        alert.force_on = false;

        assert_eq!(alerts.fire(&alert, 0), Fire::Rejected);
        assert_eq!(alerts.counters(), (0, 1), "counted, not silently swallowed");
    }

    #[test]
    fn an_alert_that_does_not_force_on_still_shows_on_a_lit_strip() {
        let mut alerts = Alerts::new();
        alerts.user_command(vec![vec![0x10]], false);

        let mut alert = normal("dishwasher", vec![vec![0xAA]]);
        alert.force_on = false;

        assert_eq!(
            alerts.fire(&alert, 0),
            Fire::Fired {
                frames: vec![vec![0xAA]]
            },
            "nothing is being woken, so there is nothing to refuse"
        );
    }

    #[test]
    fn an_unestablished_state_counts_as_dark() {
        // No command, no query answered. Refusing to disturb is the only
        // defensible reading; the alternative is lighting a room on a guess.
        let mut alerts = Alerts::new();
        let mut alert = normal("dishwasher", vec![vec![0xAA]]);
        alert.force_on = false;

        assert!(!alerts.baseline_known());
        assert_eq!(alerts.fire(&alert, 0), Fire::Rejected);
    }

    #[test]
    fn a_strip_that_reported_itself_off_is_switched_off_again_after_an_alert() {
        // The case that used to be a coin flip: nothing has ever been commanded,
        // so the only thing that can rescue the restore is the strip's own answer.
        let mut alerts = Alerts::new();
        alerts.observe_power(false);

        alerts.fire(&normal("doorbell", vec![vec![0xAA]]), 0);

        assert_eq!(
            alerts.tick(1_000).expect("ended"),
            vec![power_off()],
            "it was off before the alert, so it goes back off"
        );
    }

    #[test]
    fn a_strip_that_reported_itself_on_comes_back_on_after_an_alert() {
        // The mirror of the test above, and the bug it used to hide: a strip that
        // was on when the daemon met it, with nothing commanded since, restored to
        // *nothing*. That is not a no-op — the alert's final frame dims the strip
        // to nothing, so the room was left dark and it looked like the alert had
        // switched the light off.
        let mut alerts = Alerts::new();
        alerts.observe_power(true);

        alerts.fire(&normal("doorbell", vec![vec![0xAA]]), 0);

        assert_eq!(
            alerts.tick(1_000).expect("ended"),
            vec![wire::power(true), bright()],
            "it was lit before the alert, so it comes back on — and visible, because \
             the alert set the brightness to zero and there is no recorded one to replay"
        );
    }

    #[test]
    fn a_command_outranks_what_the_device_reported() {
        // A reconnect must not overwrite the daemon's own record with a stale read.
        let mut alerts = Alerts::new();
        alerts.observe_power(false);
        alerts.user_command(vec![vec![0x10]], false);
        alerts.observe_power(false);

        alerts.fire(&normal("doorbell", vec![vec![0xAA]]), 0);
        assert_eq!(
            alerts.tick(1_000).expect("ended"),
            vec![vec![0x10]],
            "the user's command is the better evidence"
        );
    }

    #[test]
    fn a_lower_priority_alert_is_dropped_and_counted() {
        let mut alerts = Alerts::new();
        alerts.fire(&spec("doorbell", 50), 0);
        assert_eq!(alerts.fire(&spec("dishwasher", 10), 100), Fire::Rejected);
        assert_eq!(
            alerts.counters(),
            (1, 1),
            "dropped is countable, not silent"
        );
    }

    #[test]
    fn an_equal_priority_alert_does_not_pre_empt() {
        let mut alerts = Alerts::new();
        alerts.fire(&spec("doorbell", 50), 0);
        assert_eq!(alerts.fire(&spec("phone", 50), 100), Fire::Rejected);
    }

    #[test]
    fn a_higher_priority_alert_pre_empts_and_restarts_the_clock() {
        let mut alerts = Alerts::new();
        alerts.user_command(vec![vec![0x10]], false);

        alerts.fire(&spec("dishwasher", 10), 0);
        assert_eq!(
            alerts.fire(&spec("doorbell", 50), 500),
            Fire::Fired {
                frames: vec![vec![0xAA, 0x01]]
            }
        );

        // The original alert would have expired at 1_000; the new one runs from
        // 500 to 1_500.
        assert_eq!(alerts.tick(1_000), None, "not yet");
        assert_eq!(alerts.tick(1_500), Some(vec![vec![0x10]]));
    }

    #[test]
    fn a_user_command_cancels_without_replaying() {
        let mut alerts = Alerts::new();
        alerts.user_command(vec![vec![0x10]], false);
        alerts.fire(&normal("doorbell", vec![vec![0xAA]]), 0);

        // Someone changes the colour mid-alert.
        let cancelled = alerts.user_command(vec![vec![0x20]], false);
        assert!(
            cancelled,
            "the caller may want to know an alert was cut short"
        );
        assert!(!alerts.is_active());

        // The alert's timer must not later undo the person's change.
        assert_eq!(alerts.tick(5_000), None);
    }

    #[test]
    fn cancelling_with_replay_puts_the_strip_back() {
        let mut alerts = Alerts::new();
        alerts.user_command(vec![vec![0x10]], false);
        alerts.fire(&normal("doorbell", vec![vec![0xAA]]), 0);

        assert_eq!(alerts.cancel(true), Some(vec![vec![0x10]]));
        assert!(!alerts.is_active());
    }

    #[test]
    fn cancelling_without_replay_leaves_the_strip_alone() {
        let mut alerts = Alerts::new();
        alerts.user_command(vec![vec![0x10]], false);
        alerts.fire(&normal("doorbell", vec![vec![0xAA]]), 0);

        assert_eq!(alerts.cancel(false), None);
        assert!(!alerts.is_active());
    }

    #[test]
    fn an_alert_never_records_its_own_frames_as_the_restore_point() {
        // The self-erasing bug: if firing captured the frames currently on
        // screen, the second alert would restore to the first one and the strip
        // would never come back.
        let mut alerts = Alerts::new();
        alerts.user_command(vec![vec![0x10]], false);

        alerts.fire(&normal("first", vec![vec![0xA1]]), 0);
        alerts.fire(&normal("second", vec![vec![0xA2]]), 2_000);

        assert_eq!(
            alerts.tick(3_000),
            Some(vec![vec![0x10]]),
            "restore is the user's state, not either alert"
        );
    }

    #[test]
    fn ticking_before_the_deadline_does_nothing() {
        let mut alerts = Alerts::new();
        alerts.fire(&normal("doorbell", vec![vec![0xAA]]), 0);
        assert_eq!(alerts.tick(999), None);
        assert!(alerts.is_active());
    }

    #[test]
    fn cancel_and_tick_after_the_alert_has_gone_are_harmless() {
        let mut alerts = Alerts::new();
        assert_eq!(alerts.cancel(true), None);
        assert_eq!(alerts.tick(10_000), None);
    }
}
