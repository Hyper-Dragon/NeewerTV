//! The real Bluetooth link, on top of `btleplug` and BlueZ.
//!
//! Deliberately the thinnest layer in the project. Everything interesting —
//! retry policy, coalescing, restore bookkeeping — lives above the [`Link`]
//! trait where it can be tested without a radio. What is left here is the
//! mechanical part: find the device, connect, subscribe, write, read.
//!
//! Two things in here are not mechanical, and both came from measuring the real
//! hardware rather than from reading documentation:
//!
//! * **Every attach rebuilds from the adapter.** It never reuses a `Peripheral`
//!   handle: after a power cycle that handle refers to an object BlueZ has
//!   already destroyed, so reconnecting to it fails forever.
//! * **Ambiguity is refused, not resolved.** If two devices match the name and
//!   no address is configured, this returns an error naming both rather than
//!   driving whichever the scan happened to return first.

use std::pin::Pin;
use std::time::Duration;

use btleplug::api::{Central, Manager as _, Peripheral as _, ScanFilter, WriteType};
use btleplug::platform::{Adapter, Manager, Peripheral};
use futures::{Stream, StreamExt as _};

use crate::ble::discovery;
use crate::ble::link::{BoxFuture, Link, LinkError};
use crate::protocol::command::ID_STATE_REPORT;
use crate::protocol::state::StateReport;

/// The vendor service. Its presence is the identity check: the advertised name
/// is neither unique nor authenticated.
const SERVICE_UUID: &str = "69400001-b5a3-f393-e0a9-e50e24dcca99";
/// Commands are written here.
const WRITE_UUID: &str = "69400002-b5a3-f393-e0a9-e50e24dcca99";
/// Replies arrive here.
const NOTIFY_UUID: &str = "69400003-b5a3-f393-e0a9-e50e24dcca99";

/// How often to re-check the scan results while waiting for the device.
const SCAN_POLL: Duration = Duration::from_millis(400);

type NotificationStream = Pin<Box<dyn Stream<Item = btleplug::api::ValueNotification> + Send>>;

/// Everything an attached link consists of.
struct Attached {
    device: Peripheral,
    write_char: btleplug::api::Characteristic,
    notifications: NotificationStream,
    address: String,
    /// Signal strength at connect, when the host reports one.
    ///
    /// A link that keeps dropping at -55 dBm is a firmware problem and one doing
    /// it at -88 dBm is a physics problem, and from inside the daemon they look
    /// identical. This is the number that tells them apart.
    rssi: Option<i16>,
}

pub struct BluetoothLink {
    name_pattern: String,
    address: Option<String>,
    scan: Duration,
    /// The D-Bus session, opened once and reused for every attempt.
    ///
    /// btleplug opens a connection to bluetoothd for each `Manager`, and
    /// bluetoothd limits connections per UID. Opening one per attach attempt
    /// meant leaking them at exactly the rate of the retry loop: a strip that was
    /// switched off overnight produced 1,308 attempts and then "the maximum
    /// number of active connections for UID 0 has been reached", after which
    /// nothing could connect again until the process was restarted. The daemon
    /// was reporting the host adapter as wedged when it had wedged itself.
    adapter: Option<Adapter>,
    device: Option<Peripheral>,
    write_char: Option<btleplug::api::Characteristic>,
    notifications: Option<NotificationStream>,
    current_address: String,
    rssi: Option<i16>,
}

impl BluetoothLink {
    #[must_use]
    pub fn new(name_pattern: impl Into<String>, address: Option<String>, scan: Duration) -> Self {
        Self {
            name_pattern: name_pattern.into(),
            address,
            scan,
            adapter: None,
            device: None,
            write_char: None,
            notifications: None,
            current_address: String::new(),
            rssi: None,
        }
    }
}

/// Open the host adapter, which also opens the D-Bus session behind it.
async fn open_adapter() -> Result<Adapter, LinkError> {
    let manager = Manager::new()
        .await
        .map_err(|err| LinkError::Connect(format!("Bluetooth manager: {err}")))?;

    manager
        .adapters()
        .await
        .map_err(|err| LinkError::Connect(format!("listing adapters: {err}")))?
        .into_iter()
        .next()
        .ok_or_else(|| {
            LinkError::Connect(
                "no Bluetooth adapter — is it attached, and is bluetoothd running?".to_owned(),
            )
        })
}

/// Whether the failure says the session itself is finished, rather than the strip
/// being absent or the link being briefly unhappy.
///
/// Those are the only cases worth throwing the session away for: dropping a good
/// session on an ordinary "strip not seen" failure would put us back to opening
/// one per attempt, which is the bug this cache exists to fix.
fn is_session_failure(err: &LinkError) -> bool {
    match err {
        LinkError::Connect(message) => {
            let message = message.to_ascii_lowercase();
            message.contains("maximum number of active connections")
                || message.contains("bluetooth manager")
                || message.contains("listing adapters")
        }
        _ => false,
    }
}

/// Find, connect and subscribe. Owns everything it returns.
///
/// A free function rather than a method so the future holds only owned values:
/// keeping `&mut self` alive across these awaits is what makes the future
/// non-`Send`.
async fn connect_device(
    adapter: &Adapter,
    name_pattern: &str,
    address: Option<&str>,
    scan: Duration,
) -> Result<Attached, LinkError> {
    let device = find(adapter, name_pattern, address, scan).await?;
    let address = device.address().to_string();

    device
        .connect()
        .await
        .map_err(|err| LinkError::Connect(err.to_string()))?;

    // From here on the link is held, and every failure has to give it back. A
    // connected device stops advertising, so returning early while still attached
    // makes the strip invisible to the next scan — the very state `clear_stale`
    // exists to undo, and only after a wasted scan and a 1.5 s wait.
    match prepare(&device).await {
        Ok((write_char, notifications)) => {
            let rssi = device
                .properties()
                .await
                .ok()
                .flatten()
                .and_then(|properties| properties.rssi);
            Ok(Attached {
                device,
                write_char,
                notifications,
                address,
                rssi,
            })
        }
        Err(err) => {
            let _ = device.disconnect().await;
            Err(err)
        }
    }
}

/// Everything an attach does after the connect, gathered so the caller has a
/// single place to clean up from.
async fn prepare(
    device: &Peripheral,
) -> Result<(btleplug::api::Characteristic, NotificationStream), LinkError> {
    device
        .discover_services()
        .await
        .map_err(|err| LinkError::Connect(format!("discovering services: {err}")))?;

    let wanted = uuid::Uuid::parse_str(SERVICE_UUID).expect("constant UUID");
    if !device.services().iter().any(|svc| svc.uuid == wanted) {
        return Err(LinkError::Connect(format!(
            "{} does not expose the vendor service — refusing to talk to it",
            device.address()
        )));
    }

    let write_char = find_characteristic(device, WRITE_UUID)?;
    let notify_char = find_characteristic(device, NOTIFY_UUID)?;

    device
        .subscribe(&notify_char)
        .await
        .map_err(|err| LinkError::Connect(format!("subscribing: {err}")))?;
    let notifications = device
        .notifications()
        .await
        .map_err(|err| LinkError::Connect(format!("notification stream: {err}")))?;

    Ok((write_char, notifications))
}

/// Scan until a name match appears, then choose — or refuse.
async fn find(
    adapter: &Adapter,
    name_pattern: &str,
    address: Option<&str>,
    scan: Duration,
) -> Result<Peripheral, LinkError> {
    let mut candidates = scan_for(adapter, name_pattern, scan).await?;

    if candidates.is_empty() {
        // A connected device stops advertising, so "not seen" can mean "BlueZ
        // still thinks we are attached to it" — which is exactly what an
        // ungraceful restart leaves behind. Clear that and look once more before
        // declaring the strip absent.
        if clear_stale(adapter, name_pattern, address).await {
            candidates = scan_for(adapter, name_pattern, scan).await?;
        }
    }

    if candidates.is_empty() {
        return Err(LinkError::Connect(format!(
            "no device matching {name_pattern:?} within {scan:?}"
        )));
    }

    // A configured address wins when it is present.
    if let Some(wanted) = address {
        if let Some(index) = candidates
            .iter()
            .position(|p| p.address().to_string().eq_ignore_ascii_case(wanted))
        {
            return Ok(candidates.remove(index));
        }
        tracing::warn!(
            configured = %wanted,
            found = %candidates[0].address(),
            "configured address not seen — it may have rotated"
        );
    }

    if candidates.len() == 1 {
        return Ok(candidates.remove(0));
    }

    let seen: Vec<String> = candidates.iter().map(|p| p.address().to_string()).collect();
    Err(LinkError::Connect(format!(
        "{} devices match {name_pattern:?} ({}). Refusing to guess — set BLE_DEVICE_ADDRESS.",
        candidates.len(),
        seen.join(", ")
    )))
}

/// One scan pass, returning everything that matches the name pattern.
async fn scan_for(
    adapter: &Adapter,
    name_pattern: &str,
    scan: Duration,
) -> Result<Vec<Peripheral>, LinkError> {
    adapter
        .start_scan(ScanFilter::default())
        .await
        .map_err(|err| LinkError::Connect(format!("starting scan: {err}")))?;

    let deadline = tokio::time::Instant::now() + scan;
    let mut candidates: Vec<Peripheral> = Vec::new();

    while tokio::time::Instant::now() < deadline {
        let peripherals = adapter
            .peripherals()
            .await
            .map_err(|err| LinkError::Connect(format!("listing peripherals: {err}")))?;

        candidates.clear();
        for peripheral in peripherals {
            if let Ok(Some(properties)) = peripheral.properties().await
                && let Some(name) = properties.local_name
                && discovery::matches(name_pattern, &name)
            {
                candidates.push(peripheral);
            }
        }

        if !candidates.is_empty() {
            break;
        }
        tokio::time::sleep(SCAN_POLL).await;
    }

    adapter.stop_scan().await.ok();
    Ok(candidates)
}

/// Disconnect anything BlueZ still believes it is attached to.
///
/// Returns whether anything was released, so the caller knows a rescan is worth
/// doing. This is the recovery for an ungraceful restart: the previous process
/// died holding the single connection, the strip stopped advertising, and no
/// amount of scanning will find it until the link is dropped.
async fn clear_stale(adapter: &Adapter, name_pattern: &str, address: Option<&str>) -> bool {
    let Ok(peripherals) = adapter.peripherals().await else {
        return false;
    };

    // Collect what is actually connected, remembering whether any of it is
    // identifiable.
    let mut connected: Vec<(Peripheral, bool)> = Vec::new();
    for peripheral in peripherals {
        // Match on the cached name, or on a configured address. Note that once a
        // device is connected BlueZ reports the *chip's* name rather than the
        // advertised one — `BK3633_BLE789` instead of `NH-PD20250145` — so a name
        // match alone will miss exactly the case this function exists for.
        let matches_address = address.is_some_and(|wanted| {
            peripheral
                .address()
                .to_string()
                .eq_ignore_ascii_case(wanted)
        });
        let matches_name = match peripheral.properties().await {
            Ok(Some(properties)) => properties
                .local_name
                .is_some_and(|name| discovery::matches(name_pattern, &name)),
            _ => false,
        };

        if peripheral.is_connected().await.unwrap_or(false) {
            connected.push((peripheral, matches_address || matches_name));
        }
    }

    let identified = connected.iter().any(|(_, matched)| *matched);

    // If nothing is identifiable but exactly one device is connected, that is
    // almost certainly ours: we could not find the strip and something is
    // holding the single connection it offers. Refusing to act here means the
    // user has to run `bluetoothctl disconnect` by hand to restart the
    // container, which is not a daemon's job to require.
    let sole_unknown = !identified && connected.len() == 1;

    let mut released = false;
    for (peripheral, matched) in connected {
        if !(matched || sole_unknown) {
            continue;
        }
        tracing::warn!(
            address = %peripheral.address(),
            identified = matched,
            "releasing a link that was holding the device out of reach"
        );
        let _ = peripheral.disconnect().await;
        released = true;
    }

    if released {
        // Give BlueZ a moment to tear the link down before scanning again.
        tokio::time::sleep(Duration::from_millis(1500)).await;
    }
    released
}

fn find_characteristic(
    device: &Peripheral,
    uuid: &str,
) -> Result<btleplug::api::Characteristic, LinkError> {
    let wanted = uuid::Uuid::parse_str(uuid).expect("constant UUID");
    device
        .characteristics()
        .into_iter()
        .find(|characteristic| characteristic.uuid == wanted)
        .ok_or_else(|| LinkError::Connect(format!("characteristic {uuid} not found")))
}

impl Link for BluetoothLink {
    fn attach(&mut self) -> BoxFuture<'_, Result<(), LinkError>> {
        Box::pin(async move {
            let name = self.name_pattern.clone();
            let address = self.address.clone();
            let scan = self.scan;

            // Drop the old handle first: a stale one is worse than none.
            self.device = None;
            self.write_char = None;
            self.notifications = None;
            self.current_address.clear();

            // Reuse the D-Bus session; see the note on the field. Only a session
            // that says it is out of connections is worth replacing, and then
            // only so the next attempt can open a fresh one.
            let adapter = match self.adapter.clone() {
                Some(adapter) => adapter,
                None => {
                    let opened = open_adapter().await?;
                    self.adapter = Some(opened.clone());
                    opened
                }
            };

            let attached = match connect_device(&adapter, &name, address.as_deref(), scan).await {
                Ok(attached) => attached,
                Err(err) => {
                    if is_session_failure(&err) {
                        tracing::warn!(
                            "the Bluetooth session is unusable; opening a fresh one next attempt"
                        );
                        self.adapter = None;
                    }
                    return Err(err);
                }
            };

            self.device = Some(attached.device);
            self.write_char = Some(attached.write_char);
            self.notifications = Some(attached.notifications);
            self.current_address = attached.address;
            self.rssi = attached.rssi;
            Ok(())
        })
    }

    fn write<'a>(&'a mut self, frame: &'a [u8]) -> BoxFuture<'a, Result<(), LinkError>> {
        Box::pin(async move {
            let device = self
                .device
                .as_ref()
                .ok_or_else(|| LinkError::Write("not attached".to_owned()))?;
            let write_char = self
                .write_char
                .as_ref()
                .ok_or_else(|| LinkError::Write("no write characteristic".to_owned()))?;

            device
                .write(write_char, frame, WriteType::WithResponse)
                .await
                .map_err(|err| LinkError::Write(err.to_string()))
        })
    }

    fn next_report<'a>(
        &'a mut self,
        timeout: Duration,
    ) -> BoxFuture<'a, Result<StateReport, LinkError>> {
        Box::pin(async move {
            let stream = self
                .notifications
                .as_mut()
                .ok_or_else(|| LinkError::Write("not attached".to_owned()))?;

            let deadline = tokio::time::Instant::now() + timeout;
            loop {
                let next = tokio::time::timeout_at(deadline, stream.next())
                    .await
                    .map_err(|_| LinkError::NoReply(timeout))?;

                let Some(notification) = next else {
                    return Err(LinkError::Write("notification stream ended".to_owned()));
                };

                // Acknowledgements and anything else share the characteristic,
                // so only a state report counts as an answer.
                if notification.value.get(1) == Some(&ID_STATE_REPORT) {
                    return StateReport::parse(&notification.value).map_err(LinkError::Protocol);
                }
            }
        })
    }

    fn is_up(&mut self) -> BoxFuture<'_, bool> {
        Box::pin(async {
            match &self.device {
                Some(device) => device.is_connected().await.unwrap_or(false),
                None => false,
            }
        })
    }

    fn detach(&mut self) -> BoxFuture<'_, ()> {
        Box::pin(async {
            if let Some(device) = self.device.take() {
                let _ = device.disconnect().await;
            }
            self.write_char = None;
            self.notifications = None;
            self.current_address.clear();
            self.rssi = None;
        })
    }

    fn rssi(&self) -> Option<i16> {
        self.rssi
    }

    fn address(&self) -> String {
        self.current_address.clone()
    }
}
