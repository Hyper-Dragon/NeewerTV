//! V4L2 capture and MJPEG decode.
//!
//! The capture loop is blocking by nature — `VIDIOC_DQBUF` parks the thread until
//! a frame arrives — so it runs on a thread of its own rather than inside the
//! runtime, where a blocking call would stall everything else sharing that worker.
//!
//! MJPEG is the only pixel format asked for, and deliberately: the frames can
//! then be forwarded to the browser exactly as the camera produced them, and the
//! decode cost is paid once for the sampler rather than twice for a re-encode.

use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use tracing::warn;

use super::sampling::Frame;

/// The four character code the capture asks the camera for.
pub const FOURCC: &[u8; 4] = b"MJPG";

/// Decode one MJPEG frame into RGB.
///
/// Platform-neutral on purpose: this is the half of capture that can be reasoned
/// about without a camera attached, and it is where a camera that quietly emits
/// something other than RGB24 gets caught by name rather than by a wrong picture.
pub fn decode(jpeg: &[u8]) -> anyhow::Result<(u32, u32, Vec<u8>)> {
    let mut decoder = jpeg_decoder::Decoder::new(jpeg);
    let pixels = decoder
        .decode()
        .map_err(|err| anyhow::anyhow!("decoding MJPEG: {err}"))?;
    let info = decoder
        .info()
        .ok_or_else(|| anyhow::anyhow!("the JPEG carried no frame information"))?;
    if info.pixel_format != jpeg_decoder::PixelFormat::RGB24 {
        anyhow::bail!(
            "the camera produced {:?}, which is not RGB24",
            info.pixel_format
        );
    }
    Ok((u32::from(info.width), u32::from(info.height), pixels))
}

/// Open the camera and publish every frame it produces.
///
/// Returns immediately; the thread retries with a delay when the device is not
/// there yet, because a container that starts before the camera is plugged in is
/// a normal thing and should not need a restart to recover.
#[cfg(target_os = "linux")]
pub fn spawn(
    path: PathBuf,
    frames: watch::Sender<Option<Arc<Frame>>>,
    shutdown: CancellationToken,
) {
    let name = "ambient-capture".to_owned();
    let spawned = std::thread::Builder::new().name(name).spawn(move || {
        while !shutdown.is_cancelled() {
            match capture(&path, &frames, &shutdown) {
                // Only a cancelled shutdown returns `Ok`.
                Ok(()) => return,
                Err(err) => warn!(
                    camera = %path.display(),
                    %err,
                    "the camera is unavailable; retrying"
                ),
            }

            // Sleep in slices rather than once, so a shutdown is not held up for
            // the whole delay by a camera that simply is not plugged in.
            let mut waited = std::time::Duration::ZERO;
            while waited < super::RECONNECT_DELAY && !shutdown.is_cancelled() {
                std::thread::sleep(super::RECONNECT_SLICE);
                waited += super::RECONNECT_SLICE;
            }
        }
    });

    if let Err(err) = spawned {
        warn!(%err, "the capture thread could not be started; ambient capture is off");
    }
}

#[cfg(not(target_os = "linux"))]
pub fn spawn(
    path: PathBuf,
    _frames: watch::Sender<Option<Arc<Frame>>>,
    _shutdown: CancellationToken,
) {
    warn!(
        camera = %path.display(),
        "ambient capture is implemented for Linux V4L2 only; no camera will be opened on this build"
    );
}

#[cfg(target_os = "linux")]
fn capture(
    path: &std::path::Path,
    frames: &watch::Sender<Option<Arc<Frame>>>,
    shutdown: &CancellationToken,
) -> anyhow::Result<()> {
    use anyhow::Context as _;
    use v4l::buffer::Type;
    use v4l::device::Device;
    use v4l::format::{Format, FourCC};
    use v4l::io::mmap::Stream;
    use v4l::io::traits::CaptureStream as _;
    use v4l::video::Capture as _;

    let device = Device::with_path(path)
        .map_err(|err| anyhow::anyhow!("opening {}: {err}", path.display()))?;

    let wanted = Format::new(
        super::CAPTURE_WIDTH,
        super::CAPTURE_HEIGHT,
        FourCC::new(FOURCC),
    );
    let format = device
        .set_format(&wanted)
        .map_err(|err| anyhow::anyhow!("asking for MJPEG: {err}"))?;

    if format.fourcc != FourCC::new(FOURCC) {
        anyhow::bail!(
            "the camera settled on {:?} at {}x{}, and this daemon only accepts MJPEG",
            format.fourcc,
            format.width,
            format.height
        );
    }
    tracing::info!(
        camera = %path.display(),
        width = format.width,
        height = format.height,
        "ambient camera open"
    );

    let mut stream = Stream::with_buffers(&device, Type::VideoCapture, super::CAPTURE_BUFFERS)
        .context("preparing the capture buffers")?;

    loop {
        if shutdown.is_cancelled() {
            return Ok(());
        }

        let (buffer, meta) = stream.next().context("waiting for a frame")?;
        let used = meta.bytesused as usize;
        if used == 0 {
            continue;
        }
        let Some(jpeg) = buffer.get(..used) else {
            continue;
        };

        match decode(jpeg) {
            Ok((width, height, rgb)) => {
                let frame = Arc::new(Frame {
                    width,
                    height,
                    rgb,
                    jpeg: jpeg.to_vec(),
                });
                // A closed receiver just means nobody is watching; the camera
                // keeps running so that a page opened later has frames at once.
                let _ = frames.send(Some(frame));
            }
            Err(err) => warn!(%err, "dropping a frame that would not decode"),
        }
    }
}
