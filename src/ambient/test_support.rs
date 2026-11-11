//! A handle with no camera behind it.
//!
//! The switches on [`Handle`] — viewing, measuring, bypass, mode — are what the
//! rest of the daemon reacts to, and the Reacting is where the ordering bugs
//! live. Testing that ordering should not require a V4L2 device, so the handle is
//! assembled here with the channels connected and nothing feeding them.

use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use tokio::sync::{mpsc, watch};

use super::{Handle, Lease, Recorder};

/// A calibration with plausible corners and nothing measured.
///
/// Deserialised rather than built field by field so that adding a field to
/// [`super::Calibration`] does not mean editing a test fixture in another module.
pub fn calibration() -> super::Calibration {
    serde_json::from_value(serde_json::json!({
        "version": super::CALIBRATION_VERSION,
        "frame": { "width": 640, "height": 480 },
        "corners": [[0.17, 0.15], [0.87, 0.10], [0.93, 0.90], [0.11, 0.87]],
    }))
    .expect("the minimum a calibration file has to carry")
}

/// A path nothing should be written to in a test that does not care.
pub fn scratch_path() -> PathBuf {
    std::env::temp_dir().join(format!("neewertv-mode-{}.json", std::process::id()))
}

impl Handle {
    /// A handle for a deployment with no camera and no sampling task.
    ///
    /// `calibration` decides whether this looks like a deployment that has been
    /// set up. `path` is where [`Handle::set_mode`] would write, and nothing reads
    /// it: a mode set through this handle is not persisted.
    pub fn for_test(calibration: Option<super::Calibration>, path: PathBuf) -> Self {
        let (_, latest) = watch::channel(None::<Arc<super::Latest>>);
        let (_, frames) = watch::channel(None::<Arc<super::Frame>>);
        let (mode_change, _) = watch::channel(false);
        let (measure, _) = mpsc::channel(1);

        Self {
            latest,
            frames,
            calibration: Arc::new(RwLock::new(calibration.map(Arc::new))),
            path,
            viewing: Arc::new(Lease::new()),
            measuring: Arc::new(Lease::new()),
            bypass: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            mode: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            page_mode: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            mode_change: Arc::new(mode_change),
            recorder: Arc::new(std::sync::Mutex::new(Recorder::default())),
            measure,
            camera: None,
        }
    }
}
