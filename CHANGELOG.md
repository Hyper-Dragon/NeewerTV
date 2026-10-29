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
  `set` topic accepting Home Assistant's `json` schema, and `alert/<id>`.
- **Alerts.** Flash, then restore exactly what was playing. The restore point is
  captured from user commands only, so an alert can never record its own frames
  and erase itself.
- **Web interface** in three modes — `off`, `observe`, `full` — where the mode
  decides which routes exist rather than which routes refuse.
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
- **Configuration from the environment**, with `WEB_INTERFACE=off|observe|full`.
  An invalid value stops the container with a message naming the variable rather
  than defaulting into something surprising.
- **Graceful shutdown** on `SIGINT`/`SIGTERM`, so a restart releases the
  Bluetooth connection instead of leaving the device believing it is still held.

### Notes

- `arm64` images, the Bluetooth link, MQTT, effects and alerts are all still to
  come. Nothing here is deployable against a real light yet.
