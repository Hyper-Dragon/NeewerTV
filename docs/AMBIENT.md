# Ambient capture and calibration

Status: **experimental**. Capture, geometry, sampling and the calibration screen
are implemented and tested. Nothing sends the twenty colours to the strip yet —
that is the next slice.

## What it is

A camera pointed at the television, four clicked corners, and twenty averaged
colours: one per addressable segment of the strip. No screen capture and no
display-server access, so it works from inside the container with only a camera
device passed through.

The camera is expected to be mounted once and left alone, which is why the
calibration is a file on a volume rather than something held in memory.

## Enabling it

| Variable | Default | Meaning |
|---|---|---|
| `WEB_INTERFACE` | `observe` | Set to `ambient` to serve `/calibrate` and `/preview`. |
| `AMBIENT_CAMERA` | `/dev/video0` | The V4L2 device node. |
| `AMBIENT_CALIBRATION` | `/data/ambient-calibration.json` | Where the corners are stored. |

Capture starts when either the calibration screen is being served **or** a
calibration file already exists. That second condition is what lets you calibrate
once, switch `WEB_INTERFACE` back to `observe` or `off`, and keep sampling.

Pass the camera in with `--device /dev/video0`. The container runs as root for
BlueZ's sake, so no group is needed; if that ever changes, the node is
`root:video 0660`.

## The pipeline

```
V4L2 (MJPEG) → decode → Sampler ─┬→ /api/ambient  (colours + outlines, polled)
                                 └→ /preview      (the camera's own JPEGs)
```

- **MJPEG only.** The frames are forwarded to the browser byte for byte, so the
  preview costs no encode at all, and the decode is paid once for the sampler.
  A camera that refuses MJPEG is rejected by name at startup rather than silently
  producing garbage.
- **One write per frame, no queue.** A `watch` channel carries the newest frame
  and the newest reading. If sampling ever fell behind, skipping frames is the
  correct behaviour — a stale colour is worse than a fresh one.
- **No geometry in the browser.** `/api/ambient` returns each zone's four corners
  already projected into the frame, so the overlay is drawn from the numbers the
  sampler used. One implementation, in Rust, that cannot drift.

## Geometry

Rectified space is `x ∈ [0, aspect]`, `y ∈ [0, 1]`, origin at the top-left of the
screen, units physical. Every zone is an axis-aligned rectangle in that space and
perspective lives entirely in the homography — walking the perimeter instead puts
the corner bands on a normal that flips halfway round, and the corners come out
L-shaped with no depth value that fixes them.

The layout is fixed for this hardware: four corner squares of side `depth`, six
sections across the top, six across the bottom, two down each side.

Zone `n` (index `n − 1`) drives segment `n − 1`, with no remapping: the strip's
segment 0 sits just left of top centre and the index runs counter-clockwise, which
is the same convention the zone table uses.

`src/ambient/geometry.rs` asserts the published table for 16:9 at ten percent
depth. That check matters more than it looks: the browser prototype painted its
test card *with the zone map it then verified*, so a mirrored or mis-numbered
layout would have passed it.

The homography is solved in image coordinates normalised to `0..1`, which keeps
every term of the system the same order of magnitude. Pivoting alone cannot detect
a quad with three collinear corners — that system is consistent, it simply has a
singular solution — so the determinant is checked against the cube of the largest
entry, which makes the test scale-free.

## Colour

Tier 0 only: no calibration pass, just better arithmetic.

- **Average in linear light.** sRGB is gamma-encoded; a flat mean of encoded values
  under-reports brightness. A half-black, half-white zone reads about 126 encoded
  and about 195 linear, against an analytic 188.
- **Density from the zone's size on screen**, roughly one sample per 6×6 px,
  clamped to 3–14 per axis.
- **Clip rejection, but only while clipped samples are a minority.** Dropping every
  clipped sample turns a half-bright zone into its own dark half.
- **Temporal EMA** of 0.5 on the encoded output.
- Output rounds rather than truncates: a flat 128 comes back as 127.9999 and
  truncation would report 127, making a lossless round trip look lossy.

## Not done yet

- Sending the colours to the strip, and the precedence rule that makes the mode
  yield to any explicit command.
- Reading the mode back from a strip that cannot report it.
- Tiers 1–3: flat-field and black-level correction, a colour-correction matrix, and
  locking exposure. For a calibration driven by a video on the television, locking
  exposure has to happen *first*, or the white and black fields are measured at
  different gains.

## Build note

`v4l` pulls `v4l2-sys-mit`, which runs bindgen over `videodev2.h` at build time.
Both the Dockerfile's chef stage and the CI job install `clang` and `libclang-dev`
for that reason; without libclang the build script panics with "Unable to find
libclang", which reads like a broken checkout rather than a missing package. None
of it reaches the runtime image.
