//! The calibration file: four clicked corners, plus the parameters that depend
//! on the room rather than on the picture.
//!
//! The camera is mounted once and never moves again, so this file is the durable
//! half of the feature. It is read on every start — including starts where the
//! calibration screen is switched off — and it is the only thing the web surface
//! is allowed to write.

use std::path::Path;

use serde::{Deserialize, Serialize};

use super::geometry::Homography;
use super::{
    ASPECT_MAX, ASPECT_MIN, CALIBRATION_VERSION, CORNERS, DEFAULT_ASPECT, DEFAULT_DEPTH, DEPTH_MAX,
    DEPTH_MIN, FRAME_MAX,
};

/// The size of the captured frame the corners were measured against.
///
/// Stored because it is what the operator calibrated *at*, and because a preview
/// stream needs to know how big to draw itself. It is not used to scale anything:
/// corners are normalised, so the calibration holds if the camera renegotiates a
/// different resolution with the same field of view.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrameSize {
    pub width: u32,
    pub height: u32,
}

impl FrameSize {
    pub const fn new(width: u32, height: u32) -> Self {
        Self { width, height }
    }

    pub const fn is_usable(self) -> bool {
        self.width > 0 && self.height > 0 && self.width <= FRAME_MAX && self.height <= FRAME_MAX
    }
}

/// Everything the sampler needs, and nothing else.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Calibration {
    pub version: u32,
    pub frame: FrameSize,
    #[serde(default = "default_aspect")]
    pub aspect: f32,
    /// Corner-square side and band thickness, as a fraction of screen height.
    #[serde(default = "default_depth")]
    pub depth: f32,
    /// Average in linear light rather than gamma-encoded sRGB.
    #[serde(default = "default_linear")]
    pub linear: bool,
    /// Top-left, top-right, bottom-right, bottom-left, normalised to the frame.
    ///
    /// The order is not negotiable: it defines which way the strip unwraps. Two
    /// swapped corners still produce a valid homography and a plausible picture,
    /// just one mirrored or rotated relative to the LEDs.
    pub corners: [[f32; 2]; CORNERS],
}

fn default_aspect() -> f32 {
    DEFAULT_ASPECT
}

fn default_depth() -> f32 {
    DEFAULT_DEPTH
}

fn default_linear() -> bool {
    true
}

/// Why a calibration was refused.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum CalibrationError {
    #[error("version {found} is not {expected}")]
    Version { found: u32, expected: u32 },
    #[error("frame {width}x{height} is not a usable capture size")]
    Frame { width: u32, height: u32 },
    #[error("aspect {0} is outside {ASPECT_MIN}..={ASPECT_MAX}")]
    Aspect(f32),
    #[error("depth {0} is outside {DEPTH_MIN}..={DEPTH_MAX}")]
    Depth(f32),
    #[error("corner {index} is ({x}, {y}), which is not inside the frame")]
    Corner { index: usize, x: f32, y: f32 },
    #[error("the corners are degenerate: three of them lie on one line")]
    Degenerate,
}

impl Calibration {
    /// Check everything the sampler and the file format rely on.
    pub fn validate(&self) -> Result<(), CalibrationError> {
        if self.version != CALIBRATION_VERSION {
            return Err(CalibrationError::Version {
                found: self.version,
                expected: CALIBRATION_VERSION,
            });
        }
        if !self.frame.is_usable() {
            return Err(CalibrationError::Frame {
                width: self.frame.width,
                height: self.frame.height,
            });
        }
        if !(ASPECT_MIN..=ASPECT_MAX).contains(&self.aspect) {
            return Err(CalibrationError::Aspect(self.aspect));
        }
        if !(DEPTH_MIN..=DEPTH_MAX).contains(&self.depth) {
            return Err(CalibrationError::Depth(self.depth));
        }
        for (index, [x, y]) in self.corners.iter().copied().enumerate() {
            if !(0.0..=1.0).contains(&x) || !(0.0..=1.0).contains(&y) {
                return Err(CalibrationError::Corner { index, x, y });
            }
        }
        if self.homography().is_none() {
            return Err(CalibrationError::Degenerate);
        }
        Ok(())
    }

    pub fn homography(&self) -> Option<Homography> {
        Homography::solve(&self.corners)
    }

    /// Read a calibration from disk.
    ///
    /// A file that exists but does not validate is an error rather than a silent
    /// fall back to defaults: the strip would then light the wrong part of the
    /// screen and there would be nothing in the log to explain it.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let raw = std::fs::read(path)
            .map_err(|err| anyhow::anyhow!("reading {}: {err}", path.display()))?;
        let calibration: Self = serde_json::from_slice(&raw)
            .map_err(|err| anyhow::anyhow!("parsing {}: {err}", path.display()))?;
        calibration
            .validate()
            .map_err(|err| anyhow::anyhow!("{} is not usable: {err}", path.display()))?;
        Ok(calibration)
    }

    /// Write a calibration, atomically.
    ///
    /// A half-written file would be worse than none: it would fail validation on
    /// the next start and take the mode down with it, so the write lands in a
    /// sibling file and is renamed over the top.
    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        self.validate()
            .map_err(|err| anyhow::anyhow!("refusing to save: {err}"))?;

        if let Some(parent) = path.parent().filter(|dir| !dir.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)
                .map_err(|err| anyhow::anyhow!("creating {}: {err}", parent.display()))?;
        }

        let json = serde_json::to_vec_pretty(self).context_serialise()?;
        let temporary = path.with_extension("json.tmp");
        std::fs::write(&temporary, &json)
            .map_err(|err| anyhow::anyhow!("writing {}: {err}", temporary.display()))?;
        std::fs::rename(&temporary, path)
            .map_err(|err| anyhow::anyhow!("renaming onto {}: {err}", path.display()))?;
        Ok(())
    }
}

trait SerialiseContext<T> {
    fn context_serialise(self) -> anyhow::Result<T>;
}

impl<T> SerialiseContext<T> for Result<T, serde_json::Error> {
    fn context_serialise(self) -> anyhow::Result<T> {
        self.map_err(|err| anyhow::anyhow!("serialising calibration: {err}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Calibration {
        Calibration {
            version: CALIBRATION_VERSION,
            frame: FrameSize::new(640, 480),
            aspect: DEFAULT_ASPECT,
            depth: DEFAULT_DEPTH,
            linear: true,
            corners: [[0.17, 0.15], [0.87, 0.10], [0.93, 0.90], [0.11, 0.87]],
        }
    }

    #[test]
    fn a_sensible_calibration_validates() {
        sample()
            .validate()
            .expect("the prototype's corners are usable");
    }

    #[test]
    fn a_corner_outside_the_frame_is_refused() {
        let mut calibration = sample();
        calibration.corners[2] = [1.4, 0.5];
        assert!(matches!(
            calibration.validate(),
            Err(CalibrationError::Corner { index: 2, .. })
        ));
    }

    #[test]
    fn an_unknown_version_is_refused_rather_than_guessed() {
        let mut calibration = sample();
        calibration.version = CALIBRATION_VERSION + 1;
        assert!(matches!(
            calibration.validate(),
            Err(CalibrationError::Version { .. })
        ));
    }

    #[test]
    fn depth_is_clamped_by_the_reader_not_silently_accepted() {
        let mut calibration = sample();
        calibration.depth = 0.9;
        assert!(matches!(
            calibration.validate(),
            Err(CalibrationError::Depth(_))
        ));
    }

    #[test]
    fn json_round_trips_through_a_real_file() {
        let calibration = sample();
        let directory = std::env::temp_dir().join(format!(
            "neewertv-calibration-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let path = directory.join("calibration.json");

        calibration.save(&path).expect("saving is writable");
        let loaded = Calibration::load(&path).expect("what was saved can be read");
        assert_eq!(loaded, calibration);

        // And the temporary file is not left behind for the next start to trip over.
        assert!(!path.with_extension("json.tmp").exists());

        std::fs::remove_dir_all(&directory).ok();
    }
}
