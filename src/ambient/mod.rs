//! Ambient screen sync: a camera pointed at the screen, twenty zone colours out.
//!
//! This module owns the capture, the rectified geometry, the colour sampling and
//! the calibration file. It deliberately does **not** talk to the strip: a
//! reading is published on a watch channel for whatever ends up driving LEDs.
//!
//! # Why the numbers are constants
//!
//! Everything below describes one piece of hardware — an NS03-series strip, which
//! has exactly twenty addressable segments and is designed to wrap a 16:9 panel.
//! Parameterising the zone count would mean carrying a layout search that has no
//! second caller, so the counts are fixed and named instead of being magic values
//! scattered through the arithmetic.

pub mod calibration;
pub mod camera;
pub mod geometry;
pub mod render;
pub mod sampling;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

pub use calibration::{Calibration, FrameSize};
pub use geometry::{Homography, Rect, zone_rects};
pub use sampling::{Frame, Reading, Rgb, Sampler};

/// Format of the calibration file. Bumped when the meaning of a field changes.
pub const CALIBRATION_VERSION: u32 = 1;

/// Addressable segments on the strip. Segments are equal length along the strip;
/// the *zones* they are fed from are not, because the corners are squares.
pub const SEGMENTS: usize = 20;

/// Corners of a screen: top-left, top-right, bottom-right, bottom-left.
pub const CORNERS: usize = 4;

/// Sections along the top and bottom edges, and down each side.
///
/// Six and two is the split that 20 zones and a 16:9 panel leave between the four
/// corner squares: `4 + 6 + 6 + 2 + 2`.
pub const TOP_SECTIONS: usize = 6;
pub const SIDE_SECTIONS: usize = 2;

pub const DEFAULT_ASPECT: f32 = 16.0 / 9.0;
pub const ASPECT_MIN: f32 = 0.5;
pub const ASPECT_MAX: f32 = 4.0;

/// Corner-square side and band thickness, as a fraction of screen height.
pub const DEFAULT_DEPTH: f32 = 0.10;
pub const DEPTH_MIN: f32 = 0.02;
/// A third of the short edge, beyond which the corner squares meet in the middle.
pub const DEPTH_MAX: f32 = 1.0 / 3.0;

/// Largest capture dimension accepted in a calibration file.
pub const FRAME_MAX: u32 = 8192;

/// At or above this a channel has no usable hue, because its real value is
/// unknown and the ratio between channels is therefore wrong.
pub const CLIP_LEVEL: u8 = 252;

/// Above this fraction of clipped samples the zone is reported as over-exposed.
pub const CLIP_SAMPLE_FRACTION: f32 = 0.35;

/// Clipped samples are dropped only while this fraction of the zone is clean.
///
/// Not arbitrary: a half-bright zone averaged from its clean half alone reads as
/// black, which is the defect this threshold exists to avoid.
pub const CLEAN_SAMPLE_FRACTION: f32 = 0.75;
pub const CLEAN_SAMPLE_MINIMUM: usize = 4;

/// Roughly one sample per this many pixels of the zone on screen.
pub const SAMPLE_PIXELS: f32 = 6.0;
pub const SAMPLES_MIN: usize = 3;
pub const SAMPLES_MAX: usize = 14;

/// Where in the zone the sample grid starts and ends, as a fraction of each edge.
///
/// Inset so that a sample can never land on the boundary and read a neighbouring
/// zone, or the bezel.
pub const INSET_X: f32 = 0.12;
pub const INSET_WIDTH: f32 = 0.76;
pub const INSET_Y: f32 = 0.14;
pub const INSET_HEIGHT: f32 = 0.72;

/// Exponential moving average applied to each zone's output every frame.
pub const SMOOTHING: f32 = 0.5;

/// Capture geometry asked of the camera.
pub const CAPTURE_WIDTH: u32 = 640;
pub const CAPTURE_HEIGHT: u32 = 480;
pub const CAPTURE_BUFFERS: u32 = 4;

/// How long to wait before reopening a camera that failed, and the slice the
/// wait is broken into so a shutdown is not delayed by it.
pub const RECONNECT_DELAY: Duration = Duration::from_millis(3000);
pub const RECONNECT_SLICE: Duration = Duration::from_millis(250);

/// The latest reading, as the browser and the (future) strip driver see it.
#[derive(Debug, Clone, PartialEq)]
pub struct Latest {
    pub frame: FrameSize,
    /// Index `n` drives segment `n`.
    pub colours: [Rgb; SEGMENTS],
    pub clipped: [bool; SEGMENTS],
    /// Zone corners projected into the frame and normalised, so the page can draw
    /// the overlay without implementing any geometry of its own.
    pub outlines: [[[f32; 2]; CORNERS]; SEGMENTS],
    pub clipped_zones: usize,
    /// Measured over the last completed second.
    pub fps: f32,
    /// False until a calibration has been saved: there is nothing to sample yet.
    pub calibrated: bool,
}

impl Latest {
    fn uncalibrated(frame: FrameSize, fps: f32) -> Self {
        Self {
            frame,
            colours: [[0; 3]; SEGMENTS],
            clipped: [false; SEGMENTS],
            outlines: [[[0.0; 2]; CORNERS]; SEGMENTS],
            clipped_zones: 0,
            fps,
            calibrated: false,
        }
    }
}

/// A running ambient service.
///
/// Cloning is cheap: the channels and the calibration slot are handles.
#[derive(Clone)]
pub struct Handle {
    latest: watch::Receiver<Option<Arc<Latest>>>,
    frames: watch::Receiver<Option<Arc<Frame>>>,
    calibration: Arc<RwLock<Option<Arc<Calibration>>>>,
    path: PathBuf,
    viewing: Arc<Viewing>,
}

impl Handle {
    pub fn latest(&self) -> Option<Arc<Latest>> {
        self.latest.borrow().clone()
    }

    /// The latest reading, for a task that wants to be told when it changes.
    pub fn latest_changes(&self) -> watch::Receiver<Option<Arc<Latest>>> {
        self.latest.clone()
    }

    /// Frames as the camera produced them, for the preview stream.
    pub fn frames(&self) -> watch::Receiver<Option<Arc<Frame>>> {
        self.frames.clone()
    }

    pub fn calibration(&self) -> Option<Arc<Calibration>> {
        self.calibration.read().ok().and_then(|slot| slot.clone())
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Announce that a browser is looking at the calibration page.
    ///
    /// The preview stream *is* the lease: it opens when the page loads and closes
    /// when the page is left, which is exactly the span over which the strip
    /// should follow the camera. It is held until the returned guard is dropped,
    /// which happens even when the connection dies mid-stream.
    pub fn begin_viewing(&self) -> ViewGuard {
        if self.viewing.open_one() {
            let _ = self.viewing.change.send(true);
        }
        ViewGuard {
            viewing: Arc::clone(&self.viewing),
        }
    }

    /// Whether anybody is looking, for whoever drives the strip.
    pub fn watching(&self) -> watch::Receiver<bool> {
        self.viewing.change.subscribe()
    }

    /// Persist a calibration and put it to work.
    ///
    /// The file is written first: a calibration that is running but was never
    /// saved would vanish at the next restart, and the operator would have no way
    /// to tell that from the mode having been broken.
    pub fn save(&self, calibration: Calibration) -> anyhow::Result<()> {
        calibration.save(&self.path)?;
        if let Ok(mut slot) = self.calibration.write() {
            *slot = Some(Arc::new(calibration));
        }
        Ok(())
    }
}

/// How many preview streams are open, and a channel that says whether any are.
struct Viewing {
    open: AtomicUsize,
    change: watch::Sender<bool>,
}

impl Viewing {
    /// Returns true when this was the first viewer.
    fn open_one(&self) -> bool {
        self.open.fetch_add(1, Ordering::SeqCst) == 0
    }

    /// Returns true when this was the last one.
    fn close_one(&self) -> bool {
        self.open.fetch_sub(1, Ordering::SeqCst) == 1
    }
}

/// Keeps a preview lease alive for as long as it is held.
pub struct ViewGuard {
    viewing: Arc<Viewing>,
}

impl Drop for ViewGuard {
    fn drop(&mut self) {
        if self.viewing.close_one() {
            let _ = self.viewing.change.send(false);
        }
    }
}

/// Start capture and sampling.
///
/// Returns a handle whether or not a calibration exists yet: the calibration
/// screen needs a preview *before* there is anything to sample with.
pub fn start(
    device: PathBuf,
    calibration_path: PathBuf,
    max_fps: u8,
    shutdown: CancellationToken,
) -> Handle {
    let stored = match Calibration::load(&calibration_path) {
        Ok(calibration) => {
            info!(
                path = %calibration_path.display(),
                frame = ?calibration.frame,
                aspect = calibration.aspect,
                depth = calibration.depth,
                "ambient calibration loaded"
            );
            Some(Arc::new(calibration))
        }
        Err(err) => {
            // A missing file is the normal first-run case. A file that exists but
            // will not load is not, and saying so is the only way it gets fixed.
            if calibration_path.exists() {
                warn!(
                    path = %calibration_path.display(),
                    %err,
                    "the stored calibration could not be used; calibrate again"
                );
            } else {
                info!(
                    path = %calibration_path.display(),
                    "no ambient calibration yet; open the calibration page to make one"
                );
            }
            None
        }
    };

    let (frames_tx, frames_rx) = watch::channel(None);
    let (latest_tx, latest_rx) = watch::channel(None);
    let (change, _) = watch::channel(false);
    let calibration = Arc::new(RwLock::new(stored));

    camera::spawn(device, frames_tx, shutdown.clone());

    tokio::spawn(sample_frames(
        frames_rx.clone(),
        latest_tx,
        Arc::clone(&calibration),
        max_fps,
        shutdown,
    ));

    Handle {
        latest: latest_rx,
        frames: frames_rx,
        calibration,
        path: calibration_path,
        viewing: Arc::new(Viewing {
            open: AtomicUsize::new(0),
            change,
        }),
    }
}

/// Sample every frame the camera publishes.
///
/// One frame in, one [`Latest`] out: no queue, no backlog. If sampling ever fell
/// behind, the right answer is to skip frames rather than to work through them
/// late, because a stale colour is worse than a fresh one.
async fn sample_frames(
    mut frames: watch::Receiver<Option<Arc<Frame>>>,
    latest: watch::Sender<Option<Arc<Latest>>>,
    calibration: Arc<RwLock<Option<Arc<Calibration>>>>,
    max_fps: u8,
    shutdown: CancellationToken,
) {
    let mut sampler: Option<Sampler> = None;
    let mut sampler_for: Option<Arc<Calibration>> = None;
    let mut window = FrameWindow::default();

    // A cap, not a rate: a frame that arrives early is dropped rather than
    // delayed, because a late colour is worse than a missing one. It keeps an
    // over-eager camera from spending a small box's whole budget on frames the
    // strip cannot be told about in time anyway. Zero means no cap at all.
    let interval = (max_fps > 0).then(|| Duration::from_secs_f64(1.0 / f64::from(max_fps)));
    let mut last_sampled: Option<Instant> = None;

    loop {
        tokio::select! {
            () = shutdown.cancelled() => break,
            changed = frames.changed() => {
                if changed.is_err() {
                    break;
                }
            }
        }

        let Some(frame) = frames.borrow_and_update().clone() else {
            continue;
        };

        if let Some(interval) = interval {
            let now = Instant::now();
            if last_sampled.is_some_and(|last| now.duration_since(last) < interval) {
                continue;
            }
            last_sampled = Some(now);
        }

        // Rebuild only when the calibration actually changed: a save swaps the
        // `Arc`, so pointer equality is the test and no version counter is needed.
        let current = calibration.read().ok().and_then(|slot| slot.clone());
        let unchanged = match (&sampler_for, &current) {
            (Some(previous), Some(next)) => Arc::ptr_eq(previous, next),
            (None, None) => true,
            _ => false,
        };
        if !unchanged {
            sampler = current.as_ref().and_then(|next| Sampler::new(next));
            sampler_for = current;
        }

        let fps = window.tick();

        let Some(sampler) = sampler.as_mut() else {
            let _ = latest.send(Some(Arc::new(Latest::uncalibrated(frame.size(), fps))));
            continue;
        };

        let reading = sampler.sample(&frame);
        let _ = latest.send(Some(Arc::new(Latest {
            frame: frame.size(),
            colours: reading.colours,
            clipped: reading.clipped,
            outlines: reading.outlines,
            clipped_zones: reading.clipped_zones,
            fps,
            calibrated: true,
        })));
    }
}

/// Frames per second, averaged over a second so the page does not flicker
/// between two numbers that are both momentarily true.
#[derive(Default)]
struct FrameWindow {
    started: Option<Instant>,
    counted: u32,
    fps: f32,
}

impl FrameWindow {
    fn tick(&mut self) -> f32 {
        let now = Instant::now();
        let started = *self.started.get_or_insert(now);
        self.counted += 1;

        let elapsed = now.duration_since(started).as_secs_f32();
        if elapsed >= 1.0 {
            self.fps = self.counted as f32 / elapsed;
            self.counted = 0;
            self.started = Some(now);
        }
        self.fps
    }
}
