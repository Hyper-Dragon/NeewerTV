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
use crate::protocol::state::StateReport;

/// Device units (0–1000) to Home Assistant's 0–255.
fn units_to_255(units: u16) -> u8 {
    ((u32::from(units.min(1000)) * 255 + 500) / 1000) as u8
}

/// Home Assistant's 0–255 back to device units, for rebuilding a restore.
fn level_255_to_units(level: u8) -> u16 {
    ((u32::from(level) * 1000 + 127) / 255) as u16
}

/// The effect the device says it is playing, if the report names one.
///
/// The report's mode is what makes this safe. The same identifier means a scene
/// in one mode and a preset in another, so looking it up across the whole
/// catalogue would be a guess that publishes the wrong name with confidence.
/// Only the modes whose identifiers *are* catalogue effects are mapped, and
/// everything else — colour, music, the undocumented modes — reports nothing,
/// because nothing is what we know.
fn running_effect(report: &StateReport) -> Option<&'static str> {
    let id = report.scene_id()?;
    let kind = match report.mode {
        3 => Kind::Scene,
        5 => Kind::Diy,
        9 => Kind::TwoWay,
        15 => Kind::Preset,
        _ => return None,
    };

    catalogue::by_id(kind, id).map(|effect| effect.name)
}

/// How long to wait for a state report before deciding the link is dead.
pub const REPLY_TIMEOUT: Duration = Duration::from_secs(5);

/// Attempts made in a row before the worker gives up and lets the caller retry
/// on its next message. Two is deliberate: the first retry is the one measured
/// to rescue an aborted connect, and anything longer belongs in the backoff
/// schedule rather than inside a single command.
const IMMEDIATE_ATTEMPTS: u32 = 2;

/// How long a keep-alive waits for its own reply before giving up on it.
///
/// The answer is discarded either way; the wait only exists so the worker does
/// not walk away leaving a reply in the stream, and it must stay short enough
/// that a strip which stops answering cannot delay a real command.
const KEEPALIVE_REPLY_MS: u64 = 250;

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
    /// Signal strength at the last attach, when the host reports one.
    pub rssi: Option<i16>,
    /// How many times a link that was up was found down again.
    ///
    /// The daemon reconnects silently and cheaply, which is right — but silence
    /// makes a strip that is dropping every few minutes look like a daemon that
    /// is connecting for no reason. Counting them turns that into one number.
    pub drops: u64,
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
    /// When the current link came up, on the ticker's clock.
    ///
    /// A drop reads very differently at two seconds and at forty minutes: the
    /// first is a link that never really formed, the second is a strip that lost
    /// power. Without this the log said "link lost" and left the operator to
    /// guess which.
    up_since: u64,
    /// How often to poke an otherwise idle link, zero for never.
    ///
    /// A workaround for firmware that closes a connection it hears nothing on,
    /// which some of these controllers do after a minute or two. Off by default:
    /// it is traffic the strip did not ask for, and most strips do not need it.
    keepalive_ms: u64,
    /// When the link last carried anything, keep-alive included.
    last_traffic: u64,
}

/// Why an attach or a write failed, carrying the decision it earned.
///
/// The step travels with the error deliberately. Deriving the wait by asking the
/// policy *again* meant every failure was recorded twice, so the backoff climbed
/// twice as fast as it is designed to and the host-remedy warning fired at half
/// the failure count it documents.
struct AttachFailure {
    error: LinkError,
    step: Step,
}

/// How long to wait before the next attempt, from a step already decided.
fn retry_delay(step: Step) -> u64 {
    match step {
        // Only reachable if the immediate retry budget were ever exhausted.
        Step::RetrySame => 500,
        Step::RescanAfter(wait) => wait.as_millis() as u64,
        Step::Idle => 0,
    }
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
            up_since: 0,
            keepalive_ms: 0,
            last_traffic: 0,
        }
    }

    /// Poke an idle link every `every` so firmware that closes quiet connections
    /// has something to hear. Zero, the default, leaves the link alone.
    #[must_use]
    pub fn with_keepalive(mut self, every: Duration) -> Self {
        self.keepalive_ms = every.as_millis() as u64;
        self
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
                        if let Err(failure) = self.write_all(&frames).await {
                            self.snapshot.last_error = Some(failure.error.to_string());
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
                        && let Err(failure) = self.write_all(&frames).await
                    {
                        self.snapshot.last_error = Some(failure.error.to_string());
                    }

                    // Hold the link even when nothing is being asked of it.
                    // Availability follows the link, so letting it go idle
                    // between commands would advertise a reachable light as
                    // unavailable. The backoff is what stops a strip that is
                    // simply unpowered from turning this into a busy loop.
                    //
                    // One poll, used twice: a link that was up and is now down
                    // is a drop worth saying out loud, and it is also the
                    // trigger to attach again.
                    let link_up = self.link.is_up().await;
                    if !link_up && self.snapshot.connected {
                        self.snapshot.connected = false;
                        self.snapshot.drops += 1;
                        // The reading describes a link that no longer exists.
                        self.snapshot.rssi = None;
                        tracing::info!(
                            address = %self.snapshot.address,
                            drops = self.snapshot.drops,
                            held_seconds = self.now.saturating_sub(self.up_since) / 1000,
                            "link lost; reconnecting"
                        );
                    }

                    // Something for an idle link to hear, when the strip is the
                    // sort that hangs up on silence.
                    if link_up
                        && self.keepalive_ms > 0
                        && self.now.saturating_sub(self.last_traffic) >= self.keepalive_ms
                    {
                        self.last_traffic = self.now;
                        match self.link.write(&wire::query_state(true)).await {
                            Ok(()) => {
                                // Read the answer, and discard it. Nobody asked a
                                // question here — this is housekeeping — but an
                                // unread reply per keep-alive would sit in the
                                // notification stream for ever. Short timeout:
                                // a slow strip must not hold the worker.
                                let _ = self
                                    .link
                                    .next_report(Duration::from_millis(KEEPALIVE_REPLY_MS))
                                    .await;
                            }
                            Err(err) => {
                                tracing::debug!(%err, "keep-alive could not be written");
                            }
                        }
                    }

                    if now >= self.retry_at && !link_up {
                        match self.attach().await {
                            Ok(()) => {}
                            Err(failure) => {
                                self.snapshot.last_error = Some(failure.error.to_string());
                                self.retry_at = now + retry_delay(failure.step);
                                tracing::debug!(error = %failure.error, "idle: no link");
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
                        && let Err(failure) = self.write_all(&frames).await
                    {
                        self.snapshot.last_error = Some(failure.error.to_string());
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
        self.last_traffic = self.now;

        match self.link.next_report(REPLY_TIMEOUT).await {
            Ok(report) => {
                match report.power() {
                    Some(on) => {
                        self.snapshot.on = on;
                        self.alerts.observe_power(on);
                        tracing::info!(on, "learned the strip's state from the device");
                    }
                    None => tracing::debug!("state report carried no power segment"),
                }

                // The report carries a brightness too, but only in the mode that
                // has one to report — DIY. Take it when it is there and when
                // nothing better is known: a commanded brightness is the daemon's
                // own record of what it asked for, and a measurement must not
                // overwrite it.
                if self.snapshot.brightness.is_none()
                    && let Some(percent) = report.diy_brightness_pct()
                {
                    let level = units_to_255(u16::from(percent).min(100) * 10);
                    self.snapshot.brightness = Some(level);
                    tracing::info!(
                        percent,
                        brightness_255 = level,
                        "learned the strip's brightness from the device"
                    );
                }

                // And what it is playing. The device does not render an effect's
                // colours back to us, but it does name the effect it is running,
                // so Home Assistant can say which one rather than showing nothing.
                if self.snapshot.effect.is_none()
                    && self.snapshot.on
                    && let Some(name) = running_effect(&report)
                {
                    self.snapshot.effect = Some(name.to_owned());
                    tracing::info!(effect = name, "learned the running effect from the device");
                }
            }
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
                Err(failure) => {
                    // Keep the instruction. A transient connect failure is not a
                    // reason to forget what the user asked for.
                    self.queue.restore(batch);
                    self.retry_at = self.now + retry_delay(failure.step);
                    return Err(failure.error);
                }
            }
        }

        // Encoding a command is deterministic: if it fails it will fail again, so
        // this batch is not worth retrying and the error is the honest outcome.
        let mut frames = Vec::new();
        for command in &batch.commands {
            frames.extend(command::frames(command)?);
        }

        if let Err(failure) = self.write_all(&frames).await {
            // The link was up but would not take the write. Nothing reached the
            // device, so the instruction is still outstanding — hold it for the
            // next tick exactly as a failed connect is held.
            self.queue.restore(batch);
            self.retry_at = self.now + retry_delay(failure.step);
            return Err(failure.error);
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

        // A command that only said "on" leaves the strip at whatever brightness
        // it had, and its frames therefore say nothing about brightness — while
        // every alert ends by dimming the strip to nothing. Replaying the power
        // frame alone would give back a strip that is on at zero percent, which
        // reads as "the alert turned my light off". Put the brightness the daemon
        // knows about on the end of the restore so that cannot happen.
        let mut restore = frames;
        let mentions_light = batch
            .commands
            .iter()
            .any(|command| !matches!(command, Command::Power(_)));
        if !mentions_light
            && let Some(level) = self.snapshot.brightness
            && let Ok(frame) = wire::brightness(level_255_to_units(level))
        {
            restore.push(frame);
        }

        // Recording happens only for user commands, never for an alert's own
        // frames — otherwise the restore point would erase itself.
        self.alerts.user_command(restore, was_off);
        Ok(())
    }

    /// Attach, retrying immediately once.
    async fn attach(&mut self) -> Result<(), AttachFailure> {
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
                    self.up_since = self.now;
                    self.snapshot.rssi = self.link.rssi();
                    tracing::info!(
                        address = %self.snapshot.address,
                        rssi = ?self.snapshot.rssi,
                        "link up"
                    );
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
                            "repeated connect failures — the daemon keeps retrying by itself, so \
                             this is only worth acting on if the strip has power. If it does, check \
                             `bluetoothctl show` on the host; a wedged adapter is the usual cause, \
                             and the vendor app holding the single connection is the next"
                        );
                    }
                    last = Some(AttachFailure {
                        error: err,
                        step: outcome.step,
                    });
                    // Only bother retrying when the policy says the next attempt
                    // is immediate; a backoff belongs to the caller's schedule.
                    if outcome.step != Step::RetrySame {
                        break;
                    }
                }
            }
        }

        self.snapshot.connected = false;
        Err(last.unwrap_or_else(|| AttachFailure {
            error: LinkError::Connect("no attempt made".to_owned()),
            step: Step::RescanAfter(Duration::ZERO),
        }))
    }

    async fn write_all(&mut self, frames: &[Vec<u8>]) -> Result<(), AttachFailure> {
        if !self.link.is_up().await {
            self.attach().await?;
        }

        for frame in frames {
            match self.link.write(frame).await {
                Ok(()) => {
                    self.snapshot.writes_sent += 1;
                    self.last_traffic = self.now;
                }
                Err(err) => {
                    // The link was up and would not take the write. This is the
                    // one place that failure is counted; the caller only needs
                    // the decision it earned.
                    let outcome = self.recovery.on(LinkEvent::Dropped);
                    self.snapshot.connected = false;
                    self.snapshot.last_error = Some(err.to_string());
                    return Err(AttachFailure {
                        error: err,
                        step: outcome.step,
                    });
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
                // Colour mode is not an effect mode, so the effect name is no
                // longer true. Leaving it here kept Home Assistant showing the
                // effect as still selected: picking a colour then looked like it
                // had done nothing, and re-selecting the effect did nothing
                // either, because Home Assistant saw no change to send.
                self.snapshot.effect = None;
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
                // Segmented colour is the colour command in another shape, so the
                // same reasoning applies as above.
                self.snapshot.effect = None;
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
        /// What the link reports as its signal strength.
        rssi: Option<i16>,
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

        fn rssi(&self) -> Option<i16> {
            self.state.lock().unwrap().rssi
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

    /// A report that also carries the DIY brightness, as a real strip in colour
    /// mode does.
    fn report_with_brightness(on: bool, percent: u8) -> StateReport {
        let mut report = report_with_power(on);
        report.segments.push(Segment {
            kind: 5,
            payload: vec![percent, 0],
        });
        report
    }

    /// A report from a strip playing a scene: the mode decides what the id means.
    fn report_with_scene(on: bool, mode: u8, scene: u8) -> StateReport {
        let mut report = report_with_power(on);
        report.mode = mode;
        report.segments.push(Segment {
            kind: 3,
            payload: vec![0, scene, 100],
        });
        report
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
    async fn a_failure_is_counted_once_so_the_wait_matches_the_policy() {
        // Every attach fails, so the immediate retry is made and two failures
        // have happened. The wait must be the one the policy sets for two.
        let (link, state) = FakeLink::new(u32::MAX);
        let mut worker = Worker::new(link);
        worker.queue.push(brightness(500));

        worker.flush().await.unwrap_err();

        let attempts = state.lock().unwrap().attaches;
        assert_eq!(attempts, IMMEDIATE_ATTEMPTS, "the immediate retry happened");
        assert_eq!(
            worker.recovery.consecutive_failures(),
            attempts,
            "deriving the wait used to count the same failure again, so the log \
             read 1, 2, 4, 6 and the backoff escalated at twice its rate"
        );
        assert_eq!(
            worker.retry_at, 2_000,
            "two seconds for the second failure — eight would mean four were counted"
        );
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
    async fn an_alert_gives_back_the_brightness_of_a_strip_that_was_only_told_to_turn_on() {
        // `{"state":"ON"}` with no brightness is what half of Home Assistant's
        // automations send. Its frames say nothing about brightness — and every
        // alert ends by dimming the strip to nothing — so replaying that command
        // alone gave back a strip that was on at zero percent: from the sofa,
        // indistinguishable from the alert having switched the light off.
        let (link, fake) = FakeLink::new(0);
        fake.lock().unwrap().report = Some(report_with_power(true));

        let (state, _) = drain_paced(
            Worker::new(link),
            vec![
                Message::Batch(Batch {
                    commands: vec![Command::Brightness(1000)],
                }),
                Message::Batch(Batch {
                    commands: vec![Command::Power(true)],
                }),
                Message::Alert(AlertSpec {
                    id: "red".into(),
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
        let last = state.writes.len() - 1;
        assert_eq!(
            state.writes[last],
            wire::brightness(1000).expect("valid brightness"),
            "the restore has to put the brightness back, not just the power"
        );
        assert_eq!(
            state.writes[last - 1],
            wire::power(true),
            "and the power before it"
        );
    }

    #[tokio::test]
    async fn a_brightness_the_device_reports_is_adopted() {
        // The report only carries one in DIY mode, but when it does, taking it is
        // the difference between an alert restoring the real brightness and
        // restoring a guess.
        let (link, fake) = FakeLink::new(0);
        fake.lock().unwrap().report = Some(report_with_brightness(true, 75));

        let (_, snapshot) = drain_paced(Worker::new(link), vec![Message::Tick(0)]).await;

        let expected = units_to_255(750);
        assert_eq!(snapshot.brightness, Some(expected), "75 % as 0–255");
        assert!(snapshot.on);
    }

    #[tokio::test]
    async fn a_reported_brightness_never_overwrites_a_commanded_one() {
        // The daemon's record of what it asked for outranks a re-read: a reconnect
        // must not quietly change the brightness Home Assistant was told about.
        let (link, fake) = FakeLink::new(0);
        fake.lock().unwrap().report = Some(report_with_brightness(true, 75));

        let (_, snapshot) = drain_paced(
            Worker::new(link),
            vec![Message::Batch(brightness(400)), Message::Tick(1_000)],
        )
        .await;

        assert_eq!(snapshot.brightness, Some(units_to_255(400)));
    }

    #[tokio::test]
    async fn the_running_effect_is_named_rather_than_left_blank() {
        // Home Assistant cannot show the colours an effect is rendering — the
        // device never reports them — but it can say which effect is running, and
        // "nothing" is a worse answer than the name.
        let (link, fake) = FakeLink::new(0);
        fake.lock().unwrap().report = Some(report_with_scene(true, 3, 0x31));

        let (_, snapshot) = drain_paced(Worker::new(link), vec![Message::Tick(0)]).await;

        assert_eq!(snapshot.effect.as_deref(), Some("Icelandic Blue"));
    }

    #[tokio::test]
    async fn an_identifier_in_a_mode_with_no_effect_is_not_guessed() {
        // Mode 2 is colour: whatever the type-3 segment says, it is not the effect
        // on screen, and publishing a name here would be a confidently wrong
        // answer.
        let (link, fake) = FakeLink::new(0);
        fake.lock().unwrap().report = Some(report_with_scene(true, 2, 0x31));

        let (_, snapshot) = drain_paced(Worker::new(link), vec![Message::Tick(0)]).await;

        assert_eq!(snapshot.effect, None, "a colour has no effect name");
    }

    #[tokio::test]
    async fn a_commanded_effect_is_not_replaced_by_the_reports_opinion() {
        // The daemon's record of what it was told outranks a re-read, exactly as
        // it does for power: a reconnect must not rename what Home Assistant was
        // just told is playing.
        let (link, fake) = FakeLink::new(0);
        fake.lock().unwrap().report = Some(report_with_scene(true, 3, 0x31));

        let effect = catalogue::by_name("Starstream").expect("in the catalogue");
        let (_, snapshot) = drain_paced(
            Worker::new(link),
            vec![
                Message::Batch(Batch {
                    commands: vec![Command::Effect {
                        effect,
                        brightness: 500,
                        speed: 0,
                    }],
                }),
                Message::Tick(1_000),
            ],
        )
        .await;

        assert_eq!(snapshot.effect.as_deref(), Some("Starstream"));
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
    async fn a_keep_alive_pokes_an_idle_link() {
        // Some controllers hang up on a connection they hear nothing on. The
        // keep-alive is opt-in, so it must do nothing at all by default and
        // exactly one query per interval when it is asked for.
        let (link, fake) = FakeLink::new(0);
        fake.lock().unwrap().report = Some(report_with_power(true));

        let (state, _) = drain_paced(
            Worker::new(link).with_keepalive(Duration::from_secs(30)),
            vec![
                Message::Tick(0),
                Message::Tick(10_000),
                Message::Tick(31_000),
            ],
        )
        .await;

        let state = state.lock().unwrap();
        assert_eq!(
            state.queries, 2,
            "one on attach, one keep-alive — not one per tick"
        );
        assert!(state.writes.is_empty(), "and nothing that changes anything");
    }

    #[tokio::test]
    async fn no_keep_alive_traffic_without_being_asked_for_it() {
        let (link, fake) = FakeLink::new(0);
        fake.lock().unwrap().report = Some(report_with_power(true));

        let (state, _) = drain_paced(
            Worker::new(link),
            vec![Message::Tick(0), Message::Tick(600_000)],
        )
        .await;

        assert_eq!(
            state.lock().unwrap().queries,
            1,
            "the attach query only, for ten minutes of silence"
        );
    }

    #[tokio::test]
    async fn the_signal_strength_at_connect_is_reported() {
        // Both "the strip hangs up" and "the host is in a metal box" look
        // identical from inside the daemon; the number is what separates them.
        let (link, fake) = FakeLink::new(0);
        fake.lock().unwrap().rssi = Some(-72);

        let (_, snapshot) =
            drain_paced(Worker::new(link), vec![Message::Batch(brightness(500))]).await;

        assert_eq!(snapshot.rssi, Some(-72));
    }

    #[tokio::test]
    async fn a_lost_link_is_counted_instead_of_being_quietly_reestablished() {
        // The strip going away looks, from the outside, exactly like a daemon
        // that reconnects for no reason: the log said "link up" every few
        // minutes and nothing said why. One number and one log line fix that.
        let (link, fake) = FakeLink::new(0);
        let (tx, rx) = mpsc::channel(16);
        let (updates, mut published) = watch::channel(Snapshot::default());
        let handle = tokio::spawn(Worker::new(link).run(rx, updates));

        tx.send(Message::Batch(brightness(500))).await.unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(published.borrow_and_update().connected, "up to begin with");

        // The strip loses power, or walks out of range. Nothing announces it;
        // the next poll is what notices.
        fake.lock().unwrap().up = false;
        tx.send(Message::Tick(1_000)).await.unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;

        // Read it before the channel closes: the last thing the worker publishes
        // on the way out is the shutdown snapshot, where the link is down by
        // definition and the count would tell us nothing new.
        let snapshot = published.borrow_and_update().clone();
        assert_eq!(snapshot.drops, 1, "the drop is counted, not just retried");
        assert!(snapshot.connected, "back up by the time it published");
        assert_eq!(
            snapshot.rssi, None,
            "a reading from the old link is not a reading about the new one"
        );

        drop(tx);
        handle.await.unwrap();

        assert_eq!(
            fake.lock().unwrap().attaches,
            2,
            "the drop cost one reattach, which is the cheap part"
        );
    }

    #[tokio::test]
    async fn picking_a_colour_clears_the_effect_home_assistant_shows() {
        // Colour mode is not an effect mode. Leaving the name in the published
        // state kept Home Assistant's effect list showing the effect as still
        // selected, so choosing a colour looked like it had been ignored and the
        // effect could not be re-selected without going via a different one.
        let (link, _) = FakeLink::new(0);
        let effect =
            crate::protocol::catalogue::by_name("TV Screen (two-way)").expect("must be catalogued");

        let (_, snapshot) = drain_paced(
            Worker::new(link),
            vec![
                Message::Batch(Batch {
                    commands: vec![Command::Effect {
                        effect,
                        brightness: 1000,
                        speed: 50,
                    }],
                }),
                Message::Batch(Batch {
                    commands: vec![Command::Colour {
                        colour: Hsi {
                            hue: 120,
                            saturation: 100,
                            lightness: 50,
                        },
                        brightness: 1000,
                    }],
                }),
            ],
        )
        .await;

        assert!(
            snapshot.effect.is_none(),
            "the effect is no longer what the strip is doing"
        );
        assert!(snapshot.colour.is_some(), "but the colour is");
    }

    #[tokio::test]
    async fn brightness_alone_does_not_clear_the_effect() {
        // Brightness belongs to whatever mode is running, so dragging the slider
        // must not knock Home Assistant out of the effect it has selected.
        let (link, _) = FakeLink::new(0);
        let effect = crate::protocol::catalogue::by_name("Superfluid").expect("must be catalogued");

        let (_, snapshot) = drain_paced(
            Worker::new(link),
            vec![
                Message::Batch(Batch {
                    commands: vec![Command::Effect {
                        effect,
                        brightness: 1000,
                        speed: 50,
                    }],
                }),
                Message::Batch(brightness(400)),
            ],
        )
        .await;

        assert_eq!(snapshot.effect.as_deref(), Some("Superfluid"));
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
