# Changelog

All notable changes to this project. The format follows
[Keep a Changelog](https://keepachangelog.com/), and the project adheres to
[Semantic Versioning](https://semver.org/).

## [Unreleased]

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

- **Container image and CI.** Multi-stage `cargo-chef` build, a `ci` workflow that
  formats, lints, tests, then boots the image and probes `/healthz`, and a
  `release` workflow that publishes to GHCR with SBOM and provenance attestations.
  Published for `linux/amd64` for now; arm64 is a one-line change once the
  repository is public and GitHub's free arm64 runners apply.
- **`/healthz`** and a `--healthcheck` subcommand, so the image needs no `curl`
  to report its own liveness.
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

### Notes

- `arm64` images, the Bluetooth link, MQTT, effects and alerts are all still to
  come. Nothing here is deployable against a real light yet.
