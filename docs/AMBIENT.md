# Ambient capture and calibration

Status: **experimental**. Capture, geometry, sampling, flat-field calibration, the
calibration screen and a persistent ambient mode are implemented and tested.

## What it is

A camera pointed at the television, four clicked corners, and twenty averaged
colours: one per addressable segment of the strip. No screen capture and no
display-server access, so it works from inside the container with only a camera
device passed through.

The camera is expected to be mounted once and left alone, which is why the
calibration is a file on a volume rather than something held in memory.

## Enabling it

| Variable | Default | Meaning |
| --- | --- | --- |
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

```text
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
- **The link, not the camera, is the limit.** The camera samples at fifteen frames
  a second; one frame is a single 93-byte write, and that write is the whole
  update rate. That write is an **acknowledged** Write Command. The strip also
  advertises Write Without Response and accepts those without complaint, but it
  ignores them: the writes were counted as sent and the light sat still. An
  acknowledged write costs a round trip, and a handful of round trips a second is
  fine — the queue is what makes the rate survivable, draining to the newest frame
  rather than replaying a backlog, because a colour the screen has already left
  behind is not worth sending. The calibration page shows both rates — sampled and
  delivered — so the difference is visible.
- **A busy link is not a broken one.** BlueZ answers `In Progress` when its own
  queue is full, which at fifteen frames a second is ordinary. It used to be
  treated as a write failure, which marked the link down — and because the handle
  was still open the link never looked lost, so nothing reconnected and the light
  stayed frozen with a healthy-looking socket. It is now its own error, retried a
  few times at ten milliseconds, and the *frame* is dropped if it still will not
  go rather than the link.
- **No geometry in the browser.** `/api/ambient` returns each zone's four corners
  already projected into the frame, so the overlay is drawn from the numbers the
  sampler used. One implementation, in Rust, that cannot drift.

## Two ways to be driven

The strip follows the camera when **either** of two things asks it to, and they are
deliberately different kinds of thing.

**The preview is a lease.** While the calibration page is open the strip follows the
camera: twenty segments from the sampler, scaled by the brightness the light is
already set to. Its lifetime is the `/preview` stream, not a timer — it opens when
the page loads and closes when the page is left. A browser that vanishes without a
clean close still ends it (the stream is dropped). The worker restores whatever the
strip was doing before, including switching it back off if it was off; the restore
path is the alert path, capturing the strip's state in both of the forms it may be
in before taking it over and replaying the right one afterwards.

**Ambient mode is an instruction.** It is not tied to a page at all, which is the
point: it is what "this light is in ambient mode" means. It is offered to Home
Assistant as an effect called `TV Ambient`, in the same list as the hardware
effects, because Home Assistant already has one place to say what a light is doing
and a second entity would need rules about which of the two wins. It only appears
when a calibration exists — an effect that does nothing is worse than an absent one.

Choosing it is intercepted before the wire encoder, which would otherwise reject the
name. Choosing *anything else* — a colour, a brightness, a hardware effect,
`{"state":"OFF"}` — clears the mode first and then runs the command, so a camera
frame cannot overwrite a colour sixty-six milliseconds later. While the mode runs the
published effect is `TV Ambient`, and ending it puts back the effect Home Assistant
was told before.

The mode is stored in the calibration file, which is the only place that outlives
both a daemon restart and the strip forgetting everything across a power cycle.

Any command from MQTT also ends a preview, for the same reason it ends the mode.

**The camera is closed when nothing wants frames.** Both the lease and the mode feed
one `Demand` value, and the capture task closes the device when neither is asking.
That is what turns the camera's indicator off: a camera left open is a camera the
operator can see is watching, and in ambient mode it is never idle anyway.

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

## The camera's own settings

A flat field is only a measurement if the instrument did not move between the two
halves. Left alone, a camera does exactly that: aimed at a dark film it raises its
own gain until black reads as mid grey, aimed at a bright one it winds down until
the white field is a grey field. Measuring the white field means making the camera
stop clipping; measuring the black field means making it stop compensating. If it
compensates differently for each, the two halves describe two different cameras and
the correction is pointless.

The answer is not to turn the television down to five percent. The calibration page
lists the camera's own V4L2 controls — gain, exposure, white balance, power-line
frequency, backlight compensation — and lets them be pinned. Whatever is pinned is
stored with the calibration and applied when the device opens, so a restart comes
back to the same instrument rather than to whatever the camera decided today.

Two of them are worth knowing about:

- **Auto Exposure must be off before Exposure, Absolute can be set.** The driver
  reports the absolute control as `inactive` while auto is on, and refuses it.
  Settings are applied twice at open for this reason: one control can gate
  another, and the file lists them in the order they were first touched rather
  than in dependency order.
- **Power Line Frequency** should match the mains. Left on the wrong value the
  camera integrates over a fraction of a lighting cycle and the sampling picks up
  a beat that looks like the strip pulsing.

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
- **Colour is gated on lightness.** Below `FULL_COLOUR_LIGHTNESS` (45) saturation
  falls off quadratically to nothing. It is not a taste setting: in a dark scene
  the camera's noise has a *hue*, and multiplying that hue by a gain of two turns
  a black film into a slowly drifting rainbow. Dark zones go grey, which is what a
  dark zone should be.
- **A brightness ceiling and a base glow** — `maximum_brightness` (default 100) and
  `minimum_lightness` (default 1). The ceiling is for a strip that is far brighter
  than the panel it is imitating; the floor is for one that never quite lights. One
  rather than zero because the strip's bottom end is not linear — a segment at 1 to 3
  barely lights, so a scene creeping towards black flickers between lit and off.
  Both are clamp operations on the final lightness, applied after everything else.
- **Smoothing** — `smoothing`, default 0.2, capped at 0.95. 1.0 is refused rather
  than accepted: at 1.0 nothing ever arrives and the strip freezes on its first
  colour, which looks exactly like a crashed daemon.

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

- Reading the mode back from a strip that cannot report it. The mode is believed
  because this daemon set it; a strip power-cycled by someone else comes back lit
  in whatever colour it kept.
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

## The fields have to actually be black and white

The flat field is a measurement of the screen, so the screen has to be showing the
thing the measurement assumes. A calibration video encoded **studio range** puts
"black" at `Y=16` and "white" at `Y=235`, and a player that ignores the range flag —
which many do, including some TV apps and some browsers — stretches that to full
range anyway, or does not, depending on which one you got. Either way the strip is
measuring a grey field and calling it black, and no amount of gain tuning fixes a
reference that was wrong to begin with.

The videos under `D:\NeewerTvCalibration\` are encoded **full range** for this
reason: black is `Y=0`, white is `Y=255`, `pix_fmt=yuvj420p`, `-color_range pc`,
every filter chain carrying `scale=out_range=pc,format=yuv420p`. There is no range
flag left to be ignored.

Four hardware facts worth writing down, because each one cost a round of guessing:

1. **The black field measures about thirteen times darker after the range fix**,
   which is the size of the error and why it was worth chasing.
2. **Auto white balance has to be off.** Left on, the camera corrects for the
   screen's own colour and every zone reads roughly `(203, 203, 203)`. Auto-calibrate
   pins it at 6500 K.
3. **A dim, desaturated sample comes back from the firmware as near-white**, not as
   a dim colour — which is why the saturation gain exists at all.
4. **The camera tops out below full scale, and its ISP desaturates on the way.**
   This camera saturates near sRGB 204: a blue sky arrived as `204,204,204` —
   neutral, no hue — and the strip painted it white. A clip level of 252 could
   never fire, because 252 is unreachable, so it went unnoticed for days. The clip
   level is now taken from each frame's own peak, and the white-field target is
   0.45 linear (sRGB ~180) rather than 0.75, which keeps the field off the shoulder
   where the colour goes.
5. **The strip's own light lands on the screen** while a field is being measured, so
   the preview is suspended and the strip switched off for the duration. Otherwise
   the measurement includes the thing being calibrated.

## Build note

`v4l` pulls `v4l2-sys-mit`, which runs bindgen over `videodev2.h` at build time.
Both the Dockerfile's chef stage and the CI job install `clang` and `libclang-dev`
for that reason; without libclang the build script panics with "Unable to find
libclang", which reads like a broken checkout rather than a missing package. None
of it reaches the runtime image.
