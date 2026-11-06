# Ambient capture and calibration

Status: **experimental**. Capture, geometry, sampling, flat-field calibration and
the calibration screen are implemented and tested. While the calibration page is
open the strip follows the camera live; there is no persistent ambient mode yet.

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

Tier 0 arithmetic, then Tier 1 flat field.

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

## The flat field

The pale band down one side and the colour cast across a uniform white screen are
not the screen: they are the camera. A lens is dimmer at its edges than at its
centre, a camera is rarely square-on, and the room is rarely symmetric. Nothing
about that is visible in a single frame of a film, and nothing about it can be
worked out from the corner positions.

So it is measured instead. Two buttons on the calibration page:

- **White** with a full-screen white field up. Every zone should see the same
  colour, so every difference between them is the camera. Each zone-and-channel
  gets a gain of `mean ÷ measured`, which flattens the shading and leaves the
  overall exposure alone. Over-exposed zones are refused rather than corrected: a
  clipped sample was cut off before it was measured, so the gain it would produce
  is unknown. Turn the screen's brightness down, or move the camera back.
- **Black** with a full-screen black field up. This is the floor the camera reports
  with nothing on screen, and it is subtracted from every sample. A black field
  that reads as anything but black is refused — that is the room, not the screen.

Each measurement averages twenty frames, which is what makes sensor noise a
non-issue. Either field can be measured on its own: white alone already flattens
the shading, black alone already removes the floor.

Correction is applied per sample in linear light, before the average, because an
affine map does not commute with averaging — `(a − b)·g` averaged is not the same
as the average of `a` scaled.

Gains are clamped to 0.25–4.0. A zone that measured almost nothing would otherwise
get an enormous multiplier and turn sensor noise into a light show.

## What the strip is actually sent

`rgb_to_hsi` reports what the camera saw. The firmware does not render that well,
so a reading is lifted on the way out (`src/ambient/render.rs`):

- **The black point is reset first** — `black_level`, default 10. A camera without
  a locked exposure raises gain in the dark until a black screen reads as mid grey,
  and the strip then never dims; this is the software answer to that, and locking
  the exposure (Tier 3) is the real one.
- **Saturation is multiplied** — `saturation_gain`, default 2. A dim, desaturated
  sample, which is what an averaged low-light frame produces, comes back from the
  firmware as near-white instead of as the colour it describes.
- **Lightness is scaled above a small floor** — `lightness_gain`, default 1.4,
  floor 8. Measured on an NS03: segments do not light at all below about L=20. The
  floor exists so that a genuinely dark scene stays dark; lifting every zone by a
  constant would make a black film glow, which is the defect the whole feature
  exists to avoid.

Hue is never touched.

These three live in the calibration file, not in the environment, and are edited on
the calibration page with the swatches in front of you. They are judgements made
while looking at the screen and the strip together — a compose file cannot offer
that — and they belong to the screen they were tuned for. The page draws its
swatches from the same `(lightness, hue, saturation)` triples the strip is sent, so
what you see is what the strip will do.

All three are still guesses about the device's transfer curve. Measuring the curve
properly — camera pointed at the strip's own light, a known grid of
`(lightness, saturation)` — is what would turn them into facts.

## Not done yet

- The mode as a selectable state: a preview that only runs while a page is open is
  not the same thing as "this light is in ambient mode".
- Reading the mode back from a strip that cannot report it.
- **Tier 3, locking exposure.** This is the real fix for a camera that lifts black
  to mid grey in a dark scene, and it has to happen before the flat field is
  measured: the white and black fields have to be captured at the same gain, or the
  two halves describe two different cameras.
- **Tier 2, a colour-correction matrix.** A camera's red is not the screen's red,
  and the strip's red is not either. Fitting a 3×3 between camera and screen is a
  measurement, not a guess, and it is the step after the flat field.
- Sampling outside the screen, when the film's border does not fill the panel.
- Capturing the fields automatically from the calibration video rather than asking
  the operator to drive them by hand.

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
