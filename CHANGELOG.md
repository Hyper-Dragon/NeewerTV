# Changelog

All notable changes to this project. The format follows
[Keep a Changelog](https://keepachangelog.com/), and the project adheres to
[Semantic Versioning](https://semver.org/).

## [Unreleased]

Nothing yet.

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
