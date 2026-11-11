//! MQTT: the daemon's whole outside world.
//!
//! Three things come out — a retained discovery payload so Home Assistant finds
//! the light, retained state, and availability — and two things go in, `set` and
//! `alert/#`.
//!
//! The payload builders are pure functions and are tested; the connection
//! plumbing around them is deliberately dull.

use rumqttc::{AsyncClient, Event, LastWill, MqttOptions, Packet, QoS};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::app::alerts::AlertSpec;
use crate::app::command::{self, SetPayload};
use crate::app::palette;
use crate::ble::worker::Snapshot;
use crate::config::Config;
use crate::homeassistant::{self, discovery, topic};
use crate::protocol::catalogue;
use crate::protocol::command as wire;

/// Shown in Home Assistant's device page. The vendor, not us — and nominative:
/// this project is not affiliated with them.
const MANUFACTURER: &str = "NEEWER";

/// The product family. The model suffix differs by strip length, so it is not
/// claimed here.
const MODEL: &str = "NS03";

/// Build the retained discovery message body.
///
/// `ambient` says whether this deployment can follow the camera at all, which is
/// the same question as whether a calibration exists. Home Assistant is offered
/// the mode only when it would work: an effect in the list that does nothing is
/// worse than one that is absent.
#[must_use]
pub fn discovery_payload(config: &Config, ambient: bool) -> String {
    let effects = published_effects(ambient);

    json!({
        "name": config.device.name,
        "unique_id": config.device.id,
        "schema": "json",
        "command_topic": config.mqtt.topic(topic::SET),
        "state_topic": config.mqtt.topic(topic::STATE),
        "availability_topic": config.mqtt.topic(topic::AVAILABILITY),
        "payload_available": homeassistant::AVAILABILITY_ONLINE,
        "payload_not_available": homeassistant::AVAILABILITY_OFFLINE,
        "brightness": true,
        "supported_color_modes": ["rgb"],
        "effect": true,
        "effect_list": effects,
        "device": {
            "identifiers": [config.device.id],
            "name": config.device.name,
            "manufacturer": MANUFACTURER,
            "model": MODEL,
        },
    })
    .to_string()
}

/// The effect names in the order Home Assistant shows them.
///
/// The list is long — fifty-odd vendor names — so what is at the top is what anyone
/// actually finds. Ambient goes first on a deployment that has one: it is what the
/// camera work exists for, and appended at the end it sat below every music mode.
/// Screen sync follows, because it is the other thing on the list driven by
/// something other than a fixed palette, and everything else keeps catalogue order
/// behind them.
///
/// Only the published order changes. An effect is selected by name and the names
/// are untouched, so nothing on the wire moves.
fn published_effects(ambient: bool) -> Vec<&'static str> {
    let all = catalogue::all();
    let mut names = Vec::with_capacity(all.len() + usize::from(ambient));
    if ambient {
        names.push(crate::ambient::MODE_EFFECT);
    }
    names.extend(
        all.iter()
            .filter(|effect| effect.kind == catalogue::Kind::TwoWay)
            .map(|effect| effect.name),
    );
    names.extend(
        all.iter()
            .filter(|effect| effect.kind != catalogue::Kind::TwoWay)
            .map(|effect| effect.name),
    );
    names
}

/// Build the retained state message body.
///
/// This is what the daemon last *commanded*, not what the strip reports. The
/// device does not echo effect parameters back, so there is nothing better to
/// publish; the alternative is publishing nothing and leaving Home Assistant
/// guessing.
#[must_use]
pub fn state_payload(snapshot: &Snapshot) -> String {
    let mut value = json!({
        "state": if snapshot.on {
            homeassistant::PAYLOAD_ON
        } else {
            homeassistant::PAYLOAD_OFF
        },
    });

    // Brightness and colour are only reported once something has established
    // them. Sending `0` and black instead would be asserting a measurement the
    // daemon has not taken, and Home Assistant would render it as a light that is
    // on at zero percent.
    if let Some(map) = value.as_object_mut() {
        if let Some(brightness) = snapshot.brightness {
            map.insert("brightness".to_owned(), json!(brightness));
        }

        if let Some((red, green, blue)) = snapshot.colour {
            map.insert("color_mode".to_owned(), json!("rgb"));
            map.insert(
                "color".to_owned(),
                json!({ "r": red, "g": green, "b": blue }),
            );
        }

        // Always present, even when there is nothing to report. Home Assistant's
        // JSON light only applies attributes that appear in the payload, so an
        // omitted `effect` means "no change" rather than "no effect" — which is
        // why picking a colour left the old effect name sitting in its UI. Null
        // is how you say nothing is running.
        map.insert(
            "effect".to_owned(),
            match &snapshot.effect {
                Some(name) => Value::String(name.clone()),
                None => Value::Null,
            },
        );
    }

    value.to_string()
}

/// Turn an alert message into a spec: flash the named colour, then stop.
///
/// The payload is a duration in seconds; anything unparseable falls back to five
/// rather than dropping the alert, because a doorbell that does nothing is worse
/// than one that flashes for the wrong length of time. The identifier is a colour
/// name when it matches one — `alert/blue` is blue — and anything else keeps the
/// old behaviour of flashing red under its own name, so an existing automation
/// using `alert/doorbell` does not break.
#[must_use]
pub fn alert_spec(id: &str, payload: &[u8]) -> AlertSpec {
    let seconds = std::str::from_utf8(payload)
        .ok()
        .and_then(|text| text.trim().parse::<u64>().ok())
        .unwrap_or(5)
        .clamp(1, 300);

    let colour = palette::by_name(id).unwrap_or(palette::FALLBACK);

    // One flash: the colour, then dark. The worker repeats this every
    // `FLASH_PERIOD_MS` for as long as the alert lasts, so building three
    // flashes here would give six a second and read as a strobe rather than an
    // alert.
    let on = wire::colour(colour.hue, colour.saturation, 1000)
        .expect("every palette colour is within the protocol's limits");
    let off = wire::brightness(0).expect("valid brightness");

    AlertSpec {
        id: id.to_owned(),
        priority: 100,
        duration_ms: seconds * 1_000,
        force_on: true,
        frames: vec![on, off],
    }
}

/// Where a colour's alert button sends its press.
#[must_use]
pub fn alert_button_topic(config: &Config, colour: palette::NamedColour) -> String {
    discovery::button_topic(
        &config.mqtt.disc_prefix,
        &format!("{}_alert_{}", config.device.id, colour.name),
    )
}

/// Build the retained discovery document for one colour's alert button.
///
/// A button carries a single payload, so the length is fixed here rather than
/// asked for: pressing "Red alert" in Home Assistant is one tap, and the tap
/// says `10` because that is what `ALERT_BUTTON_LENGTH` says.
#[must_use]
pub fn alert_button_payload(config: &Config, colour: palette::NamedColour) -> String {
    // "red" becomes "Red alert" — the entity list is scanned by eye, and a row
    // of lowercase names reads as machine output.
    let mut title = colour.name.chars();
    let title = match title.next() {
        Some(first) => format!("{}{} alert", first.to_uppercase(), title.as_str()),
        None => "Alert".to_owned(),
    };

    json!({
        "name": title,
        "unique_id": format!("{}_alert_{}", config.device.id, colour.name),
        "command_topic": format!(
            "{}{}",
            config.mqtt.topic(topic::ALERT_PREFIX),
            colour.name
        ),
        "payload_press": config.alerts.length_seconds.to_string(),
        "icon": "mdi:alarm-light",
        "availability_topic": config.mqtt.topic(topic::AVAILABILITY),
        "payload_available": homeassistant::AVAILABILITY_ONLINE,
        "payload_not_available": homeassistant::AVAILABILITY_OFFLINE,
        "device": {
            "identifiers": [config.device.id],
            "name": config.device.name,
            "manufacturer": MANUFACTURER,
            "model": MODEL,
        },
    })
    .to_string()
}

/// Topics this daemon subscribes to.
#[must_use]
pub fn subscriptions(config: &Config) -> Vec<String> {
    vec![
        config.mqtt.topic(topic::SET),
        config.mqtt.topic(topic::ALERT_WILDCARD),
    ]
}

fn options(config: &Config) -> MqttOptions {
    let mut options = MqttOptions::new(
        format!("neewertv-{}", config.device.id),
        &config.mqtt.host,
        config.mqtt.port,
    );
    options.set_keep_alive(std::time::Duration::from_secs(30));

    if let Some(username) = &config.mqtt.username {
        options.set_credentials(username, config.mqtt.password.clone().unwrap_or_default());
    }

    // The broker publishes `offline` for us if the process dies, so Home
    // Assistant does not show a light that is no longer reachable.
    options.set_last_will(LastWill::new(
        config.mqtt.topic(topic::AVAILABILITY),
        homeassistant::AVAILABILITY_OFFLINE,
        QoS::AtLeastOnce,
        true,
    ));

    options
}

/// How long to wait before polling again after a connection-level failure.
const RECONNECT_DELAY: std::time::Duration = std::time::Duration::from_secs(5);

/// Set up one connection: subscribe, then republish everything it should have.
///
/// Called for **every** `ConnAck`, not just the first. rumqttc does not replay
/// subscriptions across a reconnect and the session is clean, so the broker
/// forgets them — and it fires our last will, leaving Home Assistant showing the
/// light offline. A daemon that only announced itself at startup would survive a
/// broker restart by going permanently deaf while every liveness check reported
/// healthy.
async fn announce(
    client: &AsyncClient,
    config: &Config,
    available: bool,
    ambient: bool,
) -> anyhow::Result<()> {
    for topic in subscriptions(config) {
        client
            .subscribe(&topic, QoS::AtLeastOnce)
            .await
            .with_context_msg("subscribing")?;
    }

    // Retained, so Home Assistant finds the light even if it starts later.
    client
        .publish(
            discovery::topic(&config.mqtt.disc_prefix, &config.device.id),
            QoS::AtLeastOnce,
            true,
            discovery_payload(config, ambient),
        )
        .await
        .with_context_msg("publishing discovery")?;

    publish_alert_buttons(client, config)
        .await
        .with_context_msg("publishing alert buttons")?;

    publish_availability(client, config, available)
        .await
        .with_context_msg("publishing availability")?;

    Ok(())
}

/// Publish a discovery document per configured alert colour, and withdraw the
/// rest.
///
/// The withdrawal is not tidiness. A retained discovery document outlives the
/// setting that produced it, so switching `pink` off would otherwise leave a
/// button in Home Assistant for ever — one that publishes an alert nobody
/// configured and, worse, looks like it works. An empty retained payload deletes
/// the retained message, which is the only way to take a discovered entity away.
async fn publish_alert_buttons(
    client: &AsyncClient,
    config: &Config,
) -> Result<(), rumqttc::ClientError> {
    for colour in palette::ALL {
        let payload = if config.alerts.buttons.contains(&colour) {
            alert_button_payload(config, colour)
        } else {
            String::new()
        };

        client
            .publish(
                alert_button_topic(config, colour),
                QoS::AtLeastOnce,
                true,
                payload,
            )
            .await?;
    }

    Ok(())
}

/// Say whether the strip is reachable.
///
/// Deliberately tied to the link rather than to the process. The two are not the
/// same question: a container that is running but cannot reach the strip is
/// exactly the case Home Assistant has to know about, and calling that `online`
/// is how a light sits there looking controllable while every command vanishes.
/// A strip that is merely switched off at the wall is reported the same way — it
/// genuinely is not reachable, and no amount of restarting this process changes
/// that.
async fn publish_availability(
    client: &AsyncClient,
    config: &Config,
    available: bool,
) -> Result<(), rumqttc::ClientError> {
    let payload = if available {
        homeassistant::AVAILABILITY_ONLINE
    } else {
        homeassistant::AVAILABILITY_OFFLINE
    };

    client
        .publish(
            config.mqtt.topic(topic::AVAILABILITY),
            QoS::AtLeastOnce,
            true,
            payload,
        )
        .await
}

async fn publish_state(client: &AsyncClient, topic: &str, snapshot: &Snapshot) -> String {
    let payload = state_payload(snapshot);
    if let Err(err) = client
        .publish(topic, QoS::AtLeastOnce, true, payload.clone())
        .await
    {
        tracing::warn!(%err, "publishing state");
    }
    payload
}

/// Connect, announce, then pump events forever.
pub async fn run(
    config: Config,
    mut state: tokio::sync::watch::Receiver<Snapshot>,
    commands: tokio::sync::mpsc::Sender<crate::ble::worker::Message>,
    ambient: Option<crate::ambient::Handle>,
    shutdown: CancellationToken,
) -> anyhow::Result<()> {
    let (client, mut eventloop) = AsyncClient::new(options(&config), 32);

    let set_topic = config.mqtt.topic(topic::SET);
    let alert_prefix = config.mqtt.topic(topic::ALERT_PREFIX);
    let state_topic = config.mqtt.topic(topic::STATE);
    let mut last_state = String::new();
    let mut last_available: Option<bool> = None;
    // `None` until the first connection has announced, so the loop below only
    // republishes on a *change* rather than racing the ConnAck handler.
    let mut announced_ambient: Option<bool> = None;

    loop {
        // The discovery payload is retained and only rewritten on connect, so a
        // calibration saved from the page after start-up would otherwise leave
        // Home Assistant offering an effect it never mentions — or, on a fresh
        // install, offering one that does nothing. Re-announcing on a change is
        // what makes the page the thing that turns the mode on, rather than a
        // restart being the thing that does.
        //
        // The keepalive alone brings the loop round every thirty seconds, which is
        // often enough for something a person has to walk over and click.
        let ambient_now = ambient
            .as_ref()
            .is_some_and(|handle| handle.calibration().is_some());
        if announced_ambient.is_some_and(|announced| announced != ambient_now) {
            let connected = state.borrow().connected;
            match announce(&client, &config, connected, ambient_now).await {
                Ok(()) => announced_ambient = Some(ambient_now),
                Err(err) => tracing::warn!(%err, "re-announcing after the calibration changed"),
            }
        }

        tokio::select! {
            event = eventloop.poll() => {
                match event {
                    Ok(Event::Incoming(Packet::ConnAck(_))) => {
                        tracing::info!("MQTT connected");
                        let snapshot = state.borrow().clone();

                        if let Err(err) = announce(&client, &config, snapshot.connected, ambient_now).await {
                            tracing::warn!(%err, "announcing on connect");
                        }
                        announced_ambient = Some(ambient_now);
                        last_available = Some(snapshot.connected);

                        // Whatever Home Assistant last saw is now stale: the
                        // session is new, so republish the current state and
                        // remember it as published.
                        last_state = publish_state(&client, &state_topic, &snapshot).await;
                    }
                    Ok(Event::Incoming(Packet::Publish(publish))) => {
                        handle_publish(&publish, &set_topic, &alert_prefix, &commands, ambient.as_ref()).await;
                    }
                    Ok(_) => {}
                    Err(err) => {
                        tracing::warn!(%err, "MQTT event loop error; retrying");
                        last_state.clear();
                        tokio::time::sleep(RECONNECT_DELAY).await;
                    }
                }
            }

            changed = state.changed() => {
                if changed.is_err() {
                    break;
                }
                let snapshot = state.borrow().clone();

                if last_available != Some(snapshot.connected) {
                    if let Err(err) =
                        publish_availability(&client, &config, snapshot.connected).await
                    {
                        tracing::warn!(%err, "publishing availability");
                    }
                    last_available = Some(snapshot.connected);
                }

                if state_payload(&snapshot) != last_state {
                    last_state = publish_state(&client, &state_topic, &snapshot).await;
                }
            }

            () = shutdown.cancelled() => {
                // Say goodbye on purpose. A clean disconnect does not trigger the
                // last will, so without this the broker would keep advertising a
                // light whose process has gone until the keepalive catches up.
                if let Err(err) = publish_availability(&client, &config, false).await {
                    tracing::warn!(%err, "publishing offline on shutdown");
                }
                let _ = client.disconnect().await;
                break;
            }
        }
    }

    Ok(())
}

async fn handle_publish(
    publish: &rumqttc::Publish,
    set_topic: &str,
    alert_prefix: &str,
    commands: &tokio::sync::mpsc::Sender<crate::ble::worker::Message>,
    ambient: Option<&crate::ambient::Handle>,
) {
    let topic = publish.topic.as_str();

    if topic == set_topic {
        match serde_json::from_slice::<SetPayload>(&publish.payload) {
            Ok(payload) => {
                // Ambient is a mode of this daemon rather than a frame the strip
                // understands, so it is intercepted before the wire encoder can
                // reject it as an unknown effect.
                //
                // Only honoured when there is a calibration to follow. Without
                // one the mode would be a flag that drives nothing, and — worse —
                // it would swallow the rest of the payload, so a retained
                // discovery message from before the calibration was deleted could
                // leave a light that answers nothing at all. Falling through sends
                // the command to the encoder, which rejects the name and says so.
                if payload.effect.as_deref() == Some(crate::ambient::MODE_EFFECT) {
                    let calibrated = ambient.is_some_and(|ambient| ambient.calibration().is_some());
                    if calibrated {
                        tracing::info!("ambient mode requested");
                        if let Some(ambient) = ambient {
                            ambient.set_mode(true);
                        }
                        return;
                    }
                    tracing::warn!("ambient mode requested but this deployment has no calibration");
                }

                match command::resolve(&payload) {
                    // Anything the strip is actually told to do ends ambient mode.
                    // Otherwise the camera would overwrite a colour a frame later,
                    // which is the defect the mode exists to avoid.
                    Ok(resolved) if resolved.is_empty() => {}
                    Ok(resolved) => {
                        if let Some(ambient) = ambient {
                            ambient.set_mode(false);
                        }

                        let batch = crate::app::queue::Batch { commands: resolved };
                        if commands
                            .send(crate::ble::worker::Message::Batch(batch))
                            .await
                            .is_err()
                        {
                            tracing::warn!("worker has gone away");
                        }
                    }
                    Err(err) => tracing::warn!(%err, "rejecting command"),
                }
            }
            Err(err) => {
                tracing::warn!(%err, payload = %String::from_utf8_lossy(&publish.payload), "malformed set payload")
            }
        }
        return;
    }

    if let Some(id) = topic.strip_prefix(alert_prefix) {
        let spec = alert_spec(id, &publish.payload);
        tracing::info!(alert = %spec.id, "alert requested");
        let _ = commands
            .send(crate::ble::worker::Message::Alert(spec))
            .await;
    }
}

/// Small helper so the `anyhow::Context` import stays local to this file.
trait ContextMsg<T> {
    fn with_context_msg(self, message: &'static str) -> anyhow::Result<T>;
}

impl<T, E: std::fmt::Display> ContextMsg<T> for Result<T, E> {
    fn with_context_msg(self, message: &'static str) -> anyhow::Result<T> {
        self.map_err(|err| anyhow::anyhow!("{message}: {err}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use std::collections::HashMap;

    fn config() -> Config {
        let pairs = HashMap::from([
            ("MQTT_HOST".to_owned(), "broker".to_owned()),
            ("MQTT_BASE".to_owned(), "neewertv".to_owned()),
        ]);
        Config::from_lookup(|name| pairs.get(name).cloned()).unwrap()
    }

    #[test]
    fn discovery_names_the_topics_home_assistant_will_use() {
        let payload: Value = serde_json::from_str(&discovery_payload(&config(), false)).unwrap();
        assert_eq!(payload["command_topic"], "neewertv/set");
        assert_eq!(payload["state_topic"], "neewertv/state");
        assert_eq!(payload["availability_topic"], "neewertv/availability");
        assert_eq!(payload["schema"], "json");
        assert_eq!(payload["brightness"], true);
        assert_eq!(payload["supported_color_modes"][0], "rgb");
    }

    #[test]
    fn discovery_offers_every_effect_in_the_catalogue() {
        let payload: Value = serde_json::from_str(&discovery_payload(&config(), false)).unwrap();
        let effects = payload["effect_list"].as_array().unwrap();
        assert_eq!(effects.len(), catalogue::all().len());
        assert!(effects.iter().any(|e| e == "Superfluid"));
    }

    #[test]
    fn ambient_is_offered_only_where_it_would_work() {
        // An effect in the list that does nothing is worse than an absent one, so
        // the mode only appears when there is a calibration behind it.
        let without: Value = serde_json::from_str(&discovery_payload(&config(), false)).unwrap();
        assert!(
            !without["effect_list"]
                .as_array()
                .unwrap()
                .iter()
                .any(|e| e == crate::ambient::MODE_EFFECT)
        );

        let with: Value = serde_json::from_str(&discovery_payload(&config(), true)).unwrap();
        let effects = with["effect_list"].as_array().unwrap();
        assert_eq!(effects.len(), catalogue::all().len() + 1);
        assert!(effects.iter().any(|e| e == crate::ambient::MODE_EFFECT));
    }

    #[test]
    fn ambient_is_first_and_screen_sync_follows_it() {
        // The list is fifty names long, so whatever is at the top is what anyone
        // finds. Ambient used to be appended last, below every music mode.
        let names = published_effects(true);
        assert_eq!(names[0], crate::ambient::MODE_EFFECT);

        let sync: Vec<&str> = catalogue::all()
            .iter()
            .filter(|effect| effect.kind == catalogue::Kind::TwoWay)
            .map(|effect| effect.name)
            .collect();
        assert_eq!(&names[1..=sync.len()], sync.as_slice());

        // Everything else is still there, once each, and nothing was invented.
        assert_eq!(names.len(), catalogue::all().len() + 1);
        let mut seen = names.clone();
        seen.sort_unstable();
        let deduped = {
            let mut copy = seen.clone();
            copy.dedup();
            copy
        };
        assert_eq!(seen, deduped, "a name appears twice");

        // Without ambient the sync family still leads.
        let plain = published_effects(false);
        assert_eq!(&plain[..sync.len()], sync.as_slice());
    }

    #[test]
    fn discovery_uses_the_configured_discovery_prefix() {
        let pairs = HashMap::from([
            ("MQTT_HOST".to_owned(), "broker".to_owned()),
            ("DISC_PREFIX".to_owned(), "ha".to_owned()),
        ]);
        let config = Config::from_lookup(|name| pairs.get(name).cloned()).unwrap();
        assert_eq!(config.mqtt.disc_prefix, "ha");
    }

    #[test]
    fn state_payload_reports_power_brightness_and_colour() {
        let snapshot = Snapshot {
            on: true,
            brightness: Some(128),
            colour: Some((255, 0, 0)),
            effect: Some("Superfluid".to_owned()),
            ..Snapshot::default()
        };
        let payload: Value = serde_json::from_str(&state_payload(&snapshot)).unwrap();
        assert_eq!(payload["state"], homeassistant::PAYLOAD_ON);
        assert_eq!(payload["brightness"], 128);
        assert_eq!(payload["color"]["r"], 255);
        assert_eq!(payload["effect"], "Superfluid");
    }

    #[test]
    fn state_payload_says_explicitly_when_no_effect_is_running() {
        // Not the same as leaving the key out: an absent attribute tells Home
        // Assistant to keep whatever it had, which is how a colour change used to
        // leave the effect list showing the previous effect.
        let payload: Value = serde_json::from_str(&state_payload(&Snapshot::default())).unwrap();
        assert_eq!(payload["effect"], Value::Null);
        assert_eq!(payload["state"], homeassistant::PAYLOAD_OFF);
    }

    #[test]
    fn an_alert_payload_is_a_duration_in_seconds() {
        let spec = alert_spec("doorbell", b"10");
        assert_eq!(spec.duration_ms, 10_000);
        assert_eq!(spec.id, "doorbell");
        assert!(spec.force_on);
        // One flash: the worker repeats it for the whole duration.
        assert_eq!(spec.frames.len(), 2, "a colour and a blank");
    }

    #[test]
    fn a_nonsense_alert_duration_still_fires() {
        // A doorbell that does nothing because the payload was empty is worse
        // than one that flashes for the default length.
        assert_eq!(alert_spec("bell", b"").duration_ms, 5_000);
        assert_eq!(alert_spec("bell", b"forever").duration_ms, 5_000);
        assert_eq!(alert_spec("bell", b"99999").duration_ms, 300_000);
    }

    #[test]
    fn an_alert_named_after_a_colour_flashes_that_colour() {
        for colour in palette::ALL {
            let spec = alert_spec(colour.name, b"5");
            let expected = wire::colour(colour.hue, colour.saturation, 1000)
                .expect("palette colours are valid");

            assert_eq!(
                spec.frames[0], expected,
                "{:?} should flash its own colour",
                colour.name
            );
            assert_eq!(spec.frames[1], wire::brightness(0).unwrap(), "then dark");
        }
    }

    #[test]
    fn an_alert_named_after_something_else_still_flashes_red() {
        // The pre-colour contract, kept: an automation already publishing to
        // `alert/doorbell` must not silently stop doing anything.
        let spec = alert_spec("doorbell", b"5");
        assert_eq!(
            spec.frames[0],
            wire::colour(palette::FALLBACK.hue, palette::FALLBACK.saturation, 1000).unwrap()
        );
        assert_eq!(spec.id, "doorbell", "and it keeps its own identity");
    }

    #[test]
    fn alert_buttons_press_their_own_colour_for_the_configured_length() {
        let config = crate::config::Config::from_lookup(|name| {
            (name == "MQTT_HOST").then(|| "10.0.0.5".to_owned())
        })
        .expect("minimal config");

        let red = palette::by_name("red").unwrap();
        let payload: serde_json::Value =
            serde_json::from_str(&alert_button_payload(&config, red)).expect("valid json");

        assert_eq!(payload["name"], "Red alert");
        assert_eq!(payload["command_topic"], "neewertv/alert/red");
        assert_eq!(
            payload["payload_press"], "10",
            "the configured length, as text — the alert topic parses it as seconds"
        );
        assert_eq!(payload["unique_id"], "neewer_ns03_alert_red");
        assert_eq!(
            payload["device"]["identifiers"][0], "neewer_ns03",
            "the buttons belong to the light's device, not to a device of their own"
        );
        assert_eq!(
            alert_button_topic(&config, red),
            "homeassistant/button/neewer_ns03_alert_red/config"
        );
    }

    #[test]
    fn two_strips_publish_buttons_that_do_not_collide() {
        // Same palette, different strip: the object id carries the device id, so
        // Home Assistant sees two distinct buttons rather than one that flickers.
        let strip = |id: &str| {
            crate::config::Config::from_lookup(move |name| match name {
                "MQTT_HOST" => Some("10.0.0.5".to_owned()),
                "DEVICE_ID" => Some(id.to_owned()),
                _ => None,
            })
            .expect("minimal config")
        };

        let first = strip("neewer_lounge");
        let second = strip("neewer_bedroom");
        let red = palette::by_name("red").unwrap();

        assert_ne!(
            alert_button_topic(&first, red),
            alert_button_topic(&second, red)
        );
    }

    #[test]
    fn the_alerts_frames_are_real_protocol_frames() {
        let spec = alert_spec("bell", b"3");
        for frame in &spec.frames {
            assert_eq!(frame[0], 0x7A, "header");
            assert_eq!(crate::protocol::frame::verify(frame), Ok(()));
        }
    }

    // A mode and a command both want the strip, and which of them wins is decided
    // by the order two statements run in. That is not something a screenshot can
    // show, so it is pinned here.

    fn ambient_handle(with_calibration: bool) -> crate::ambient::Handle {
        use crate::ambient::test_support;
        crate::ambient::Handle::for_test(
            with_calibration.then(test_support::calibration),
            test_support::scratch_path(),
        )
    }

    fn a_publish(body: &str) -> rumqttc::Publish {
        rumqttc::Publish::new("neewertv/set", QoS::AtLeastOnce, body)
    }

    #[tokio::test]
    async fn choosing_the_ambient_effect_turns_the_mode_on_and_sends_no_frame() {
        let ambient = ambient_handle(true);
        let (commands, mut sent) = tokio::sync::mpsc::channel(4);

        handle_publish(
            &a_publish(r#"{"effect":"TV Ambient"}"#),
            "neewertv/set",
            "neewertv/alert",
            &commands,
            Some(&ambient),
        )
        .await;

        assert!(ambient.mode(), "the mode is the whole point of choosing it");
        // The wire encoder would refuse the name, so nothing may reach it: this
        // asserts the interception happened before the command was queued.
        assert!(
            sent.try_recv().is_err(),
            "an effect the daemon owns must not be sent to the strip"
        );
    }

    #[tokio::test]
    async fn any_command_that_reaches_the_strip_ends_ambient_mode() {
        let ambient = ambient_handle(true);
        let (commands, mut sent) = tokio::sync::mpsc::channel(4);
        ambient.set_mode(true);

        handle_publish(
            &a_publish(r#"{"color":{"r":255,"g":0,"b":0}}"#),
            "neewertv/set",
            "neewertv/alert",
            &commands,
            Some(&ambient),
        )
        .await;

        // Both halves matter. Left on, a camera frame would repaint the strip
        // sixty-six milliseconds after the colour landed.
        assert!(!ambient.mode(), "the colour must outrank the camera");
        assert!(sent.try_recv().is_ok(), "the colour still has to be sent");
    }

    #[tokio::test]
    async fn the_ambient_effect_is_ignored_when_nothing_could_follow_it() {
        // A retained discovery message can outlive the calibration it was written
        // for. Honouring the effect then would set a flag that drives nothing and
        // swallow the rest of the payload, so the request falls through instead.
        let ambient = ambient_handle(false);
        let (commands, mut sent) = tokio::sync::mpsc::channel(4);

        handle_publish(
            &a_publish(r#"{"effect":"TV Ambient"}"#),
            "neewertv/set",
            "neewertv/alert",
            &commands,
            Some(&ambient),
        )
        .await;

        assert!(!ambient.mode());
        assert!(sent.try_recv().is_err());
    }
}
