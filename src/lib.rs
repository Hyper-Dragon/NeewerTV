//! `neewertv` — a bridge from a NEEWER NS03-series TV backlight to MQTT.
//!
//! The library exists so that the pieces with no runtime dependencies can be
//! tested, and reused, without dragging in a tokio runtime or a Bluetooth
//! adapter. In particular [`protocol`] is pure: it turns intent into bytes and
//! bytes into state, and touches nothing else.

pub mod app;
pub mod ble;
pub mod config;
pub mod homeassistant;
pub mod http;
pub mod logs;
pub mod mqtt;
pub mod protocol;
