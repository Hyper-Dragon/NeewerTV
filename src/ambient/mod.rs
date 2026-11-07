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
pub mod controls;
pub mod geometry;
pub mod render;
pub mod sampling;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use tokio::sync::{mpsc, oneshot, watch};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

pub use calibration::{Calibration, FrameSize};
pub use controls::{CameraControl, CameraSetting};
pub use geometry::{Homography, Rect, zone_rects};
pub use render::Render;
pub use sampling::{Correction, Frame, Measurement, Reading, Rgb, Sampler};

/// Format of the calibration file. Bumped when the meaning of a field changes.
///
/// Version 1 had no flat field and no render tuning. Version 2 added them, along
/// with the camera settings that pin the things the flat field is measured at.
/// Every one is supplied by a serde default when an older file is read, so an
/// old calibration keeps its corners and is rewritten at the current version.
pub const CALIBRATION_VERSION: u32 = 3;

/// How many frames a flat-field measurement averages over.
///
/// Enough to beat sensor noise, few enough that holding a white field for a
/// second and a half is plenty.
pub const MEASURE_FRAMES: usize = 20;

/// The channel a measurement request travels on.
type MeasureReply = oneshot::Sender<anyhow::Result<Measurement>>;

/// How long a measurement waits after the strip is told to go dark.
///
/// One BLE write plus a frame or two of camera time is all the hardware needs;
/// the rest is the panel and the room settling, which is slower than either.
const MEASURE_SETTLE: Duration = Duration::from_millis(600);

/// How long a measurement waits for one camera frame before giving up.
const MEASURE_TIMEOUT: Duration = Duration::from_secs(5);

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
    /// How the reading should be pushed towards what the firmware renders well.
    ///
    /// Carried with the reading rather than read from the environment, because it
    /// is part of the calibration: it is tuned while looking at the screen and the
    /// strip together, and it has to change when the calibration changes.
    pub render: Render,
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
            render: Render::default(),
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
    viewing: Arc<Lease>,
    measuring: Arc<Lease>,
    measure: mpsc::Sender<MeasureReply>,
    camera: Option<std::sync::mpsc::Sender<controls::CameraRequest>>,
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
            holding: Arc::clone(&self.viewing),
        }
    }

    /// Whether a flat field is being measured right now.
    ///
    /// The strip goes dark for the duration. Nothing else in the daemon can
    /// arrange that: the thing being measured and the light that pollutes the
    /// measurement are the same fixture.
    pub fn measuring(&self) -> watch::Receiver<bool> {
        self.measuring.change.subscribe()
    }

    fn begin_measuring(&self) -> MeasureGuard {
        if self.measuring.open_one() {
            let _ = self.measuring.change.send(true);
        }
        MeasureGuard {
            holding: Arc::clone(&self.measuring),
        }
    }

    /// Whether anybody is looking, for whoever drives the strip.
    pub fn watching(&self) -> watch::Receiver<bool> {
        self.viewing.change.subscribe()
    }

    /// Measure the screen as it is right now, averaged over a few frames.
    ///
    /// Goes through the sampling task rather than reading the frame channel here,
    /// because that task already owns the geometry and sees every frame.
    pub async fn measure(&self) -> anyhow::Result<Measurement> {
        // Taken for the whole call, including the settle: the strip has to be off
        // before the first frame is read, or the first frames describe a screen
        // lit by the light that is about to be switched off.
        let dark = self.begin_measuring();
        tokio::time::sleep(MEASURE_SETTLE).await;

        let answer = self.measure_now().await;
        drop(dark);
        answer
    }

    async fn measure_now(&self) -> anyhow::Result<Measurement> {
        let (reply, answer) = oneshot::channel();
        self.measure
            .send(reply)
            .await
            .map_err(|_| anyhow::anyhow!("the ambient sampler is not running"))?;
        answer
            .await
            .map_err(|_| anyhow::anyhow!("the ambient sampler dropped the measurement"))?
    }

    /// Persist a calibration and put it to work.
    ///
    /// The file is written first: a calibration that is running but was never
    /// saved would vanish at the next restart, and the operator would have no way
    /// to tell that from the mode having been broken.
    pub fn save(&self, calibration: Calibration) -> anyhow::Result<()> {
        calibration.save(&self.path)?;
        // A saved calibration is also a statement about how the camera is set, so
        // the device is brought into line with it. Otherwise the file and the
        // instrument drift apart and the next flat field is measured through
        // settings nobody chose.
        if let Some(camera) = self.camera.as_ref() {
            let _ = camera.send(controls::CameraRequest::Apply(calibration.camera.clone()));
        }
        if let Ok(mut slot) = self.calibration.write() {
            *slot = Some(Arc::new(calibration));
        }
        Ok(())
    }

    /// Every control the camera offers, with its current value.
    pub async fn camera_controls(&self) -> anyhow::Result<Vec<CameraControl>> {
        let Some(camera) = self.camera.as_ref() else {
            anyhow::bail!("this build has no camera to configure")
        };
        let (reply, answer) = oneshot::channel();
        camera
            .send(controls::CameraRequest::List(reply))
            .map_err(|_| anyhow::anyhow!("the camera is not running"))?;
        answer
            .await
            .map_err(|_| anyhow::anyhow!("the camera dropped the request"))
    }

    /// Pin one of the camera's controls, so a measurement is repeatable.
    pub async fn set_camera_control(&self, id: u32, value: i64) -> anyhow::Result<()> {
        let Some(camera) = self.camera.as_ref() else {
            anyhow::bail!("this build has no camera to configure")
        };
        let (reply, answer) = oneshot::channel();
        camera
            .send(controls::CameraRequest::Set { id, value, reply })
            .map_err(|_| anyhow::anyhow!("the camera is not running"))?;
        answer
            .await
            .map_err(|_| anyhow::anyhow!("the camera dropped the request"))?
            .map_err(|err| anyhow::anyhow!(err))
    }
}

/// How many preview streams are open, and a channel that says whether any are.
struct Lease {
    open: AtomicUsize,
    change: watch::Sender<bool>,
}

impl Lease {
    fn new() -> Self {
        let (change, _) = watch::channel(false);
        Self {
            open: AtomicUsize::new(0),
            change,
        }
    }

    /// Returns true when this was the first holder.
    fn open_one(&self) -> bool {
        self.open.fetch_add(1, Ordering::SeqCst) == 0
    }

    /// Returns true when this was the last one.
    fn close_one(&self) -> bool {
        self.open.fetch_sub(1, Ordering::SeqCst) == 1
    }

    fn release(&self) {
        if self.close_one() {
            let _ = self.change.send(false);
        }
    }
}

/// Keeps a preview lease alive for as long as it is held.
pub struct ViewGuard {
    holding: Arc<Lease>,
}

impl Drop for ViewGuard {
    fn drop(&mut self) {
        self.holding.release();
    }
}

/// Keeps the strip dark for as long as a field is being measured.
///
/// A flat field is a measurement of the screen, and the strip is a light source
/// aimed at the same room. Measuring the white field while the strip is showing
/// white floods the room, which lands in every zone as an uneven haze; measuring
/// the black field while it is showing a colour reads the light the strip is
/// throwing at the screen rather than the panel's own black. Neither is a
/// property of the camera, and both get baked into the correction.
pub struct MeasureGuard {
    holding: Arc<Lease>,
}

impl Drop for MeasureGuard {
    fn drop(&mut self) {
        self.holding.release();
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
    let (measure_tx, measure_rx) = mpsc::channel(1);

    // The camera thread owns the device, so its settings travel to it rather than
    // being applied from here. Unbounded on the sending side: a request is a few
    // bytes and must never fail because the thread is mid-frame.
    let settings = stored
        .as_ref()
        .map(|calibration| calibration.camera.clone())
        .unwrap_or_default();
    let (camera_tx, camera_rx) = std::sync::mpsc::channel();
    camera::spawn(device, frames_tx, camera_rx, settings, shutdown.clone());

    let calibration = Arc::new(RwLock::new(stored));

    tokio::spawn(sample_frames(
        frames_rx.clone(),
        latest_tx,
        Arc::clone(&calibration),
        max_fps,
        measure_rx,
        shutdown,
    ));

    Handle {
        latest: latest_rx,
        frames: frames_rx,
        calibration,
        path: calibration_path,
        viewing: Arc::new(Lease::new()),
        measuring: Arc::new(Lease::new()),
        measure: measure_tx,
        camera: Some(camera_tx),
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
    mut requests: mpsc::Receiver<MeasureReply>,
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
        // Rebuild only when the calibration actually changed: a save swaps the
        // `Arc`, so pointer equality is the test and no version counter is needed.
        // Done before the select so that a measurement request never meets a
        // sampler built from the calibration it is about to replace.
        let current = calibration.read().ok().and_then(|slot| slot.clone());
        let unchanged = match (&sampler_for, &current) {
            (Some(previous), Some(next)) => Arc::ptr_eq(previous, next),
            (None, None) => true,
            _ => false,
        };
        if !unchanged {
            sampler = current.as_ref().and_then(|next| Sampler::new(next));
            sampler_for = current.clone();
        }

        // The measurement arm has to borrow the frame channel too, so the select
        // only decides *what* happened and the work happens after it.
        let measured = tokio::select! {
            () = shutdown.cancelled() => break,
            reply = requests.recv() => reply,
            changed = frames.changed() => {
                if changed.is_err() {
                    break;
                }
                None
            }
        };

        if let Some(reply) = measured {
            let outcome = measure_over(&mut frames, sampler.as_ref(), MEASURE_FRAMES).await;
            let _ = reply.send(outcome);
            continue;
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
            render: current.map_or_else(Render::default, |calibration| calibration.render),
        })));
    }
}

/// Average the next few frames into one measurement.
///
async fn measure_over(
    frames: &mut watch::Receiver<Option<Arc<Frame>>>,
    sampler: Option<&Sampler>,
    count: usize,
) -> anyhow::Result<Measurement> {
    let Some(sampler) = sampler else {
        anyhow::bail!("there is no calibration to measure through yet");
    };

    let mut sum = [[0.0_f32; 3]; SEGMENTS];
    let mut worst = [0.0_f32; SEGMENTS];
    let mut seen = 0_usize;

    while seen < count {
        // A camera that has stopped publishing must not leave the request hanging
        // forever: the operator would see a button that never answers.
        match tokio::time::timeout(MEASURE_TIMEOUT, frames.changed()).await {
            Ok(Ok(())) => {}
            Ok(Err(_)) => anyhow::bail!("the camera stopped while the field was being measured"),
            Err(_) => anyhow::bail!("the camera is not publishing frames"),
        }

        let frame = {
            let borrowed = frames.borrow_and_update();
            (*borrowed).clone()
        };
        let Some(frame) = frame else {
            continue;
        };

        let measurement = sampler.measure(&frame);
        for (zone, channels) in measurement.linear.iter().enumerate() {
            for (channel, value) in channels.iter().enumerate() {
                sum[zone][channel] += value;
            }
            worst[zone] = worst[zone].max(measurement.clipped[zone]);
        }
        seen += 1;
    }

    let count = seen as f32;
    let mut linear = [[0.0_f32; 3]; SEGMENTS];
    for (zone, channels) in linear.iter_mut().enumerate() {
        for (channel, value) in channels.iter_mut().enumerate() {
            *value = sum[zone][channel] / count;
        }
    }

    Ok(Measurement {
        linear,
        clipped: worst,
    })
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
