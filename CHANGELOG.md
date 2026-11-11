# Changelog

All notable changes to this project. The format follows
[Keep a Changelog](https://keepachangelog.com/), and the project adheres to
[Semantic Versioning](https://semver.org/).

## [Unreleased]

Nothing yet.

## [0.2.1] — 2026-11-11

Ambient mode. Point a camera at the television and the strip follows what is on it:
twenty zones sampled from twenty parts of the panel, one per addressable segment. No
screen capture and no display-server access, so it works from the same container with
nothing but a camera passed through.

It shows up in Home Assistant as a `TV Ambient` effect, sitting at the top of the
effect list. Pick it and the strip follows the camera with nothing else running;
pick anything else and the mode stops before your colour is sent. It survives a
restart and a power cut, because it is written to the calibration file — the only
thing that outlives both the daemon and the strip's own memory.

Setting it up is four clicked corners and two flat fields. That second part is what
makes it work: a camera left to itself hunts the scene and cancels out the screen's
colour, so the calibration page can pin the white balance, step the exposure until a
white field lands in a usable window, and then subtract the room and the lens shading
from every zone. It also keeps the tuning that makes the strip look like the picture
rather than like a camera reading, and shows you the exact numbers it is sending as
swatches so you can see the effect of a change before walking over to the telly.

Screenshots do not do it justice and neither does this paragraph. There is a camera
on the page, a zone map over it, and a `TV Ambient` switch to try it from.

### Added

- **Ambient capture**: V4L2 camera, four clicked corners, a homography, and twenty
  zone rectangles sampled from the frame. MJPEG is forwarded untouched, YUYV is
  converted here, and neither needs an encoder.
- **The `TV Ambient` effect**, offered only where a calibration exists — an effect
  that does nothing is worse than an absent one. Any other command ends the mode
  first, so a camera frame cannot overwrite a colour sixty milliseconds later.
- **Auto-calibration** from a white field: pin the white balance, force manual
  exposure, step it into range, and store the white field with the camera settings.
- **A flat field**, white and black, applied per sample in linear light — an affine
  correction does not commute with averaging.
- **A calibration page**: live preview with the zone map drawn on it, the camera's
  own controls, swatches drawn from the exact numbers the strip is sent, the six
  tuning values with a description each, and a recording you can download when
  something looks wrong.
- **A diagnostic download** bundling that recording with the calibration, the camera
  settings, the link state and the log.

### Fixed

- **The calibration page drew the wrong colour in its segment swatches.** Hue
  reached the page normalised to 0–255 rather than as a fraction of a turn, so
  every hue was stretched by 1.41× and wrapped past 181°: a blue zone was painted
  magenta. The swatches are the one place the strip's output can be judged without
  walking over to the television, and they had stopped describing what was being
  sent. The strip was never affected — only the preview of it — but every judgement
  made from the swatches was made against the wrong colour.
- **Auto-calibration pushed the camera onto its own highlight ceiling, which turns
  colour into white.** The white field was targeted at 0.75 in linear light, about
  sRGB 225, on the theory that a highlight in the picture should still have room.
  That camera tops out nearer sRGB 204 and desaturates as it approaches it, so a
  blue sky measured `204,204,204` and the strip painted it white — and nothing
  caught it, because clipping is counted at 252 and this camera can never reach 252
  to be counted. The target is now 0.45 (sRGB ~180) with the window at 0.30–0.65,
  which keeps the white field off the shoulder that throws the colour away.
- **A failed auto-calibration left the camera at its limit.** The search is the one
  thing that moves the camera without being asked to, and when it could not reach
  the window it stopped wherever it ran out — maximum exposure, then maximum gain —
  so the next ordinary picture came back as a flat wash with nothing in the log to
  explain it. A search that stores nothing now puts every control back where it
  found it.
- **Auto-calibration optimised whatever was in front of the camera.** A picture can
  never satisfy the white target, so the search ran to the limit. The zones now have
  to read evenly first — a white field does at any exposure, a picture does not —
  and anything else is refused before the camera is touched, with the same evidence
  `Measure white` has always asked for.
- **Clipping was counted at 252, which some cameras can never reach.** This camera
  saturates near sRGB 204 and desaturates on the way there, so its highlights were
  reported as ordinary colour and a blue sky was fed to the strip as white. The clip
  level is now taken from each frame's own peak: whatever the brightest sample is,
  anything on it is on the plateau. A full-range camera's peak is 255, so it reads
  the same as before.
- **The selected tab was the dark one.** The tab rules were written after the
  dark-mode block, and an equally specific rule that comes later wins, so the
  unselected tab kept its light background and the selected one kept its dark one —
  the opposite of what the class means.
- **The calibration page's ambient switch wrote the mode Home Assistant owns.**
  Ticking it to see what the strip would do saved `ambient` into the calibration file
  and published the effect, so the light stayed in ambient mode after the browser was
  closed. The page now has a switch of its own, held in memory and gated on the page
  being open: it writes nothing, publishes nothing, and means nothing once the preview
  ends. Leaving the screen hands the strip back to Home Assistant, which is the only
  place the mode can be set.

### Changed

- **The *Linear light* checkbox is gone; sampling is always linear.** There is no
  reading for which averaging gamma-encoded bytes is the right arithmetic, and a flat
  field only exists in linear light, so the box could only ever make the picture
  wrong.
- **The *Raw camera* checkbox is gone.** It was a diagnostic that had to be explained
  before it could be used. `POST /api/ambient/bypass` still exists for anyone who
  wants it from a script.
- **The six tuning boxes say what they do, and are clamped to the range beside
  them.** Typing an out-of-range value used to become a refused save with "the render
  settings are outside their ranges"; it now snaps back and says so. Values are shown
  rounded, so a gain reads `1.3` rather than `1.2999999523162842`.
- **A measurement answers with a toast** instead of leaving a pair of ticks under the
  buttons that could not say which run of which button put them there, or whether the
  run had warned about something.
- **The flat-field section says which button does what.** *Auto-calibrate* takes the
  white field and the camera settings; *Measure black* is still required because
  auto-calibration never looks at black; *Measure white* is the fallback for cameras
  with no absolute exposure control.
- **The page points at a white or black screen you already have** — there are ten-hour
  versions on YouTube — rather than assuming a calibration video nobody else has.
- **`TV Ambient` leads Home Assistant's effect list**, followed by the `TV Screen`
  family, with the vendor catalogue behind them. It used to be appended after fifty
  effects, which is where nobody finds it.
- **The calibration page has the same footer as the monitor page**, and its heading
  says *Experimental*, which is the one thing worth knowing before clicking corners.
- **The copy on the calibration page reads like help rather than notes.** The
  descriptions explained the mechanism but not the decision.
- **New defaults: a 26% band, a base glow of 1, smoothing of 0.2.** A tenth of the
  height samples the black bars of a letterboxed film and switches the backlight off
  during the film it was bought for. A base glow of 1 stops the strip's non-linear
  bottom end flickering between lit and off as a scene creeps towards black. 0.2
  smoothing takes the edge off sensor noise without lagging a cut by visible steps.

## [0.1.6] — 2026-11-04

An alert fired at a strip that was switched off used to flash at a device that was
not listening, and the restore that followed was the state it had been in all
along. It now switches the strip on, flashes, and puts it back — including what it
was showing, which outlives a power cycle.

### Fixed

- **An alert fired at a strip that was off flashed at a device that was not
  listening.** Every alert frame was a colour and a dim-to-nothing, and neither
  wakes a powered-off strip — so the flash never reached the room, and the
  restore that followed (`power(false)`) was the state the strip had been in all
  along. An alert now switches the strip on first, once, before the first flash.
- **An alert left its own colour behind on a strip it had switched on.** The
  device remembers its colour across a power cycle, so a red alert was still red
  the next time somebody turned the light on — hours later, with nothing to
  explain why. A strip that was off has its colour and brightness put back before
  the power goes off again.
- **An alert erased the effect a strip was running.** An effect survives a power
  cycle, so a strip left alone comes back running it — but an alert's restore only
  knew a colour, and writing that puts the device into colour mode: the effect was
  gone for good while Home Assistant went on listing it as selected, and picking
  it again did nothing because Home Assistant had nothing new to send. The
  presentation is now recorded separately from the power state, so a strip that
  was switched off is put back into the exact frames it was showing — an effect,
  a pattern or a colour — before the power goes off. When nothing was ever
  commanded the alert asks the strip, and rebuilds the effect it names.

### Changed

- **An alert asks the strip what it is showing before flashing at it**, bounded at
  400 ms and skipped entirely when the daemon already knows the colour from its
  own last command. That answer, not a guess, is what the restore replays.
- **A `feat/`, `fix/` or `test/` branch publishes a `test` image.** The
  `test-image` workflow builds the same multi-arch manifest as a release and tags
  it `test` and `test-<sha>`; `latest`, `edge` and every version tag still come
  only from the release job, so a branch build can be pulled onto a strip without
  disturbing anything already running the mainline.

## [0.1.5] — 2026-10-31

The repository is public, so the image can now be built for ARM as well — and it
is built on ARM hardware rather than under emulation.

### Added

- **arm64 images.** `ghcr.io/hyper-dragon/neewertv` is published as a multi-arch
  manifest — `linux/amd64` and `linux/arm64` behind the same tags. A Pi pulls the
  tag it always pulled and gets a binary compiled for the Pi; `docker pull` picks
  the right half by itself, and `docker buildx imagetools inspect` shows both.

### Changed

- **Each architecture is built on a runner of that architecture.** GitHub's arm64
  hosted runners are free for public repositories, so the ARM half is a native
  compile with no QEMU in the way: same Dockerfile, same `cargo build`, and a
  cache scope per platform so the two jobs do not evict each other's layers. A
  separate merge job puts both digests behind one manifest list, because a tag
  pointing at a single-architecture image is a lie until it does.

## [0.1.4] — 2026-10-31

Reconnect handling, written after a strip in the field lost its link three times
in seventeen minutes — 645 s, then 344 s, then 40 s — with the keep-alive firing
throughout and every reconnect succeeding on the first attempt two seconds later.

### Fixed

- **The strip was never asked what it was doing again.** `attach` cleared the
  "already asked" flag on every reconnect, because a fresh connection may be a
  strip that was power-cycled or driven by the vendor app while we were away — and
  the condition beside it required that nothing was known, which is true for ever
  after the first answer. The intent and the guard contradicted each other, and the
  guard won: the daemon drove a strip whose state it had not looked at since
  start-up. It now asks once per connection, which is safe because adoption still
  refuses to overwrite anything that was commanded.
- **A write could hang the daemon entirely.** Commands are ATT Write Requests with
  a response, and nothing put a deadline on the result. A strip that stops
  acknowledging therefore left the write pending, and the task awaiting it is the
  one that supervises the link: no drop counted, no keep-alive, no reconnect, and a
  daemon that appears to work and then quietly stops. Writes now give up after five
  seconds, which turns a wedged link into a counted drop and an automatic
  reconnect.

### Changed

- **A keep-alive that cannot be written is a warning, not a detail.** It means the
  strip has stopped taking traffic, and the disconnect that follows a few seconds
  later should not look like a mystery when the line above it says so.
- **The "~41 s means an idle timeout" heuristic is documented as a hint, not a
  diagnosis.** The same strip has since dropped at 344 s and at 40 s with the
  keep-alive running, so `BLE_KEEPALIVE_SECONDS` defends against one cause of a
  lost link and not all of them. `PROTOCOL.md` now records what the link actually
  does — reconnect times, link lifetimes, the 11 s a hard detach costs, and the
  fact that this adapter never reports RSSI.

## [0.1.3] — 2026-10-30

The daemon works end to end against a real strip: it holds the Bluetooth link,
appears in Home Assistant over MQTT, and puts the light back after an alert.

### Added

- **Bluetooth link.** Scans for the advertised name, connects, subscribes to
  notifications and writes frames. Every reconnect rebuilds from the adapter
  rather than reusing a handle, because after a power cycle the old handle refers
  to an object BlueZ has destroyed. Two devices matching the name with no address
  configured is an error naming both, not a guess.
- **MQTT.** Retained discovery, retained state, availability with a last will, a
  `set` topic accepting Home Assistant's `json` schema, and `alert/<colour>`.
- **Alerts.** Flash a named colour — red, orange, yellow, lime, green, cyan, blue,
  purple, pink or white — then restore exactly what was playing. The restore point
  is captured from user commands only, so an alert can never record its own frames
  and erase itself. An identifier that is not a colour still fires, in red, under
  its own name.

  The restore is deliberately more careful than "replay the last command": an
  alert ends by dimming the strip to nothing, so doing nothing is not neutral. A
  strip that was lit when the daemon met it is switched back on, and a command
  that only said `{"state":"ON"}` gets the known brightness put back with it.
  Without those two, the light came back dark and it looked as though the alert
  had switched it off.

  The flash also lasts. The frames used to be written once, in a few hundred
  milliseconds, leaving the strip dark for the rest of the ten seconds — which
  reads as the light having been switched off rather than as something asking for
  attention. One flash is now repeated twice a second until the duration runs out.
- **One-tap alert buttons.** Each enabled colour is published to Home Assistant as
  a button on the light's own device, carrying the configured length, so a
  doorbell is a press rather than an automation. `ALERT_BUTTONS` (default red,
  blue and green) picks them; a colour taken out is withdrawn from Home Assistant
  on the next start rather than left behind looking functional.
- **Web interface** in two modes — `off` and `observe` — where the mode decides
  which routes exist rather than which routes refuse. The page is a status
  readout with the daemon's own log tail on it, and nothing that can change the
  strip: control belongs on MQTT, where it already worked, and a second control
  path was only ever something else to keep correct.
- **Per-segment colour and the full effect catalogue**: 57 effects, with the
  lightness defect fixed so a black segment renders black.

### Fixed

- **Alerts expired the instant they fired.** The alert path read the wall clock
  while the ticker carried its own time; two origins meant a new alert was
  already past its deadline. All alert timing now flows through the ticker's
  clock.
- **The process exited without releasing the strip.** This left BlueZ holding the
  single connection and the device stopped advertising, so the *next* start
  failed with "no device matching". Shutdown now asks the worker to detach and
  waits for it.
- **The link dropped every 41 seconds when idle.** These controllers close a
  connection they have heard nothing on, and 41 seconds is the measured number —
  the log said `held_seconds=41` over and over. The keep-alive that exists for
  exactly this was set to 45 seconds, so it always fired too late and looked like
  it did nothing at all. It now runs every 20 seconds by default
  (`BLE_KEEPALIVE_SECONDS`).
- **The link loss was invisible.** Drops are now logged with how long the link had
  lasted, counted in the state JSON and shown on the page, which is how the 41
  seconds above was found rather than guessed.

- **Container image and CI.** Multi-stage `cargo-chef` build, a `ci` workflow that
  formats, lints, tests, then boots the image and probes `/healthz`, and a
  `release` workflow that publishes to GHCR with SBOM and provenance attestations.
  Published for `linux/amd64` for now; arm64 is a one-line change once the
  repository is public and GitHub's free arm64 runners apply.
- **`/healthz`** and a `--healthcheck` subcommand, so the image needs no `curl`
  to report its own liveness.
- **The colour the strip is showing is read, not guessed.** The state report
  carries it — type `2`, subtype 1, `[lightness, hueHi, hueLo, saturation]` — and
  the daemon never asked. Without it an alert that painted the strip red could
  only put back power and brightness afterwards, so the light stayed red and the
  alert looked like it had never ended. It is adopted on connect when nothing has
  been commanded (Home Assistant gets the real colour too), refused when the value
  is out of range or the segment is a gradient, and re-sent as the alert's restore.
- **Build identity.** The status page, `/api/state` and the startup log name the
  commit the image was built from (`0.1.1 (sha-3e07912)`), because "is this the
  container with the fix?" is a question that gets asked at the worst moment and
  two containers of the same version look identical in a browser tab. The value
  is stamped at build time by the workflows; a plain `cargo build` leaves it out
  rather than claiming a revision it does not know.
- **Configuration from the environment**, with `WEB_INTERFACE=off|observe`.
  An invalid value stops the container with a message naming the variable rather
  than defaulting into something surprising — `full` is now rejected rather than
  silently downgraded, so an old compose file fails loudly instead of quietly
  losing a feature.
- **Graceful shutdown** on `SIGINT`/`SIGTERM`, so a restart releases the
  Bluetooth connection instead of leaving the device believing it is still held.

### Documentation

- **`PROTOCOL.md` decodes the colour segment**, byte by byte, against a captured
  frame. The report carries the colour the strip is showing and the restore after
  an alert depends on it, so the encoding is now written down rather than inferred
  from a hex dump each time.
- **A dropped link is diagnosed in the right order** (`NOTES.md`): rule out a
  restarted container first, then the adapter's own kernel messages, and only then
  read a reason code from `btmon`. Written after a `dmesg` that showed nothing —
  correctly, because the kernel does not log disconnects at default verbosity —
  sent the investigation after the wrong end of the system.

### Notes

- Published for `linux/amd64` only. `arm64` needs one line changed in the release
  workflow once arm64 runners are available; nothing in the code is
  architecture-specific.
- The platform versions of `btleplug` and `rumqttc` are pinned deliberately:
  both have major releases in flight, and the BLE one changes the API this daemon
  is built around. Dependabot is configured to leave them alone.
