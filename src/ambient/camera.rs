//! Camera capture: pixels for the sampler, an image for the browser.
//!
//! The capture loop is blocking by nature — `VIDIOC_DQBUF` parks the thread until
//! a frame arrives — so it runs on a thread of its own rather than inside the
//! runtime, where a blocking call would stall everything else sharing that worker.
//!
//! Two codings are supported, and the order matters:
//!
//! * **MJPEG** is preferred. The frames are already JPEG, so the preview is the
//!   camera's own bytes forwarded verbatim and nothing is encoded at all.
//! * **YUYV** is the fallback for cameras that will not give MJPEG at this size.
//!   It is converted to RGB here, and the preview is written as an uncompressed
//!   BMP at half resolution — no encoder, no crate, and browsers have displayed
//!   BMP since forever.
//!
//! Anything else is refused by name rather than silently producing garbage.

use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use tracing::warn;

use super::controls::{CameraRequest, CameraSetting};
use super::sampling::Frame;

/// The four character codes we know how to unpack.
pub const MJPEG: &[u8; 4] = b"MJPG";
pub const YUYV: &[u8; 4] = b"YUYV";

/// Content types the preview can carry, one per frame.
pub const TYPE_JPEG: &str = "image/jpeg";
pub const TYPE_BMP: &str = "image/bmp";

/// How the bytes a camera hands us have to be turned into pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Coding {
    Mjpeg,
    Yuyv,
}

/// Decode one MJPEG frame into RGB.
///
/// Platform-neutral on purpose: this is the half of capture that can be reasoned
/// about without a camera attached, and it is where a camera that quietly emits
/// something other than RGB24 gets caught by name rather than by a wrong picture.
pub fn decode_mjpeg(jpeg: &[u8]) -> anyhow::Result<(u32, u32, Vec<u8>)> {
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

/// Turn a packed YUYV (a.k.a. YUY2) frame into RGB.
///
/// Two pixels share one chroma pair, which is the whole point of the format: half
/// the bandwidth, at the cost of a colour resolution nobody can see in an average.
pub fn yuyv_to_rgb(data: &[u8], width: u32, height: u32) -> anyhow::Result<Vec<u8>> {
    let pixels = (width as usize) * (height as usize);
    let expected = pixels * 2;
    if data.len() < expected {
        anyhow::bail!(
            "expected {expected} bytes of YUYV for {width}x{height}, got {}",
            data.len()
        );
    }

    let mut rgb = Vec::with_capacity(pixels * 3);
    for pair in data[..expected].chunks_exact(4) {
        let (y0, u, y1, v) = (pair[0], pair[1], pair[2], pair[3]);
        rgb.extend_from_slice(&yuv_to_rgb(y0, u, v));
        rgb.extend_from_slice(&yuv_to_rgb(y1, u, v));
    }
    rgb.truncate(pixels * 3);
    Ok(rgb)
}

fn yuv_to_rgb(y: u8, u: u8, v: u8) -> [u8; 3] {
    // BT.601 full range, which is what UVC cameras emit.
    let luma = f32::from(y);
    let blue_difference = f32::from(u) - 128.0;
    let red_difference = f32::from(v) - 128.0;

    [
        clamp_byte(luma + 1.402 * red_difference),
        clamp_byte(luma - 0.344_136 * blue_difference - 0.714_136 * red_difference),
        clamp_byte(luma + 1.772 * blue_difference),
    ]
}

fn clamp_byte(value: f32) -> u8 {
    value.round().clamp(0.0, 255.0) as u8
}

/// Halve an RGB image by dropping every other pixel.
///
/// Nearest-neighbour on purpose: the preview exists so somebody can see where the
/// screen's corners are, and a 320x180 picture is plenty for that. It costs a
/// quarter of the bytes to send and a quarter of the time to write.
pub fn halve(rgb: &[u8], width: u32, height: u32) -> (u32, u32, Vec<u8>) {
    let half_width = (width / 2).max(1);
    let half_height = (height / 2).max(1);
    let stride = width as usize * 3;

    let mut out = Vec::with_capacity(half_width as usize * half_height as usize * 3);
    for y in 0..half_height as usize {
        let row = (y * 2) * stride;
        for x in 0..half_width as usize {
            let offset = row + (x * 2) * 3;
            out.extend_from_slice(&rgb[offset..offset + 3]);
        }
    }
    (half_width, half_height, out)
}

/// Wrap an RGB image in an uncompressed 24-bit BMP.
///
/// A BMP rather than a JPEG because encoding one costs nothing and needs no
/// crate: the header is fifty-four bytes and the rest is the pixels in BGR order.
pub fn bmp(rgb: &[u8], width: u32, height: u32) -> Vec<u8> {
    const HEADER: usize = 54;

    let row = width as usize * 3;
    let padding = (4 - row % 4) % 4;
    let stride = row + padding;
    let body = stride * height as usize;
    let total = HEADER + body;

    let mut out = Vec::with_capacity(total);
    out.extend_from_slice(b"BM");
    out.extend_from_slice(&(total as u32).to_le_bytes());
    out.extend_from_slice(&0_u32.to_le_bytes()); // reserved
    out.extend_from_slice(&(HEADER as u32).to_le_bytes());
    out.extend_from_slice(&40_u32.to_le_bytes()); // DIB header size
    out.extend_from_slice(&(width as i32).to_le_bytes());
    // Positive height means the rows are stored bottom-up, which is the format's
    // own default and one less thing to get wrong.
    out.extend_from_slice(&(height as i32).to_le_bytes());
    out.extend_from_slice(&1_u16.to_le_bytes()); // planes
    out.extend_from_slice(&24_u16.to_le_bytes()); // bits per pixel
    out.extend_from_slice(&0_u32.to_le_bytes()); // no compression
    out.extend_from_slice(&(body as u32).to_le_bytes());
    out.extend_from_slice(&2835_u32.to_le_bytes()); // pixels per metre, 72 dpi
    out.extend_from_slice(&2835_u32.to_le_bytes());
    out.extend_from_slice(&0_u32.to_le_bytes()); // palette entries
    out.extend_from_slice(&0_u32.to_le_bytes()); // important colours

    let stride_bytes = width as usize * 3;
    for y in (0..height as usize).rev() {
        let start = y * stride_bytes;
        for pixel in rgb[start..start + stride_bytes].chunks_exact(3) {
            out.extend_from_slice(&[pixel[2], pixel[1], pixel[0]]);
        }
        out.resize(out.len() + padding, 0);
    }
    out
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
    controls: std::sync::mpsc::Receiver<CameraRequest>,
    settings: Vec<CameraSetting>,
    shutdown: CancellationToken,
) {
    let name = "ambient-capture".to_owned();
    let spawned = std::thread::Builder::new().name(name).spawn(move || {
        while !shutdown.is_cancelled() {
            match capture(&path, &frames, &controls, &settings, &shutdown) {
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
    _controls: std::sync::mpsc::Receiver<CameraRequest>,
    _settings: Vec<CameraSetting>,
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
    controls: &std::sync::mpsc::Receiver<CameraRequest>,
    settings: &[CameraSetting],
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

    // Before the stream starts, so the first frame is already taken at the
    // settings the flat field was measured with.
    apply_settings(&device, settings);

    let (format, coding) = choose_format(&device)?;
    tracing::info!(
        camera = %path.display(),
        width = format.width,
        height = format.height,
        coding = ?coding,
        "ambient camera open"
    );

    let mut stream = Stream::with_buffers(&device, Type::VideoCapture, super::CAPTURE_BUFFERS)
        .context("preparing the capture buffers")?;

    loop {
        if shutdown.is_cancelled() {
            return Ok(());
        }

        // Requests are served between frames rather than from another thread: the
        // node cannot be queried twice while it is streaming, and a control change
        // is a few microseconds against a 66 ms frame.
        while let Ok(request) = controls.try_recv() {
            match request {
                CameraRequest::List(reply) => {
                    let _ = reply.send(list_controls(&device));
                }
                CameraRequest::Set { id, value, reply } => {
                    let _ = reply.send(set_control(&device, id, value));
                }
                CameraRequest::Quiet { id, value } => {
                    let _ = set_control(&device, id, value);
                }
                CameraRequest::Apply(settings) => {
                    apply_settings(&device, &settings);
                }
            }
        }

        let (buffer, meta) = stream.next().context("waiting for a frame")?;
        let used = meta.bytesused as usize;
        if used == 0 {
            continue;
        }
        let Some(bytes) = buffer.get(..used) else {
            continue;
        };

        match frame_from(bytes, coding, format.width, format.height) {
            Ok(frame) => {
                // A closed receiver just means nobody is watching; the camera
                // keeps running so that a page opened later has frames at once.
                let _ = frames.send(Some(Arc::new(frame)));
            }
            Err(err) => warn!(%err, "dropping a frame that would not decode"),
        }
    }
}

/// Pick the best coding the camera will actually give us.
///
/// Asking for a format is a negotiation, not a command: the driver is free to
/// answer with something else. So each candidate is checked against what came
/// back, and a camera that refuses both is told so by name.
#[cfg(target_os = "linux")]
fn choose_format(device: &v4l::device::Device) -> anyhow::Result<(v4l::format::Format, Coding)> {
    use v4l::format::{Format, FourCC};
    use v4l::video::Capture as _;

    for (fourcc, coding) in [(MJPEG, Coding::Mjpeg), (YUYV, Coding::Yuyv)] {
        let wanted = Format::new(
            super::CAPTURE_WIDTH,
            super::CAPTURE_HEIGHT,
            FourCC::new(fourcc),
        );

        match device.set_format(&wanted) {
            Ok(format) if format.fourcc == FourCC::new(fourcc) => return Ok((format, coding)),
            Ok(format) => tracing::debug!(
                got = ?format.fourcc,
                asked = %String::from_utf8_lossy(fourcc),
                "the camera refused this coding"
            ),
            Err(err) => tracing::debug!(
                %err,
                asked = %String::from_utf8_lossy(fourcc),
                "could not ask for this coding"
            ),
        }
    }

    anyhow::bail!(
        "this camera offers neither MJPEG nor YUYV at {}x{}; ambient capture needs one of them",
        super::CAPTURE_WIDTH,
        super::CAPTURE_HEIGHT
    )
}

/// Every control the camera offers, with its current value.
///
/// Compound and button controls are skipped: they have no single number to show,
/// and the ones that matter here — gain, exposure, white balance — are all plain
/// integers or menus.
#[cfg(target_os = "linux")]
pub fn list_controls(device: &v4l::device::Device) -> Vec<super::CameraControl> {
    use v4l::control::{Flags, MenuItem, Type, Value};

    let Ok(descriptions) = device.query_controls() else {
        return Vec::new();
    };

    let mut controls: Vec<super::CameraControl> = descriptions
        .into_iter()
        .filter_map(|description| {
            let kind = match description.typ {
                Type::Integer | Type::Integer64 | Type::IntegerMenu => "integer",
                Type::Boolean => "boolean",
                Type::Menu => "menu",
                _ => return None,
            };

            let value = match device.control(description.id) {
                Ok(control) => match control.value {
                    Value::Integer(number) => number,
                    Value::Boolean(flag) => i64::from(u8::from(flag)),
                    _ => description.default,
                },
                Err(_) => description.default,
            };

            let items = description
                .items
                .as_ref()
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|(index, item)| match item {
                            MenuItem::Name(name) => Some((*index, name.clone())),
                            MenuItem::Value(_) => None,
                        })
                        .collect()
                })
                .unwrap_or_default();

            Some(super::CameraControl {
                id: description.id,
                name: description.name,
                kind: kind.to_owned(),
                minimum: description.minimum,
                maximum: description.maximum,
                step: description.step as i64,
                default: description.default,
                value,
                inactive: description.flags.contains(Flags::INACTIVE),
                read_only: description.flags.contains(Flags::READ_ONLY),
                items,
            })
        })
        .collect();

    controls.sort_by(|a, b| a.name.cmp(&b.name));
    controls
}

/// Pin one of the camera's controls.
#[cfg(target_os = "linux")]
pub fn set_control(device: &v4l::device::Device, id: u32, value: i64) -> anyhow::Result<()> {
    use v4l::control::{Control, Value};
    use v4l::video::Capture as _;

    device
        .set_control(Control {
            id,
            value: Value::Integer(value),
        })
        .map_err(|err| anyhow::anyhow!("setting control {id} to {value}: {err}"))
}

/// Apply a stored set of settings, in an order that does not have to be right.
///
/// One control can be gated by another: `exposure_absolute` is inactive until
/// `exposure_auto` is off, and the file lists settings in the order they were
/// first touched rather than in dependency order. A second pass costs
/// microseconds and resolves any such ordering.
#[cfg(target_os = "linux")]
fn apply_settings(device: &v4l::device::Device, settings: &[super::CameraSetting]) {
    let mut failures = Vec::new();
    for pass in 0..2 {
        failures = settings
            .iter()
            .filter_map(|setting| {
                set_control(device, setting.id, setting.value)
                    .err()
                    .map(|err| (*setting, err.to_string()))
            })
            .collect();
        if failures.is_empty() || pass == 1 {
            break;
        }
    }

    for (setting, err) in failures {
        warn!(
            id = setting.id,
            value = setting.value,
            %err,
            "a stored camera setting was refused; the flat field may not match"
        );
    }
}

/// Turn one block of camera bytes into a frame the rest of the daemon can use.
pub fn frame_from(bytes: &[u8], coding: Coding, width: u32, height: u32) -> anyhow::Result<Frame> {
    match coding {
        Coding::Mjpeg => {
            let (width, height, rgb) = decode_mjpeg(bytes)?;
            Ok(Frame {
                width,
                height,
                rgb,
                preview: bytes.to_vec(),
                preview_type: TYPE_JPEG,
            })
        }
        Coding::Yuyv => {
            let rgb = yuyv_to_rgb(bytes, width, height)?;
            let (preview_width, preview_height, small) = halve(&rgb, width, height);
            Ok(Frame {
                width,
                height,
                rgb,
                preview: bmp(&small, preview_width, preview_height),
                preview_type: TYPE_BMP,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_neutral_yuyv_frame_comes_back_grey() {
        // Y=128 with neutral chroma is mid grey in every colour model.
        let rgb = yuyv_to_rgb(&[128, 128, 128, 128, 128, 128, 128, 128], 2, 2).unwrap();
        assert_eq!(rgb, [128_u8; 12]);
    }

    #[test]
    fn yuyv_chroma_is_applied_to_both_pixels_it_covers() {
        // V above centre pushes red up; both pixels share that chroma pair.
        let frame = [200, 128, 200, 200, 200, 128, 200, 200];
        let rgb = yuyv_to_rgb(&frame, 2, 2).unwrap();
        let first = &rgb[0..3];
        let second = &rgb[3..6];
        assert_eq!(first, second, "one chroma pair, one colour");
        assert!(first[0] > first[1] && first[0] > first[2], "got {first:?}");
    }

    #[test]
    fn a_short_yuyv_frame_is_refused_rather_than_read_past_the_end() {
        assert!(yuyv_to_rgb(&[0, 0, 0], 640, 480).is_err());
    }

    #[test]
    fn a_bmp_has_the_header_everyone_expects() {
        let image = vec![10, 20, 30, 40, 50, 60];
        let bmp = bmp(&image, 2, 1);

        assert_eq!(&bmp[0..2], b"BM");
        assert_eq!(bmp[18..22], 2_u32.to_le_bytes(), "width");
        assert_eq!(bmp[22..26], 1_u32.to_le_bytes(), "height");
        assert_eq!(bmp[28..30], 24_u16.to_le_bytes(), "bits per pixel");
        // Bottom-up and BGR: the one row is both pixels with the channels swapped.
        assert_eq!(&bmp[54..60], &[30, 20, 10, 60, 50, 40]);
    }

    #[test]
    fn halving_keeps_the_top_left_of_each_pair() {
        let mut image = Vec::new();
        for _ in 0..2 {
            for x in 0..4 {
                image.extend_from_slice(if x % 2 == 0 {
                    &[255, 0, 0]
                } else {
                    &[0, 255, 0]
                });
            }
        }

        let (width, height, small) = halve(&image, 4, 2);
        assert_eq!((width, height), (2, 1));
        assert_eq!(small, vec![255, 0, 0, 255, 0, 0]);
    }
}
