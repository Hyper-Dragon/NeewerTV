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
#[cfg(test)]
pub(crate) mod test_support;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use serde::Serialize;
use tokio::sync::{mpsc, oneshot, watch};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

pub use calibration::{Calibration, FrameSize};
pub use controls::{CameraControl, CameraSetting};
pub use geometry::{Homography, Rect, zone_rects};
pub use render::Render;
pub use sampling::{Correction, Frame, Measurement, Reading, Rgb, Sampler};

/// The name this mode is offered under in Home Assistant's effect list.
///
/// An effect rather than a switch on purpose: Home Assistant's light already has
/// one place to choose what a light is doing, and a second entity that fights the
/// first would need rules about which one wins. Choosing any other effect, colour
/// or brightness is then an ordinary command that happens to end this mode.
pub const MODE_EFFECT: &str = "TV Ambient";

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
///
/// A quarter and a bit by default, not a tenth: a letterboxed film puts its picture
/// in the middle and its bars over the rest of the panel, and the strip is stuck to
/// the frame rather than to the picture. Sampling a tenth of the height lands inside
/// the bars and reports black, which switches the backlight off during the film it
/// was bought for. Twenty-six percent still reads the bars on a very wide film, but
/// it reaches far enough in that the bands carry the picture's own colour, and it
/// leaves room for the corner squares to grow without meeting in the middle.
pub const DEFAULT_DEPTH: f32 = 0.26;
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

/// How many preview frames a diagnostic recording keeps.
///
/// Two minutes at the ten a second the link sustains: long enough to hold a
/// measurement, a scene change and the strip's response to both, and small
/// enough to paste into a message.
pub const RECORD_LIMIT: usize = 1200;

/// One frame of the pipeline as it was sent, for the diagnostic download.
///
/// Both ends of the transfer are kept: what the camera reported, and the exact
/// triple that went down the wire. Anything in between can be recomputed from
/// these two and the calibration that was in force.
#[derive(Debug, Clone, Serialize)]
pub struct RecordedFrame {
    /// Milliseconds since the recording started.
    pub ms: u64,
    /// Zone colours as the sampler reported them, 0–255 per channel.
    pub colours: [[u8; 3]; SEGMENTS],
    /// Segment entries as sent: lightness, hue, saturation.
    pub entries: [[u16; 3]; SEGMENTS],
    /// Zones the sampler flagged as over-exposed.
    pub clipped_zones: usize,
    pub brightness: u16,
    pub fps: f32,
}

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
    /// Bypass the flat field and the render transfer, for telling a camera that
    /// cannot see from a correction that is wrong.
    bypass: Arc<AtomicBool>,
    /// Whether the strip is meant to be following the camera, with or without a
    /// page open.
    ///
    /// This is the standing instruction, and Home Assistant is the only thing that
    /// sets it: it is written to the calibration file, because nothing else outlives
    /// a restart and a power cycle.
    mode: Arc<AtomicBool>,
    /// Whether the calibration page has asked for ambient for itself.
    ///
    /// Not written anywhere, and not published anywhere. The page is for setting the
    /// thing up, so it needs to see what it is setting up, but the mode that survives
    /// the browser being closed belongs to Home Assistant. Cleared when the page is
    /// opened, so nobody inherits a switch they did not throw.
    page_mode: Arc<AtomicBool>,
    mode_change: Arc<watch::Sender<bool>>,
    /// Frames captured for the diagnostic download, and whether it is running.
    recorder: Arc<Mutex<Recorder>>,
    measure: mpsc::Sender<MeasureReply>,
    camera: Option<std::sync::mpsc::Sender<controls::CameraRequest>>,
}

/// What the preview has been asked to remember.
#[derive(Default)]
struct Recorder {
    since: Option<Instant>,
    frames: Vec<RecordedFrame>,
}

impl Recorder {
    fn push(&mut self, mut frame: RecordedFrame) {
        let Some(since) = self.since else {
            return;
        };
        if self.frames.len() >= RECORD_LIMIT {
            return;
        }
        frame.ms = since.elapsed().as_millis() as u64;
        self.frames.push(frame);
    }
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
            // A fresh page starts with the override off: the mode belongs to Home
            // Assistant, and inheriting somebody else's switch would make the strip
            // follow the camera for reasons the operator cannot see.
            self.page_mode.store(false, Ordering::Relaxed);
            let _ = self.viewing.change.send(true);
            self.mode_change.send_modify(|_| {});
        }
        ViewGuard {
            holding: Arc::clone(&self.viewing),
        }
    }

    /// Whether a flat field is being measured right now.
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

    /// Start or stop recording preview frames for the diagnostic download.
    pub fn set_recording(&self, on: bool) {
        if let Ok(mut recorder) = self.recorder.lock() {
            recorder.since = on.then(Instant::now);
            if !on {
                // Left in place, so the download still has the last recording
                // after the operator stops it.
                return;
            }
            recorder.frames.clear();
        }
    }

    /// Whether a recording is running, and how many frames it holds.
    pub fn recording(&self) -> (bool, usize) {
        self.recorder
            .lock()
            .map(|recorder| (recorder.since.is_some(), recorder.frames.len()))
            .unwrap_or((false, 0))
    }

    /// Keep one frame, if a recording is running.
    pub fn record_frame(&self, frame: RecordedFrame) {
        if let Ok(mut recorder) = self.recorder.lock() {
            recorder.push(frame);
        }
    }

    /// Everything recorded so far.
    pub fn recorded(&self) -> Vec<RecordedFrame> {
        self.recorder
            .lock()
            .map(|recorder| recorder.frames.clone())
            .unwrap_or_default()
    }

    /// Whether the strip is following the camera of its own accord.
    pub fn mode(&self) -> bool {
        self.mode.load(Ordering::Relaxed)
    }

    /// A channel that says when that changes.
    pub fn mode_changes(&self) -> watch::Receiver<bool> {
        self.mode_change.subscribe()
    }

    /// Turn ambient mode on or off, and remember it.
    ///
    /// Remembered in the calibration file because nothing else survives: the strip
    /// forgets across a power cycle and the process forgets across a restart.
    pub fn set_mode(&self, on: bool) {
        self.mode.store(on, Ordering::Relaxed);
        let _ = self.mode_change.send(on);

        if let Ok(mut slot) = self.calibration.write()
            && let Some(stored) = slot.as_ref()
        {
            let mut calibration = (**stored).clone();
            if calibration.ambient != on {
                calibration.ambient = on;
                if let Err(err) = calibration.save(&self.path) {
                    warn!(%err, path = %self.path.display(), "the ambient mode could not be saved");
                    return;
                }
                *slot = Some(Arc::new(calibration));
            }
        }
    }

    /// Whether the calibration page has asked for ambient for itself.
    pub fn page_mode(&self) -> bool {
        self.page_mode.load(Ordering::Relaxed)
    }

    /// Drive the strip from the camera while the page is open, without touching the
    /// mode Home Assistant owns.
    ///
    /// Deliberately in memory only: no file, no discovery, no published state. What a
    /// browser leaves behind is not an instruction, and the only place an instruction
    /// can be set is Home Assistant.
    pub fn set_page_mode(&self, on: bool) {
        self.page_mode.store(on, Ordering::Relaxed);
        // Wake the bridge. The value travelling on this channel is the standing
        // mode; the page's switch is read beside it and only counts while a page is
        // open, so nothing here can outlive the browser.
        self.mode_change.send_modify(|_| {});
    }

    /// Whether the flat field is being bypassed right now.
    pub fn bypassing(&self) -> bool {
        self.bypass.load(Ordering::Relaxed)
    }

    /// Turn every correction off, or back on.
    ///
    /// This is the answer to "is it the camera or is it us". With it on the
    /// strip is driven from the raw zone colours, so if the picture looks right
    /// and the strip does not, the camera never saw the picture properly and no
    /// calibration will fix it.
    pub fn set_bypass(&self, off: bool) {
        self.bypass.store(off, Ordering::Relaxed);
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
    let viewing = Arc::new(Lease::new());
    let measuring = Arc::new(Lease::new());

    // Remembered across restarts by the file, and seeded from it here: a daemon
    // that restarts should come back whichever way the operator left it.
    let mode_on = stored.as_ref().is_some_and(|stored| stored.ambient);
    let mode = Arc::new(AtomicBool::new(mode_on));
    let (mode_tx, _) = watch::channel(mode_on);
    let mode_change = Arc::new(mode_tx);

    camera::spawn(
        device,
        frames_tx,
        camera_rx,
        settings,
        camera::Demand {
            watching: viewing.change.subscribe(),
            measuring: measuring.change.subscribe(),
            ambient: mode_change.subscribe(),
        },
        shutdown.clone(),
    );

    let bypass = Arc::new(AtomicBool::new(false));

    let calibration = Arc::new(RwLock::new(stored));

    tokio::spawn(sample_frames(
        frames_rx.clone(),
        latest_tx,
        Arc::clone(&calibration),
        max_fps,
        measure_rx,
        Arc::clone(&bypass),
        shutdown,
    ));

    Handle {
        latest: latest_rx,
        frames: frames_rx,
        calibration,
        path: calibration_path,
        viewing,
        measuring,
        bypass: Arc::clone(&bypass),
        mode,
        page_mode: Arc::new(AtomicBool::new(false)),
        mode_change,
        recorder: Arc::new(Mutex::new(Recorder::default())),
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
    bypass: Arc<AtomicBool>,
    shutdown: CancellationToken,
) {
    let mut sampler: Option<Sampler> = None;
    let mut sampler_for: Option<(Arc<Calibration>, bool)> = None;
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
        let bypassed = bypass.load(Ordering::Relaxed);
        let unchanged = match (&sampler_for, &current) {
            (Some((previous, previous_bypass)), Some(next)) => {
                Arc::ptr_eq(previous, next) && *previous_bypass == bypassed
            }
            (None, None) => true,
            _ => false,
        };
        if !unchanged {
            sampler = current
                .as_ref()
                .and_then(|next| Sampler::new_with(next, !bypassed));
            sampler_for = current.clone().map(|calibration| (calibration, bypassed));
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
            render: current
                .as_ref()
                .map_or_else(Render::default, |calibration| {
                    if bypassed {
                        Render::identity()
                    } else {
                        calibration.render
                    }
                }),
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A handle with a calibration behind it, and a path nothing reads.
    fn handle() -> Handle {
        Handle::for_test(
            Some(test_support::calibration()),
            test_support::scratch_path(),
        )
    }

    #[test]
    fn the_pages_switch_is_not_the_standing_mode() {
        // Only Home Assistant sets the mode, and only the mode survives the
        // browser. A page that wrote either of those would be a second place to
        // set the light, which is the thing this split exists to prevent.
        let handle = handle();
        handle.set_page_mode(true);

        assert!(handle.page_mode(), "the page got what it asked for");
        assert!(
            !handle.mode(),
            "and it did not become an instruction it does not own"
        );
        assert!(
            !handle.path().exists(),
            "a page's switch must not be written to the calibration file"
        );
    }

    #[test]
    fn opening_the_page_clears_an_override_left_behind() {
        // Otherwise a stale switch makes the strip follow the camera the moment
        // somebody opens the page to look at something else, for reasons they
        // cannot see on screen.
        let handle = handle();
        handle.set_page_mode(true);

        let viewer = handle.begin_viewing();
        assert!(!handle.page_mode(), "a new page starts with the switch off");
        drop(viewer);
    }
}
