//! The single owner of the Bluetooth link.
//!
//! One task, one link, one queue. Everything else — MQTT, HTTP, the alert timer —
//! sends messages and reads a snapshot. The device accepts exactly one
//! connection, so a second writer is a defect rather than a race, and this is
//! where that is enforced structurally.

use std::collections::VecDeque;
use std::time::Duration;

use tokio::sync::{mpsc, watch};

use crate::app::alerts::{AlertSpec, Alerts, Fire};
use crate::app::command::{self, Command};
use crate::app::queue::{Batch, Coalescer};
use crate::ble::link::{Link, LinkError};
use crate::ble::recovery::{LinkEvent, Recovery, Step};
use crate::protocol::catalogue::{self, Kind};
use crate::protocol::command as wire;
use crate::protocol::hsi::{Hsi, hsi_to_rgb};

/// Device units (0–1000) to Home Assistant's 0–255.
fn units_to_255(units: u16) -> u8 {
    ((u32::from(units.min(1000)) * 255 + 500) / 1000) as u8
}

/// How long to wait for a state report before deciding the link is dead.
pub const REPLY_TIMEOUT: Duration = Duration::from_secs(5);

/// Attempts made in a row before the worker gives up and lets the caller retry
/// on its next message. Two is deliberate: the first retry is the one measured
/// to rescue an aborted connect, and anything longer belongs in the backoff
/// schedule rather than inside a single command.
const IMMEDIATE_ATTEMPTS: u32 = 2;

/// What the rest of the daemon may ask for.
#[derive(Debug, Clone)]
pub enum Message {
    /// A resolved request from MQTT or HTTP.
    Batch(Batch),
    /// Show an alert.
    Alert(AlertSpec),
    /// Advance the alert clock. Sent on a timer.
    Tick(u64),
    /// Stop any alert, optionally restoring first.
    Cancel { replay: bool },
    /// Release the link and stop. Sent on shutdown, and the reason the process
    /// waits for this task instead of exiting underneath it.
    Shutdown,
}

/// The published view of the daemon, cheap to clone and always current.
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub connected: bool,
    pub address: String,
    pub alert: Option<String>,
    /// What the daemon last commanded. The device does not echo its state, so
    /// this is the only account of it that exists.
    pub on: bool,
    /// 0–255, the scale Home Assistant expects.
    ///
    /// `None` until something establishes it. The device will answer whether it
    /// is powered but not how bright it is, so a freshly connected daemon knows
    /// one and not the other — and publishing `0` for the second turns "not
    /// known" into "on at zero percent".
    pub brightness: Option<u8>,
    /// Likewise the colour, which the device does not echo at all.
    pub colour: Option<(u8, u8, u8)>,
    pub effect: Option<String>,
    pub commands_received: u64,
    pub commands_coalesced: u64,
    pub writes_sent: u64,
    pub last_error: Option<String>,
}

/// Owns the link and everything that decides what to write to it.
pub struct Worker<L: Link> {
    link: L,
    recovery: Recovery,
    alerts: Alerts,
    queue: Coalescer,
    snapshot: Snapshot,
    /// The current time in milliseconds, as last reported by the ticker.
    ///
    /// Every alert decision reads this, so there is exactly one clock. An
    /// earlier version had `Alert` read the wall clock while `Tick` carried its
    /// own — two origins, and an alert that expired the instant it fired.
    now: u64,
    /// Do not attempt to resend the pending batch before this time.
    ///
    /// Set from the recovery policy after a failure. A command is never thrown
    /// away because the link was down when it arrived: it waits here until the
    /// backoff allows another attempt.
    retry_at: u64,
    /// Whether the state query has already been sent on this connection.
    ///
    /// One query per attach, not one per tick: a strip that does not answer must
    /// not be asked every quarter second for the rest of its life.
    state_queried: bool,
}

impl<L: Link> Worker<L> {
    pub fn new(link: L) -> Self {
        Self {
            link,
            recovery: Recovery::new(),
            alerts: Alerts::new(),
            queue: Coalescer::new(),
            snapshot: Snapshot::default(),
            now: 0,
            retry_at: 0,
            state_queried: false,
        }
    }

    /// Consume messages until the channel closes.
    pub async fn run(mut self, mut rx: mpsc::Receiver<Message>, updates: watch::Sender<Snapshot>) {
        let mut deferred: VecDeque<Message> = VecDeque::new();

        loop {
            let message = match deferred.pop_front() {
                Some(message) => message,
                None => match rx.recv().await {
                    Some(message) => message,
                    None => break,
                },
            };

            match message {
                Message::Batch(batch) => {
                    self.snapshot.commands_received += 1;
                    self.queue.push(batch);

                    // Absorb whatever else is already waiting, so a slider drag
                    // becomes one write rather than one per pixel. Anything that
                    // is not a batch is put back and handled in order.
                    while let Ok(next) = rx.try_recv() {
                        match next {
                            Message::Batch(batch) => {
                                self.snapshot.commands_received += 1;
                                self.queue.push(batch);
                            }
                            other => deferred.push_back(other),
                        }
                    }

                    if let Err(err) = self.flush().await {
                        self.snapshot.last_error = Some(err.to_string());
                    }
                }

                Message::Alert(spec) => match self.alerts.fire(&spec, self.now) {
                    Fire::Fired { frames } => {
                        if let Err(err) = self.write_all(&frames).await {
                            self.snapshot.last_error = Some(err.to_string());
                        }
                    }
                    Fire::Rejected => {
                        tracing::debug!(alert = %spec.id, "alert outranked; dropped");
                    }
                },

                Message::Tick(now) => {
                    self.now = now;

                    // Retry anything the link would not take earlier. The
                    // ticker is the clock the backoff runs on.
                    if !self.queue.is_empty() && now >= self.retry_at {
                        if let Err(err) = self.flush().await {
                            self.snapshot.last_error = Some(err.to_string());
                        }
                    }

                    if let Some(frames) = self.alerts.tick(now)
                        && let Err(err) = self.write_all(&frames).await
                    {
                        self.snapshot.last_error = Some(err.to_string());
                    }

                    // Hold the link even when nothing is being asked of it.
                    // Availability follows the link, so letting it go idle
                    // between commands would advertise a reachable light as
                    // unavailable. The backoff is what stops a strip that is
                    // simply unpowered from turning this into a busy loop.
                    if now >= self.retry_at && !self.link.is_up().await {
                        match self.attach().await {
                            Ok(()) => {}
                            Err(err) => {
                                self.snapshot.last_error = Some(err.to_string());
                                self.retry_at = now + self.next_retry_delay_ms();
                                tracing::debug!(error = %err, "idle: no link");
                            }
                        }
                    }

                    // Ask the strip what it is doing, once. An alert that arrives
                    // before anyone has commanded the light still needs something
                    // honest to restore to.
                    if self.link.is_up().await
                        && !self.alerts.baseline_known()
                        && !self.state_queried
                    {
                        self.read_device_state().await;
                    }
                }

                Message::Cancel { replay } => {
                    if let Some(frames) = self.alerts.cancel(replay)
                        && let Err(err) = self.write_all(&frames).await
                    {
                        self.snapshot.last_error = Some(err.to_string());
                    }
                }

                Message::Shutdown => break,
            }

            self.publish(&updates).await;
        }

        // Release the device. A process that exits still holding the link leaves
        // the strip unreachable and not advertising, which is exactly how the
        // next start fails with "no device matching".
        self.link.detach().await;
        self.snapshot.connected = false;
        self.publish(&updates).await;
    }

    /// Ask the strip for its state and adopt what it says.
    ///
    /// The daemon otherwise starts by assuming the strip is off, because there is
    /// no way to know without asking. Asking is one frame and one reply, and it
    /// turns a guess into a fact — which matters most for the restore point an
    /// alert captures before anybody has commanded anything.
    async fn read_device_state(&mut self) {
        self.state_queried = true;
        let query = wire::query_state(true);
        if let Err(err) = self.link.write(&query).await {
            tracing::debug!(%err, "state query could not be written");
            return;
        }

        match self.link.next_report(REPLY_TIMEOUT).await {
            Ok(report) => match report.power() {
                Some(on) => {
                    self.snapshot.on = on;
                    self.alerts.observe_power(on);
                    tracing::info!(on, "learned the strip's state from the device");
                }
                None => tracing::debug!("state report carried no power segment"),
            },
            Err(err) => {
                tracing::debug!(%err, "no state report; the baseline stays unestablished");
            }
        }
    }

    /// Write the newest queued batch, attaching first if necessary.
    async fn flush(&mut self) -> Result<(), LinkError> {
        let Some(batch) = self.queue.take() else {
            return Ok(());
        };

        self.snapshot.commands_coalesced = self.queue.dropped();

        if !self.link.is_up().await {
            match self.attach().await {
                Ok(()) => {}
                Err(err) => {
                    // Keep the instruction. A transient connect failure is not a
                    // reason to forget what the user asked for.
                    self.queue.restore(batch);
                    self.retry_at = self.now + self.next_retry_delay_ms();
                    return Err(err);
                }
            }
        }

        // Encoding a command is deterministic: if it fails it will fail again, so
        // this batch is not worth retrying and the error is the honest outcome.
        let mut frames = Vec::new();
        for command in &batch.commands {
            frames.extend(command::frames(command)?);
        }

        if let Err(err) = self.write_all(&frames).await {
            // The link was up but would not take the write. Nothing reached the
            // device, so the instruction is still outstanding — hold it for the
            // next tick exactly as a failed connect is held.
            self.queue.restore(batch);
            self.retry_at = self.now + self.next_retry_delay_ms();
            return Err(err);
        }

        // Only now is the command real. Recording it before the write succeeded
        // published a state the strip might never have reached.
        for command in &batch.commands {
            self.apply(command);
        }

        // The restore point is what the strip looks like *now*, after this
        // command: an alert put on top of a lit strip must give the lit strip
        // back. `false` here means "replay the frames", which is only right when
        // something is actually on.
        let was_off = !self.snapshot.on;

        // Recording happens only for user commands, never for an alert's own
        // frames — otherwise the restore point would erase itself.
        self.alerts.user_command(frames, was_off);
        Ok(())
    }

    /// How long to wait before the next attempt, from the recovery policy.
    fn next_retry_delay_ms(&mut self) -> u64 {
        match self.recovery.on(LinkEvent::ConnectFailed).step {
            // The policy already granted an immediate retry inside `attach`.
            Step::RetrySame => 500,
            Step::RescanAfter(wait) => wait.as_millis() as u64,
            Step::Idle => 0,
        }
    }

    /// Attach, retrying immediately once.
    async fn attach(&mut self) -> Result<(), LinkError> {
        let mut last = None;

        for _ in 0..IMMEDIATE_ATTEMPTS {
            match self.link.attach().await {
                Ok(()) => {
                    self.recovery.on(LinkEvent::Connected);
                    self.snapshot.connected = true;
                    self.snapshot.address = self.link.address();
                    self.snapshot.last_error = None;
                    // A fresh connection may be a strip that was power-cycled
                    // while we were away, so ask again.
                    self.state_queried = false;
                    tracing::info!(address = %self.snapshot.address, "link up");
                    return Ok(());
                }
                Err(err) => {
                    let outcome = self.recovery.on(LinkEvent::ConnectFailed);
                    tracing::warn!(
                        error = %err,
                        attempt = self.recovery.consecutive_failures(),
                        "connect failed"
                    );
                    if outcome.warn_host_remedy {
                        tracing::warn!(
                            "repeated connect failures — if this persists the host adapter may be \
                             wedged; try `sudo systemctl restart bluetooth` on the host"
                        );
                    }
                    // Only bother retrying when the policy says the next attempt
                    // is immediate; a backoff belongs to the caller's schedule.
                    if outcome.step != Step::RetrySame {
                        last = Some(err);
                        break;
                    }
                    last = Some(err);
                }
            }
        }

        self.snapshot.connected = false;
        Err(last.unwrap_or_else(|| LinkError::Connect("no attempt made".to_owned())))
    }

    async fn write_all(&mut self, frames: &[Vec<u8>]) -> Result<(), LinkError> {
        if !self.link.is_up().await {
            self.attach().await?;
        }

        for frame in frames {
            match self.link.write(frame).await {
                Ok(()) => {
                    self.snapshot.writes_sent += 1;
                }
                Err(err) => {
                    self.recovery.on(LinkEvent::Dropped);
                    self.snapshot.connected = false;
                    self.snapshot.last_error = Some(err.to_string());
                    return Err(err);
                }
            }
        }

        Ok(())
    }

    /// Fold a command into the published state.
    ///
    /// Optimistic by design: the device reports its mode but not its brightness,
    /// speed or colour records, so the daemon's record of what it asked for is
    /// the best account available.
    fn apply(&mut self, command: &Command) {
        match command {
            Command::Power(on) => self.snapshot.on = *on,

            Command::Brightness(units) => {
                self.snapshot.brightness = Some(units_to_255(*units));
                self.snapshot.on = *units > 0;
            }

            Command::Colour { colour, brightness } => {
                self.snapshot.colour = Some(hsi_to_rgb(*colour));
                self.snapshot.brightness = Some(units_to_255(*brightness));
                self.snapshot.on = true;
            }

            Command::Segments {
                entries,
                brightness,
            } => {
                if let Some(&(lightness, hue, saturation)) = entries.first() {
                    self.snapshot.colour = Some(hsi_to_rgb(Hsi {
                        hue,
                        saturation,
                        lightness,
                    }));
                }
                self.snapshot.brightness = Some(units_to_255(*brightness));
                self.snapshot.on = true;
            }

            Command::Effect { effect, .. } => {
                self.snapshot.effect = Some(effect.name.to_owned());
                self.snapshot.on = true;
            }

            Command::Music { mode_id, .. } => {
                self.snapshot.effect =
                    catalogue::by_id(Kind::Music, *mode_id).map(|effect| effect.name.to_owned());
                self.snapshot.on = true;
            }
        }
    }

    async fn publish(&mut self, updates: &watch::Sender<Snapshot>) {
        self.snapshot.alert = self.alerts.active_id().map(str::to_owned);
        let _ = updates.send(self.snapshot.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ble::link::BoxFuture;
    use crate::protocol::command::ID_QUERY_STATE;
    use crate::protocol::state::{Segment, StateReport};
    use std::sync::{Arc, Mutex};

    /// Records what the worker did, and fails on demand.
    #[derive(Default)]
    struct FakeState {
        up: bool,
        attaches: u32,
        fail_attaches: u32,
        /// Writes to reject, counting down. The write is refused *before* it is
        /// recorded, so the reply is not a lie about what reached the device.
        fail_writes: u32,
        /// What a state query answers, if anything. `None` models a strip that
        /// never replies, which is what a real timeout looks like here.
        report: Option<StateReport>,
        /// Command frames.
        writes: Vec<Vec<u8>>,
        /// State queries, counted apart so a test can talk about commands without
        /// having to filter them out of `writes`.
        queries: u32,
    }

    struct FakeLink {
        state: Arc<Mutex<FakeState>>,
    }

    impl FakeLink {
        fn new(fail_attaches: u32) -> (Self, Arc<Mutex<FakeState>>) {
            let state = Arc::new(Mutex::new(FakeState {
                fail_attaches,
                ..FakeState::default()
            }));
            (
                Self {
                    state: Arc::clone(&state),
                },
                state,
            )
        }
    }

    impl Link for FakeLink {
        fn attach(&mut self) -> BoxFuture<'_, Result<(), LinkError>> {
            Box::pin(async {
                let mut state = self.state.lock().unwrap();
                state.attaches += 1;
                if state.attaches <= state.fail_attaches {
                    return Err(LinkError::Connect(
                        "Software caused connection abort".into(),
                    ));
                }
                state.up = true;
                Ok(())
            })
        }

        fn write(&mut self, frame: &[u8]) -> BoxFuture<'_, Result<(), LinkError>> {
            let frame = frame.to_vec();
            Box::pin(async move {
                let mut state = self.state.lock().unwrap();
                if !state.up {
                    return Err(LinkError::Write("not attached".into()));
                }
                if state.fail_writes > 0 {
                    state.fail_writes -= 1;
                    return Err(LinkError::Write("write failed".into()));
                }
                if frame.get(1) == Some(&ID_QUERY_STATE) {
                    state.queries += 1;
                } else {
                    state.writes.push(frame);
                }
                Ok(())
            })
        }

        fn next_report(
            &mut self,
            _timeout: Duration,
        ) -> BoxFuture<'_, Result<StateReport, LinkError>> {
            Box::pin(async {
                let state = self.state.lock().unwrap();
                match &state.report {
                    Some(report) => Ok(report.clone()),
                    None => Err(LinkError::NoReply(Duration::ZERO)),
                }
            })
        }

        fn is_up(&mut self) -> BoxFuture<'_, bool> {
            Box::pin(async { self.state.lock().unwrap().up })
        }

        fn detach(&mut self) -> BoxFuture<'_, ()> {
            Box::pin(async {
                self.state.lock().unwrap().up = false;
            })
        }

        fn address(&self) -> String {
            PLACEHOLDER_ADDRESS.to_owned()
        }
    }

    fn brightness(level: u16) -> Batch {
        Batch {
            commands: vec![Command::Brightness(level)],
        }
    }

    /// Synthetic, and deliberately not any real device's address.
    const PLACEHOLDER_ADDRESS: &str = "AA:BB:CC:DD:EE:FF";

    async fn drain(worker: Worker<FakeLink>, messages: Vec<Message>) -> Arc<Mutex<FakeState>> {
        let state = Arc::clone(&worker.link.state);
        let (tx, rx) = mpsc::channel(16);
        for message in messages {
            tx.send(message).await.unwrap();
        }
        drop(tx);

        let (updates, _) = watch::channel(Snapshot::default());
        worker.run(rx, updates).await;
        state
    }

    /// Send one message at a time, letting the worker finish each.
    ///
    /// [`drain`] pre-fills the channel, and the worker then absorbs everything
    /// already waiting into a single coalesced batch. That is the right model for
    /// a slider drag but useless for testing what happens *after* one command has
    /// been written, which needs a real gap between messages.
    ///
    /// Returns what the fake recorded and the last snapshot the worker published.
    async fn drain_paced(
        worker: Worker<FakeLink>,
        messages: Vec<Message>,
    ) -> (Arc<Mutex<FakeState>>, Snapshot) {
        let state = Arc::clone(&worker.link.state);
        let (tx, rx) = mpsc::channel(16);
        let (updates, mut published) = watch::channel(Snapshot::default());
        let handle = tokio::spawn(worker.run(rx, updates));

        for message in messages {
            tx.send(message).await.unwrap();
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        drop(tx);
        handle.await.unwrap();

        let last = published.borrow_and_update().clone();
        (state, last)
    }

    /// A state report carrying just a power segment.
    fn report_with_power(on: bool) -> StateReport {
        StateReport {
            mode: 0,
            segments: vec![Segment {
                kind: 0,
                payload: vec![u8::from(on)],
            }],
        }
    }

    #[tokio::test]
    async fn a_queued_batch_is_written() {
        let (link, _) = FakeLink::new(0);
        let state = drain(Worker::new(link), vec![Message::Batch(brightness(500))]).await;

        let state = state.lock().unwrap();
        assert_eq!(state.attaches, 1, "attached before writing");
        assert_eq!(state.writes.len(), 1);
        assert_eq!(state.writes[0][1], 0x0B, "brightness frame");
    }

    #[tokio::test]
    async fn batches_that_arrive_together_become_one_write() {
        // A slider drag: every message is a complete statement of intent, so
        // only the last one matters and only it is sent.
        let (link, _) = FakeLink::new(0);
        let state = drain(
            Worker::new(link),
            vec![
                Message::Batch(brightness(100)),
                Message::Batch(brightness(200)),
                Message::Batch(brightness(900)),
            ],
        )
        .await;

        let state = state.lock().unwrap();
        assert_eq!(state.writes.len(), 1, "coalesced into a single write");
        assert_eq!(state.writes[0][3], 0x00, "reserved byte");
        assert_eq!(
            state.writes[0][4], 90,
            "brightness 900 as tens — 90 decimal, not 0x90"
        );
    }

    #[tokio::test]
    async fn a_failed_connect_is_retried_once_before_giving_up() {
        let (link, _) = FakeLink::new(1);
        let state = drain(Worker::new(link), vec![Message::Batch(brightness(500))]).await;

        let state = state.lock().unwrap();
        assert_eq!(state.attaches, 2, "one retry, per the measured behaviour");
        assert_eq!(state.writes.len(), 1, "and the command still got through");
    }

    #[tokio::test]
    async fn a_persistently_failing_connect_does_not_wedge_the_worker() {
        let (link, _) = FakeLink::new(99);
        let state = drain(Worker::new(link), vec![Message::Batch(brightness(500))]).await;

        let state = state.lock().unwrap();
        assert_eq!(state.attaches, IMMEDIATE_ATTEMPTS);
        assert!(state.writes.is_empty());
    }

    #[tokio::test]
    async fn an_alert_is_written_and_then_the_strip_is_put_back() {
        let (link, _) = FakeLink::new(0);
        let state = drain(
            Worker::new(link),
            vec![
                Message::Batch(Batch {
                    commands: vec![Command::Power(true)],
                }),
                Message::Alert(AlertSpec {
                    id: "doorbell".into(),
                    priority: 50,
                    duration_ms: 1_000,
                    force_on: true,
                    frames: vec![vec![0xAA, 0x01]],
                }),
                Message::Tick(2_000),
            ],
        )
        .await;

        let state = state.lock().unwrap();
        assert_eq!(
            state.writes,
            vec![
                vec![0x7A, 0x0A, 0x01, 0x01, 0x86],
                vec![0xAA, 0x01],
                vec![0x7A, 0x0A, 0x01, 0x01, 0x86],
            ],
            "power on, alert, then restore"
        );
    }

    #[tokio::test]
    async fn a_command_survives_a_failed_write() {
        // The link was up and took the connect, then refused the write. That is
        // the same lost-instruction situation as a failed connect, and the same
        // answer applies: hold it and try again on the next tick.
        let (link, state) = FakeLink::new(0);
        state.lock().unwrap().fail_writes = 1;

        let state = drain(
            Worker::new(link),
            vec![Message::Batch(brightness(500)), Message::Tick(5_000)],
        )
        .await;

        let state = state.lock().unwrap();
        assert_eq!(state.writes.len(), 1, "the retry got the command through");
        assert_eq!(state.writes[0][1], 0x0B, "and it is the brightness frame");
    }

    #[tokio::test]
    async fn an_alert_restores_a_lit_strip_even_when_the_last_command_was_brightness_only() {
        // Home Assistant sends a brightness on its own when a slider is dragged,
        // so such a command says nothing about power. The restore point has to
        // come from what the strip is doing, not from what the payload happened
        // to mention.
        let (link, _) = FakeLink::new(0);
        let (state, _) = drain_paced(
            Worker::new(link),
            vec![
                Message::Batch(Batch {
                    commands: vec![Command::Power(true)],
                }),
                Message::Batch(brightness(400)),
                Message::Alert(AlertSpec {
                    id: "doorbell".into(),
                    priority: 50,
                    duration_ms: 1_000,
                    force_on: true,
                    frames: vec![vec![0xAA, 0x01]],
                }),
                Message::Tick(2_000),
            ],
        )
        .await;

        let state = state.lock().unwrap();
        assert_eq!(state.writes.len(), 4, "on, brightness, alert, restore");
        assert_eq!(
            state.writes[3], state.writes[1],
            "the strip was lit, so putting it back means the brightness it had"
        );
    }

    #[tokio::test]
    async fn the_strip_is_asked_what_it_is_doing_rather_than_assumed_off() {
        // Nothing has been commanded, so the daemon cannot know. The tick that
        // brings the link up is also the moment to ask.
        let (link, fake) = FakeLink::new(0);
        fake.lock().unwrap().report = Some(report_with_power(true));

        let (state, snapshot) = drain_paced(Worker::new(link), vec![Message::Tick(0)]).await;

        assert_eq!(state.lock().unwrap().queries, 1, "asked once");
        assert!(
            snapshot.on,
            "the strip said it was on, so that is what the daemon believes"
        );
    }

    #[tokio::test]
    async fn an_unanswered_query_leaves_the_baseline_unestablished() {
        // No guess in either direction: still off in the published snapshot, and
        // still unknown to the alert bookkeeping.
        let (link, _) = FakeLink::new(0);
        let (state, snapshot) = drain_paced(
            Worker::new(link),
            vec![Message::Tick(0), Message::Tick(250), Message::Tick(500)],
        )
        .await;

        assert_eq!(
            state.lock().unwrap().queries,
            1,
            "one query per connection, not one per tick"
        );
        assert!(!snapshot.on);
    }

    #[tokio::test]
    async fn the_link_is_held_while_idle() {
        // Availability is derived from the link, so an idle daemon that dropped
        // it would report a reachable light as unavailable.
        let (link, fake) = FakeLink::new(0);
        fake.lock().unwrap().report = Some(report_with_power(true));

        let (state, snapshot) = drain_paced(
            Worker::new(link),
            vec![Message::Tick(0), Message::Tick(250), Message::Tick(500)],
        )
        .await;

        let state = state.lock().unwrap();
        assert_eq!(state.attaches, 1, "attached once, then stayed attached");
        assert_eq!(state.writes.len(), 0, "holding the link writes no commands");
        drop(state);
        assert!(!snapshot.connected, "released on the way out");
    }

    #[tokio::test]
    async fn shutdown_releases_the_device() {
        // The strip accepts one connection. A process that exits while still
        // holding it leaves the device unreachable and not advertising, which is
        // exactly how the next start fails with "no device matching".
        let (link, _) = FakeLink::new(0);
        let state = drain(
            Worker::new(link),
            vec![Message::Batch(brightness(500)), Message::Shutdown],
        )
        .await;

        let state = state.lock().unwrap();
        assert!(!state.up, "the link must be released on shutdown");
    }

    #[tokio::test]
    async fn a_command_survives_a_link_that_will_not_connect() {
        // Two immediate attempts fail, so the batch must be held rather than
        // discarded: the user asked for a brightness, and a transient abort is
        // not a reason to silently ignore them.
        let (link, _) = FakeLink::new(2);
        let state = drain(
            Worker::new(link),
            vec![Message::Batch(brightness(500)), Message::Tick(5_000)],
        )
        .await;

        let state = state.lock().unwrap();
        // Three attach attempts: the two immediate ones, then the retry after
        // the backoff. `up` is false here only because the worker detaches when
        // it finishes.
        assert_eq!(state.attaches, 3, "retried after the backoff");
        assert_eq!(state.writes.len(), 1, "the command was not lost");
        assert_eq!(state.writes[0][1], 0x0B, "and it is the brightness frame");
    }
}
