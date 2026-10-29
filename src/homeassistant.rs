//! Home Assistant's MQTT vocabulary.
//!
//! Every literal in this module belongs to Home Assistant, not to this project:
//! the payload words it publishes and expects, the topic suffixes it documents,
//! and the shape of a discovery topic. They are gathered here so that the rest
//! of the daemon never contains a bare `"ON"` that a reader has to guess the
//! meaning and origin of.
//!
//! Nothing here should be renamed to taste. These are wire values defined by
//! somebody else's integration; changing one breaks the contract silently.

/// Payload for a light that is on.
pub const PAYLOAD_ON: &str = "ON";

/// Payload for a light that is off.
pub const PAYLOAD_OFF: &str = "OFF";

/// Availability payload when this daemon is reachable.
pub const AVAILABILITY_ONLINE: &str = "online";

/// Availability payload when it is not — also the last-will message, so the
/// broker says it for us if the process dies.
pub const AVAILABILITY_OFFLINE: &str = "offline";

/// Suffixes appended to the configured topic base.
pub mod topic {
    /// Retained: what the daemon last commanded.
    pub const STATE: &str = "state";

    /// Where Home Assistant sends commands.
    pub const SET: &str = "set";

    /// Retained, with a last will.
    pub const AVAILABILITY: &str = "availability";

    /// Alerts are per-identifier, so this is a prefix rather than a suffix.
    pub const ALERT_PREFIX: &str = "alert/";

    /// Every alert identifier in one subscription.
    pub const ALERT_WILDCARD: &str = "alert/+";
}

/// Building blocks of a discovery topic: `{prefix}/{component}/{object}/config`.
pub mod discovery {
    pub const COMPONENT_LIGHT: &str = "light";
    pub const CONFIG: &str = "config";

    /// The topic Home Assistant watches for this device's discovery document.
    #[must_use]
    pub fn topic(prefix: &str, device_id: &str) -> String {
        format!("{prefix}/{COMPONENT_LIGHT}/{device_id}/{CONFIG}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_payload_words_are_the_ones_home_assistant_uses() {
        // Pinned deliberately. These are the strings in Home Assistant's
        // documentation; if one of them ever looks wrong, the bug is here and
        // not in the caller.
        assert_eq!(PAYLOAD_ON, "ON");
        assert_eq!(PAYLOAD_OFF, "OFF");
        assert_eq!(AVAILABILITY_ONLINE, "online");
        assert_eq!(AVAILABILITY_OFFLINE, "offline");
    }

    #[test]
    fn topic_suffixes_compose_without_a_doubled_separator() {
        // `topic("")` is handled by the caller; what matters is that no suffix
        // carries a leading slash, which would produce `base//state`.
        for suffix in [
            topic::STATE,
            topic::SET,
            topic::AVAILABILITY,
            topic::ALERT_PREFIX,
            topic::ALERT_WILDCARD,
        ] {
            assert!(!suffix.starts_with('/'), "{suffix:?} starts with a slash");
        }
    }

    #[test]
    fn a_discovery_topic_is_prefix_component_object_config() {
        // Home Assistant's documented layout. The device id is the object id, so
        // two strips on one broker land on different topics.
        assert_eq!(
            discovery::topic("homeassistant", "neewer_ns03"),
            "homeassistant/light/neewer_ns03/config"
        );
    }
}
