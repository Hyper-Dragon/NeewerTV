//! Configuration, read once at startup.
//!
//! Reading is injected as a lookup function rather than going straight to the
//! environment, which is the only way to test it without tests fighting each
//! other over process-global state. A bad value names the variable and the value
//! it got: the container exits, the orchestrator restarts it, and the log says
//! exactly what to fix.

use std::fmt;
use std::net::SocketAddr;
use std::str::FromStr;

use anyhow::{Context as _, bail};

/// What the optional web interface is allowed to do.
///
/// There is deliberately no control mode. Driving the strip over HTTP meant
/// maintaining a second control path with its own precedence rules, and it could
/// not do anything Home Assistant cannot — so the page shows state and nothing
/// else, and the only thing that changes the light is MQTT.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WebMode {
    /// No listener at all.
    Off,
    /// Health, state and a status page. Read-only.
    Observe,
}

impl WebMode {
    pub const fn is_enabled(self) -> bool {
        !matches!(self, Self::Off)
    }
}

impl fmt::Display for WebMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Off => "off",
            Self::Observe => "observe",
        })
    }
}

impl FromStr for WebMode {
    type Err = anyhow::Error;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "off" | "false" | "0" | "no" => Ok(Self::Off),
            "observe" | "readonly" | "read-only" => Ok(Self::Observe),
            other => bail!("expected one of off, observe — got {other:?}"),
        }
    }
}

/// How log lines are formatted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogFormat {
    Text,
    Json,
}

impl FromStr for LogFormat {
    type Err = anyhow::Error;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "text" | "pretty" | "human" => Ok(Self::Text),
            "json" => Ok(Self::Json),
            other => bail!("expected one of text, json — got {other:?}"),
        }
    }
}

/// Where the broker is and how to talk to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MqttConfig {
    pub host: String,
    pub port: u16,
    pub username: Option<String>,
    pub password: Option<String>,
    /// Prefix for state and command topics.
    pub base: String,
    /// Home Assistant discovery prefix.
    pub disc_prefix: String,
}

impl MqttConfig {
    #[must_use]
    pub fn broker(&self) -> String {
        format!("mqtt://{}:{}", self.host, self.port)
    }

    /// `{base}/state`, `{base}/set`, and so on.
    #[must_use]
    pub fn topic(&self, suffix: &str) -> String {
        if suffix.is_empty() {
            self.base.clone()
        } else {
            format!("{}/{suffix}", self.base)
        }
    }
}

/// Which strip this container owns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BleConfig {
    /// Glob pattern for the advertised name — the name comes from the model, so
    /// matching it exactly would only find one model.
    pub name_match: String,
    /// Set only when more than one strip is in range.
    pub address: Option<String>,
    pub scan_seconds: u64,
    /// How often to poke an idle link, zero to leave it alone.
    ///
    /// Defaults to 20 seconds because these controllers have been observed closing
    /// a connection they have heard nothing on. A strip that holds an idle link
    /// simply gets one read-only query it did not need; set 0 to send nothing.
    pub keepalive_seconds: u64,
}

/// How the device presents itself to Home Assistant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceConfig {
    pub id: String,
    pub name: String,
}

/// Every environment variable the daemon reads.
///
/// Named because they appear both in the lookup and in the error messages that
/// tell an operator which variable to fix. A typo in one of those pairs is a
/// silent misconfiguration that the compiler cannot catch.
pub mod env {
    pub const MQTT_HOST: &str = "MQTT_HOST";
    pub const MQTT_PORT: &str = "MQTT_PORT";
    pub const MQTT_USERNAME: &str = "MQTT_USERNAME";
    pub const MQTT_PASSWORD: &str = "MQTT_PASSWORD";
    pub const MQTT_BASE: &str = "MQTT_BASE";
    pub const DISC_PREFIX: &str = "DISC_PREFIX";
    pub const BLE_NAME_MATCH: &str = "BLE_NAME_MATCH";
    pub const BLE_DEVICE_ADDRESS: &str = "BLE_DEVICE_ADDRESS";
    pub const BLE_SCAN_SECONDS: &str = "BLE_SCAN_SECONDS";
    pub const BLE_KEEPALIVE_SECONDS: &str = "BLE_KEEPALIVE_SECONDS";
    pub const ALERT_BUTTONS: &str = "ALERT_BUTTONS";
    pub const ALERT_BUTTON_LENGTH: &str = "ALERT_BUTTON_LENGTH";
    pub const DEVICE_ID: &str = "DEVICE_ID";
    pub const DEVICE_NAME: &str = "DEVICE_NAME";
    pub const WEB_INTERFACE: &str = "WEB_INTERFACE";
    pub const WEB_BIND: &str = "WEB_BIND";
    pub const LOG_LEVEL: &str = "LOG_LEVEL";
    pub const LOG_FORMAT: &str = "LOG_FORMAT";
}

/// Defaults, which are documented promises as much as code.
///
/// The README quotes every one of these in its table, so they should be
/// changeable in exactly one place and greppable by name.
pub mod default {
    pub const MQTT_PORT: u16 = 1883;
    /// Deliberately the project name rather than the model: one daemon, one
    /// narrow job, and a prefix nobody has to guess.
    pub const MQTT_BASE: &str = "neewertv";
    /// Home Assistant's own default discovery prefix.
    pub const DISC_PREFIX: &str = "homeassistant";
    /// Matches the whole family, not one model: `NH-PD20250145` is the 5 m strip
    /// and `NH-PD20250103` the 3 m.
    pub const BLE_NAME_MATCH: &str = "NH-PD*";
    pub const BLE_SCAN_SECONDS: u64 = 12;
    /// On by default, at 20 seconds.
    ///
    /// Measured on a real strip: an idle link is closed by the device. The first
    /// estimate of when was 40–200 seconds, from a run where the daemon was being
    /// driven; once it was left alone the log showed the same number over and
    /// over — `held_seconds=41`, again and again. Forty-five seconds of keep-alive
    /// was therefore always too late, which looked exactly like the keep-alive not
    /// working at all. Twenty seconds is comfortably inside the window and still
    /// costs almost nothing.
    ///
    /// It is not the whole story. Later runs kept the same strip up for ten
    /// minutes, then lost it after 344 s, then after 40 s — with the keep-alive
    /// firing throughout. A link can therefore be closed for reasons that have
    /// nothing to do with idleness, and this setting only defends against the one
    /// of them it can. `docs/NOTES.md` has the reason codes that tell them apart.
    pub const BLE_KEEPALIVE_SECONDS: u64 = 20;
    pub const DEVICE_ID: &str = "neewer_ns03";
    pub const DEVICE_NAME: &str = "TV Backlight";
    /// Three colours, because three is what a house uses: something is wrong,
    /// something needs attention, everything is fine.
    pub const ALERT_BUTTONS: &str = "red,blue,green";
    /// Ten seconds: long enough to see from the sofa, short enough not to be
    /// annoying when a test automation fires it twenty times.
    pub const ALERT_BUTTON_LENGTH: u64 = 10;
    pub const WEB_PORT: u16 = 8080;
    pub const LOG_LEVEL: &str = "info";
    /// The page is useful and changes nothing, so it is on by default.
    pub const WEB_INTERFACE: super::WebMode = super::WebMode::Observe;
}

/// Which alert colours to publish as buttons, and how long they last.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AlertConfig {
    /// The buttons Home Assistant will show, in the order they are declared.
    ///
    /// A closed palette on purpose: see [`crate::app::palette`]. An unknown
    /// colour is a startup error rather than a button that never appears.
    pub buttons: Vec<crate::app::palette::NamedColour>,
    /// Seconds each alert flashes for. One number for all of them, because the
    /// question an automation asks is "how long is this announcement", not
    /// "how long is blue".
    pub length_seconds: u64,
}

/// Everything the daemon needs to start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub web: WebMode,
    pub web_bind: SocketAddr,
    pub log_level: String,
    pub log_format: LogFormat,
    pub mqtt: MqttConfig,
    pub ble: BleConfig,
    pub device: DeviceConfig,
    pub alerts: AlertConfig,
}

impl Config {
    /// Read the process environment.
    pub fn from_env() -> anyhow::Result<Self> {
        Self::from_lookup(|name| std::env::var(name).ok())
    }

    /// Build from any key/value source.
    ///
    /// A genuinely empty value is treated as unset, because `FOO=` in a compose
    /// file is almost always a half-filled variable rather than an instruction.
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> anyhow::Result<Self> {
        let get = |name: &str| {
            lookup(name)
                .map(|value| value.trim().to_owned())
                .filter(|value| !value.is_empty())
        };

        let mqtt = MqttConfig {
            host: get(env::MQTT_HOST).with_context(|| {
                format!(
                    "{} is required — without a broker this daemon has nothing to talk to",
                    env::MQTT_HOST
                )
            })?,
            port: parse_or(&get, env::MQTT_PORT, default::MQTT_PORT)?,
            username: get(env::MQTT_USERNAME),
            password: get(env::MQTT_PASSWORD),
            base: get(env::MQTT_BASE).unwrap_or_else(|| default::MQTT_BASE.to_owned()),
            disc_prefix: get(env::DISC_PREFIX).unwrap_or_else(|| default::DISC_PREFIX.to_owned()),
        };

        let ble = BleConfig {
            name_match: get(env::BLE_NAME_MATCH)
                .unwrap_or_else(|| default::BLE_NAME_MATCH.to_owned()),
            address: get(env::BLE_DEVICE_ADDRESS),
            scan_seconds: parse_or(&get, env::BLE_SCAN_SECONDS, default::BLE_SCAN_SECONDS)?,
            keepalive_seconds: parse_or(
                &get,
                env::BLE_KEEPALIVE_SECONDS,
                default::BLE_KEEPALIVE_SECONDS,
            )?,
        };

        let device = DeviceConfig {
            id: get(env::DEVICE_ID).unwrap_or_else(|| default::DEVICE_ID.to_owned()),
            name: get(env::DEVICE_NAME).unwrap_or_else(|| default::DEVICE_NAME.to_owned()),
        };

        let alerts = AlertConfig {
            buttons: parse_buttons(
                &get(env::ALERT_BUTTONS).unwrap_or_else(|| default::ALERT_BUTTONS.to_owned()),
            )?,
            length_seconds: parse_or(&get, env::ALERT_BUTTON_LENGTH, default::ALERT_BUTTON_LENGTH)?
                .clamp(1, 300),
        };

        Ok(Self {
            web: parse_or(&get, env::WEB_INTERFACE, default::WEB_INTERFACE)?,
            web_bind: match get(env::WEB_BIND) {
                Some(raw) => raw.parse::<SocketAddr>().map_err(|err| {
                    anyhow::anyhow!("{}={raw:?} is invalid: {err}", env::WEB_BIND)
                })?,
                None => SocketAddr::from(([0, 0, 0, 0], default::WEB_PORT)),
            },
            log_level: get(env::LOG_LEVEL).unwrap_or_else(|| default::LOG_LEVEL.to_owned()),
            log_format: parse_or(&get, env::LOG_FORMAT, LogFormat::Text)?,
            mqtt,
            ble,
            device,
            alerts,
        })
    }
}

/// Turn `ALERT_BUTTONS` into colours, rejecting names that do not exist.
///
/// `none` is the way to publish no buttons at all; an empty value is treated as
/// unset, so it cannot mean "off" and must not be pressed into service as such.
/// Duplicates are dropped rather than rejected: two buttons with the same colour
/// would collide on the discovery `unique_id`, and the operator's intent is
/// obvious either way.
fn parse_buttons(raw: &str) -> anyhow::Result<Vec<crate::app::palette::NamedColour>> {
    if raw.trim().eq_ignore_ascii_case("none") {
        return Ok(Vec::new());
    }

    let mut buttons: Vec<crate::app::palette::NamedColour> = Vec::new();
    for name in raw
        .split(',')
        .map(str::trim)
        .filter(|name| !name.is_empty())
    {
        let colour = crate::app::palette::by_name(name).ok_or_else(|| {
            anyhow::anyhow!(
                "{}={name:?} is not a colour this daemon knows — choose from {}, or \"none\"",
                env::ALERT_BUTTONS,
                crate::app::palette::names()
            )
        })?;
        if !buttons.contains(&colour) {
            buttons.push(colour);
        }
    }

    Ok(buttons)
}

fn parse_or<T>(get: &impl Fn(&str) -> Option<String>, name: &str, default: T) -> anyhow::Result<T>
where
    T: FromStr,
    T::Err: fmt::Display,
{
    match get(name) {
        None => Ok(default),
        Some(raw) => raw
            .parse::<T>()
            .map_err(|err| anyhow::anyhow!("{name}={raw:?} is invalid: {err}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn config(pairs: &[(&str, &str)]) -> anyhow::Result<Config> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect();
        Config::from_lookup(|name| map.get(name).cloned())
    }

    const MINIMAL: &[(&str, &str)] = &[("MQTT_HOST", "10.0.0.5")];

    /// Never a real address: see the note in
    /// `the_device_selector_is_optional_and_starts_unset`.
    const PLACEHOLDER_ADDRESS: &str = "AA:BB:CC:DD:EE:FF";

    #[test]
    fn a_broker_is_the_only_required_setting() {
        let parsed = config(MINIMAL).expect("MQTT_HOST alone should be enough");
        assert_eq!(parsed.mqtt.host, "10.0.0.5");
        assert_eq!(parsed.mqtt.port, 1883);
        assert_eq!(parsed.mqtt.base, "neewertv");
        assert_eq!(parsed.mqtt.disc_prefix, "homeassistant");
        assert_eq!(parsed.ble.name_match, default::BLE_NAME_MATCH);
        assert_eq!(parsed.ble.scan_seconds, 12);
        assert_eq!(
            parsed.ble.keepalive_seconds, 20,
            "on by default, and below the strip's own idle timeout"
        );
        assert_eq!(parsed.web, WebMode::Observe);
        assert_eq!(parsed.log_format, LogFormat::Text);
    }

    #[test]
    fn a_missing_broker_fails_loudly_and_explains_itself() {
        let error = config(&[]).expect_err("no broker must not start");
        let message = error.to_string();
        assert!(message.contains("MQTT_HOST"), "got: {message}");
    }

    fn names(buttons: &[crate::app::palette::NamedColour]) -> Vec<&'static str> {
        buttons.iter().map(|colour| colour.name).collect()
    }

    #[test]
    fn alerts_default_to_three_buttons_and_ten_seconds() {
        // Three, because a house needs "wrong", "attention" and "fine" — and ten
        // seconds because that is long enough to notice from the sofa.
        let parsed = config(MINIMAL).expect("minimal config");
        assert_eq!(names(&parsed.alerts.buttons), ["red", "blue", "green"]);
        assert_eq!(parsed.alerts.length_seconds, 10);
    }

    #[test]
    fn alert_buttons_are_trimmed_deduplicated_and_ordered_as_written() {
        // Order is the order of the buttons in Home Assistant, so it is the
        // operator's choice rather than the palette's. A duplicate would collide
        // on the discovery unique_id and produce one button instead of two.
        let parsed = config(&[
            ("MQTT_HOST", "10.0.0.5"),
            ("ALERT_BUTTONS", " pink, GREEN ,pink,blue "),
        ])
        .expect("all of those are colours");

        assert_eq!(names(&parsed.alerts.buttons), ["pink", "green", "blue"]);
    }

    #[test]
    fn none_switches_the_buttons_off() {
        // An empty value cannot mean this: `ALERT_BUTTONS=` is a half-filled
        // compose variable, and it is treated as unset everywhere else.
        let parsed = config(&[("MQTT_HOST", "10.0.0.5"), ("ALERT_BUTTONS", "none")])
            .expect("none is a valid answer");
        assert!(parsed.alerts.buttons.is_empty());
    }

    #[test]
    fn an_unknown_colour_stops_the_daemon_and_lists_the_real_ones() {
        let error = config(&[("MQTT_HOST", "10.0.0.5"), ("ALERT_BUTTONS", "red,burgundy")])
            .expect_err("a typo must not become a button that never appears");
        let message = error.to_string();

        assert!(message.contains("burgundy"), "got: {message}");
        assert!(message.contains("ALERT_BUTTONS"), "got: {message}");
        assert!(
            message.contains("purple") && message.contains("pink"),
            "the message has to name the alternatives: {message}"
        );
    }

    #[test]
    fn the_alert_length_is_clamped_to_what_the_alert_topic_accepts() {
        // The topic clamps payloads to 1–300 s; a configured length outside that
        // would make the button appear to do nothing.
        for (raw, expected) in [("0", 1), ("9999", 300), ("25", 25)] {
            let parsed = config(&[("MQTT_HOST", "10.0.0.5"), ("ALERT_BUTTON_LENGTH", raw)])
                .expect("a number");
            assert_eq!(parsed.alerts.length_seconds, expected, "for {raw:?}");
        }
    }

    #[test]
    fn an_empty_value_is_treated_as_unset() {
        // `MQTT_HOST=` in a compose file is a half-filled variable, not an
        // instruction to use the empty hostname.
        assert!(config(&[("MQTT_HOST", "  ")]).is_err());
        // But a defaultable setting falls back rather than failing.
        assert_eq!(
            config(&[("MQTT_HOST", "x"), ("MQTT_PORT", "")])
                .unwrap()
                .mqtt
                .port,
            1883
        );
    }

    #[test]
    fn credentials_are_optional() {
        let parsed = config(MINIMAL).unwrap();
        assert_eq!(parsed.mqtt.username, None);
        assert_eq!(parsed.mqtt.password, None);

        let parsed = config(&[
            ("MQTT_HOST", "broker"),
            ("MQTT_USERNAME", "ns03"),
            ("MQTT_PASSWORD", "secret"),
        ])
        .unwrap();
        assert_eq!(parsed.mqtt.username.as_deref(), Some("ns03"));
    }

    #[test]
    fn the_broker_url_is_built_from_host_and_port() {
        let parsed = config(&[("MQTT_HOST", "broker.local"), ("MQTT_PORT", "1884")]).unwrap();
        assert_eq!(parsed.mqtt.broker(), "mqtt://broker.local:1884");
    }

    #[test]
    fn topics_hang_off_the_base() {
        let parsed = config(&[("MQTT_HOST", "h"), ("MQTT_BASE", "house/lounge")]).unwrap();
        assert_eq!(
            parsed.mqtt.topic(crate::homeassistant::topic::STATE),
            "house/lounge/state"
        );
        assert_eq!(
            parsed.mqtt.topic(crate::homeassistant::topic::ALERT_PREFIX),
            "house/lounge/alert/"
        );
        // An empty suffix is the base itself, which is how callers refer to the
        // root of their own subtree.
        assert_eq!(parsed.mqtt.topic(""), "house/lounge");
    }

    #[test]
    fn the_device_selector_is_optional_and_starts_unset() {
        assert_eq!(config(MINIMAL).unwrap().ble.address, None);
        // A deliberately synthetic address: this repository must not contain a
        // real device's address, not even one belonging to whoever wrote the
        // test. AA:BB:CC:DD:EE:FF is the conventional placeholder.
        let parsed = config(&[
            ("MQTT_HOST", "h"),
            ("BLE_DEVICE_ADDRESS", PLACEHOLDER_ADDRESS),
        ])
        .unwrap();
        assert_eq!(parsed.ble.address.as_deref(), Some(PLACEHOLDER_ADDRESS));
    }

    #[test]
    fn web_modes_accept_every_documented_spelling() {
        for raw in ["off", "OFF", "false", "0", "no"] {
            assert_eq!(
                config(&[("MQTT_HOST", "h"), ("WEB_INTERFACE", raw)])
                    .unwrap()
                    .web,
                WebMode::Off
            );
        }
        for raw in ["observe", "read-only", "ReadOnly"] {
            assert_eq!(
                config(&[("MQTT_HOST", "h"), ("WEB_INTERFACE", raw)])
                    .unwrap()
                    .web,
                WebMode::Observe
            );
        }
        for raw in ["full", "on", "true", "1", "yes"] {
            // There is no control mode any more. Saying `full` used to mean "the
            // page plus controls"; it now means nothing, and guessing on the
            // operator's behalf would leave them believing they still had a
            // control surface.
            assert!(
                config(&[("MQTT_HOST", "h"), ("WEB_INTERFACE", raw)]).is_err(),
                "{raw:?} should be rejected now that controls are gone"
            );
        }
    }

    #[test]
    fn an_invalid_value_names_the_variable_and_the_value() {
        let error = config(&[("MQTT_HOST", "h"), ("WEB_INTERFACE", "banana")])
            .unwrap_err()
            .to_string();
        assert!(error.contains("WEB_INTERFACE"), "got: {error}");
        assert!(error.contains("banana"), "got: {error}");

        let error = config(&[("MQTT_HOST", "h"), ("MQTT_PORT", "not-a-port")])
            .unwrap_err()
            .to_string();
        assert!(error.contains("MQTT_PORT"), "got: {error}");
    }
}
