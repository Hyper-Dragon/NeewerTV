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
V4L2 (MJPEG or YUYV) → pixels ─┬→ Sampler ─┬→ /api/ambient  (colours + outlines)
                               │           └→ worker        (20 segments, while previewing)
                               └→ /preview                 (an image per frame)
```

- **MJPEG first, YUYV second.** A camera that gives MJPEG has its own JPEGs
  forwarded to the browser verbatim, so nothing is encoded at all. A camera that
  only offers YUYV is converted here and its preview is written as an
  uncompressed BMP at half resolution — no encoder, no crate, and a 320x180
  picture is plenty for clicking corners. A camera offering neither is refused by
  name rather than silently producing garbage.
- **The frame rate is capped**, `AMBIENT_MAX_FPS`, default 15. The cap is on
  sampling, not on the preview: frames that arrive early are dropped rather than
  delayed, because a stale colour is worse than a missing one.
- **One write per frame, no queue.** A `watch` channel carries the newest frame
  and the newest reading. If sampling ever fell behind, skipping frames is the
  correct behaviour.
- **No geometry in the browser.** `/api/ambient` returns each zone's four corners
  already projected into the frame, so the overlay is drawn from the numbers the
  sampler used. One implementation, in Rust, that cannot drift.

## The preview is the lease

While the calibration page is open the strip follows the camera: twenty segments
from the sampler, scaled by the brightness the light is already set to.

The lifetime is the `/preview` stream, not a timer. It opens when the page loads
and closes when the page is left, and the worker restores whatever the strip was
doing before — including switching it back off if it was off. Two consequences
worth knowing: a browser that vanishes without a clean close still ends the
preview (the stream is dropped), and **any command from MQTT ends it too**, because
a user asking for a colour must not be overwritten by a camera frame 66 ms later.

The restore path is the alert path: capture the strip's state in both of the forms
it may be in before taking it over, and replay the right one afterwards.

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

- The mode as a selectable state: a preview that only runs while a page is open is
  not the same thing as "this light is in ambient mode".
- Reading the mode back from a strip that cannot report it.
- Tiers 1–3: flat-field and black-level correction, a colour-correction matrix, and
  locking exposure. For a calibration driven by a video on the television, locking
  exposure has to happen *first*, or the white and black fields are measured at
  different gains.

Aspect is fixed at 16:9 and is no longer a control on the page. Every strip this
supports is built around a 16:9 panel, and a wrong aspect silently skews the
section widths rather than failing — a knob that can only make things quietly
worse is not worth a row in the UI. The value stays in the calibration file so a
non-16:9 panel can still be described by editing it.

## Build note

`v4l` pulls `v4l2-sys-mit`, which runs bindgen over `videodev2.h` at build time.
Both the Dockerfile's chef stage and the CI job install `clang` and `libclang-dev`
for that reason; without libclang the build script panics with "Unable to find
libclang", which reads like a broken checkout rather than a missing package. None
of it reaches the runtime image.
