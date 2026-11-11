//! The camera's own controls, as something the calibration page can draw.
//!
//! A camera is not a fixed instrument. Left alone it hunts: aimed at a dark film
//! it raises its own gain until black reads as mid grey, and aimed at a bright one
//! it winds down until the white field is a grey field. That is exactly the
//! behaviour the flat field is trying to measure, so the two fight each other —
//! the white field has to be captured at one setting and the black field at the
//! same one, or they describe two different cameras.
//!
//! The answer is to let the operator pin those settings from the calibration page
//! instead of turning the television down to five percent so the camera stops
//! clipping. The values are stored with the calibration, because they are part of
//! where the camera is and how it is set up, not part of how the daemon was
//! started.

use serde::{Deserialize, Serialize};

/// One of the camera's controls, in a shape the page can draw.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CameraControl {
    /// The V4L2 control id, which is what a setting is stored by.
    pub id: u32,
    pub name: String,
    /// `integer`, `boolean`, `menu` or `unsupported` — how to draw it.
    pub kind: String,
    pub minimum: i64,
    pub maximum: i64,
    pub step: i64,
    pub default: i64,
    pub value: i64,
    /// A control the driver will not accept right now, usually because another
    /// one it depends on is set differently. `exposure_absolute` is inactive
    /// while `exposure_auto` is on.
    pub inactive: bool,
    pub read_only: bool,
    /// Menu entries as `(value, label)`, empty for anything but a menu.
    #[serde(default)]
    pub items: Vec<(u32, String)>,
}

/// A camera control pinned to a value, stored with the calibration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CameraSetting {
    pub id: u32,
    pub value: i64,
}

/// A request to the thread that owns the camera device.
///
/// The device is opened and held by the capture thread, and a V4L2 node cannot be
/// queried from a second handle while it is streaming, so everything goes through
/// here and is applied between frames.
#[derive(Debug)]
pub enum CameraRequest {
    /// Report every control, with its current value.
    List(tokio::sync::oneshot::Sender<Vec<CameraControl>>),
    /// Set one control now.
    Set {
        id: u32,
        value: i64,
        reply: tokio::sync::oneshot::Sender<Result<(), String>>,
    },
    /// Set a control with nobody waiting, for a value that came from the store.
    Quiet { id: u32, value: i64 },
    /// Apply everything a calibration asked for.
    Apply(Vec<CameraSetting>),
}
