# NeewerTV for [Home Assistant](https://www.home-assistant.io/)

## 'The TV backlight you already own, acting like a light you actually want.'

Your NEEWER NS03 is perfectly good hardware wrapped in a phone app. NeewerTV is a
small container that talks to it over Bluetooth and puts it on MQTT, so Home
Assistant gets a light entity, your automations get something to talk to, and the
vendor app gets to retire quietly.

```
  Home Assistant  ──┐                            ┌── Bluetooth ──► strip
  your scripts    ──┼──► MQTT broker ──► neewertv ─┘
  the web UI      ──┘        ▲                 │
                             └─────────────────┘  state, availability, discovery
```

Think of it as a translator. A patient one, that holds the single Bluetooth
connection the strip allows so your automations never fight the app for it — and
one that will flash red when someone rings the doorbell, then put the lights back
exactly as they were.

**This project is not affiliated with, endorsed by or supported by NEEWER in any
way.** It exists because the alternative was reverse-engineering a protocol, and
that turned out to be easier than living without a doorbell light. Parts of that
work are best effort and are marked as such; [docs/PROTOCOL.md](docs/PROTOCOL.md)
says so at the top rather than in a footnote.

## About

NeewerTV is a self-hosted bridge for NEEWER NS03-series TV backlights. It holds
the one Bluetooth connection the strip permits, publishes it to MQTT with Home
Assistant autodiscovery, and owns the save-and-restore bookkeeping that an
automation should not have to do.

What you get:

- **A real light.** Power, brightness, colour and the full effect list. No YAML,
  no HACS, no custom component, no restarting Home Assistant seventeen times.
- **Alerts that clean up after themselves.** Fire one and the strip reacts; when
  it's done it goes back to exactly what it was doing. Two overlapping alerts
  don't leave it stuck on whichever arrived second.
- **Per-segment colour**, if you're that way inclined. It's 20 addressable zones,
  not one bulb.
- **A web page**, for when you want to see what it's thinking or poke the light
  without opening Home Assistant.
- **No cloud, no account, no telemetry.** It talks to your broker and to nowhere
  else — see [PRIV_POLICY.md](PRIV_POLICY.md).

It works on my machine. Your mileage may vary, and if it does, see
[Known Issues](#known-issues).

## Where does it run?

**Anywhere that can see the strip and the broker.** It does not run on your Home
Assistant box, and it does not need to.

The container touches exactly two things: the Bluetooth adapter in the machine it
is running on, and your MQTT broker over the network. That's it. So the sensible
home for it is whatever small machine already lives near the telly with a
Bluetooth dongle in it — a Pi, a mini PC, a NAS — while Home Assistant carries on
sitting wherever it sits.

They can be in the same room, different rooms, or different buildings. The only
link between them is MQTT, so if both can reach the broker, nothing else matters:
no shared host, no add-on, no extra port to forward, no VPN between the two.

```
  Home Assistant ──┐
  your scripts   ──┼──► MQTT broker ──► neewertv ──► Bluetooth ──► strip
  the web page   ──┘                          ↑
                                   this is the only part that has
                                   to be near the telly
```

What it *does* need is a Bluetooth adapter that can actually see the strip. That's
the one piece of geography that matters, and it belongs to the machine running the
container, not to Home Assistant.

**Having a Bluetooth adapter and being able to reach the strip are different
things.** 2.4 GHz goes through walls badly, a TV cabinet full of metal is an
effective shield, and a cheap dongle buried behind a case is not the same radio as
the one on the spec sheet. So the qualification isn't "this machine has
Bluetooth" — it's "a scan from this machine finds the strip". That's the test in
step 1, and if it comes back empty, a different machine (or a USB dongle on a short
extension) is the answer. No amount of software fixes a radio that can't hear it.

## Prerequisites

### Minimum

- A NEEWER NS03 strip ¹
- A Linux machine that can **actually see the strip** over Bluetooth ² — not
  merely one with an adapter, and not necessarily your Home Assistant box
- Docker, with Compose
- An MQTT broker ³
- The strip to itself ⁴

¹ The RGBIC thing stuck to the back of your telly. The 5 m is `PD20250145`, the
3 m is `PD20250103`; both are supported.

² A Raspberry Pi, a mini PC, the NAS that's already running your broker — anything
with a working BlueZ and a radio that can reach the strip from where it sits. This
is very often *not* the machine running Home Assistant, and that's fine: see
[Where does it run?](#where-does-it-run). What it must be is somewhere a scan
actually finds `NH-PD…` — step 1 is the test, and owning an adapter is not the
same thing.

³ The Mosquitto add-on you already run for Home Assistant counts. You *can* run
without one if you only want the web page, but at that point you've built a very
elaborate light switch.

⁴ It accepts exactly one Bluetooth connection. Close the vendor app, or spend a
confusing evening wondering why nothing will connect.

## Installation

Three steps, and the middle one is the only one that ever causes trouble.

### 1 — Teach the machine to talk Bluetooth

None of this is NeewerTV-specific; it's the same prep as any BLE project on Linux.

```bash
sudo apt install bluez
sudo systemctl enable --now bluetooth

# Is there an adapter, and is it on?
bluetoothctl show

# Can you actually see the strip? Power it on first.
bluetoothctl --timeout 15 scan on | grep -i NH-PD
```

If the strip shows up in that last command, you're done with the host. Go and
pretend you never typed `bluetoothctl`.

If it doesn't show up, stop here. That machine cannot run this container, and no
amount of configuration will change its mind — it either can hear the strip or it
can't. Try it from somewhere with fewer walls in the way, or plug a USB Bluetooth
dongle into a short extension so it isn't buried behind the case.

### 2 — Hand the container the radio

This is the bit everyone worries about, so let's be clear: **you are not passing
through a device.** Bluetooth is a radio in the host, not a `/dev` node. There is
no `/dev/ttyUSB0` to hunt down, no `--privileged`, no `--network host`, no
`--cap-add` and no dark rituals. If a guide tells you to add those, it is solving
a different problem.

The container reaches the radio over the host's D-Bus socket. That is the whole
trick, and it takes two lines, both already in the example compose file:

```yaml
user: "0"                                                      # ¹
volumes:
  - /run/dbus/system_bus_socket:/run/dbus/system_bus_socket    # ²
```

¹ BlueZ's D-Bus policy on the host grants `org.bluez` to root and to members of the
`bluetooth` group. A container's uid *is* the host's uid, so an unprivileged one is
simply refused at the bus — and the failure announces itself as "no adapter found",
which is not the error message anyone would have chosen. If running as root bothers
you (it should bother you a little), add a `<policy user="…">` block to the host's
`/etc/dbus-1/system.d/bluetooth.conf` and set `user:` to match. That is the only
alternative; there is no third.

² One socket, deliberately — not the whole `/run/dbus` directory. Mounting the
directory hands the container every bus on the host, which is a much bigger gift
than the one you were trying to give.

Change either of those and you will have a bad evening. Leave them alone and you
won't.

### 3 — Start it

No build, no toolchain, no compile. The image lives at
`ghcr.io/hyper-dragon/neewertv`.

```bash
# the example files
curl -o compose.yml https://raw.githubusercontent.com/Hyper-Dragon/NeewerTV/main/docker-compose.example.yml
curl -o .env        https://raw.githubusercontent.com/Hyper-Dragon/NeewerTV/main/.env.example

$EDITOR .env          # set MQTT_HOST at minimum
docker compose up -d
docker logs -f neewertv
```

It looks like a lot but it isn't really. Two downloads, a text editor and one
command.

## Usage

### Home Assistant

Discovery is automatic. The daemon publishes a retained discovery payload and the
device appears under **Settings → Devices & Services → MQTT** as **TV Backlight**,
with no manual YAML and no restart. Power, brightness, colour and every effect are
on the entity's card.

There is also a status page at <http://localhost:8080/>, which has a link back
here, because eventually someone finds it open in a browser with no idea what it
is.

### Alerts

Publish a duration in seconds to `<base>/alert/<name>` and the strip flashes red;
when the time is up it returns to whatever it was doing before. That's the whole
doorbell story — no snapshot in your automation, no restore step:

```yaml
action: mqtt.publish
data:
  topic: neewertv/alert/doorbell
  payload: "10"
```

`<name>` is yours to choose. Each alert carries a priority and a duration, and a
person changing the light during an alert always wins: their command is applied
and the alert's restore is discarded, rather than the light fighting them.

One caveat worth knowing before you build a doorbell out of it: every alert that
arrives over MQTT currently has the same priority, so a second one firing while
the first is still showing is dropped rather than replacing it. Priorities exist
in the plumbing; nothing sets one yet.

### The web page

`WEB_INTERFACE` decides which routes **exist**, not which ones refuse you. A route
that was never mounted cannot be reached by a mistake in a check someone adds
later, which is the kind of bug you only find out about when it matters.

- `off` — no listener at all. MQTT only.
- `observe` — health, state and a page. Nothing that can change the strip.
- `full` — the above, plus controls.

It is **unauthenticated by design**: it's a page for a light, on a network you
already trust. If that isn't your network, read [SECURITY.md](SECURITY.md) before
you put the port anywhere you don't control.

## Configuration

Everything is an environment variable. There is no config file to drift out of
sync with reality, and a bad value stops the container with a message naming the
variable you got wrong. Same values whether you put them in `.env` or straight into
the compose file.

| Variable | Default | What it does |
|---|---|---|
| `MQTT_HOST` | **required** | Broker hostname or IP. |
| `MQTT_PORT` | `1883` | Broker port. |
| `MQTT_USERNAME` | — | Broker username. |
| `MQTT_PASSWORD` | — | Broker password. |
| `MQTT_BASE` | `neewertv` | Topic prefix for state, commands and alerts. |
| `DISC_PREFIX` | `homeassistant` | Home Assistant's discovery prefix. Only change it if you changed it there. |
| `DEVICE_ID` | `neewer_ns03` | Unique id for the entity. **Must differ if you run two containers.** |
| `DEVICE_NAME` | `TV Backlight` | The name Home Assistant shows. |
| `BLE_NAME_MATCH` | `NH-PD*` | Pattern for the advertised name. A pattern rather than an exact name because the name comes from the *model*, not the unit — the 5 m advertises `NH-PD20250145`, the 3 m `NH-PD20250103`. |
| `BLE_DEVICE_ADDRESS` | — | Which strip this container owns, when more than one matches. Leave it unset with a single strip. |
| `BLE_SCAN_SECONDS` | `12` | How long to scan before concluding the strip isn't there. Raise it if yours is slow to advertise. |
| `WEB_INTERFACE` | `full` | `off`, `observe` (read-only) or `full`. `on` is accepted as an alias for `full`. |
| `WEB_BIND` | `0.0.0.0:8080` | Where the web page listens. |
| `LOG_LEVEL` | `info` | A `tracing` directive, so `info,neewertv::ble=debug` works. |
| `LOG_FORMAT` | `text` | `text`, or `json` if you're shipping logs somewhere. |
| `DBUS_SYSTEM_BUS_ADDRESS` | — | Set to `unix:path=/run/dbus/system_bus_socket` inside the container. The compose file does it for you. |

## Known Issues

- **One strip per container.** Two strips means two containers, with different
  `MQTT_BASE` and `DEVICE_ID`, and — if they're the same model — a
  `BLE_DEVICE_ADDRESS` each. Same model advertises the same name, and the daemon
  refuses to guess which one you meant rather than binding to whichever the scan
  happened to return first. See [docs/NOTES.md](docs/NOTES.md).
- **No arm64 image yet.** amd64 only, for now. On an ARM host, build your own:
  `docker build -t neewertv .`
- **It's a container, not a package.** No `.deb`, no snap, no Home Assistant
  add-on, nothing on a store shelf.
- **You can run it on the host**, without Docker, if you insist — Rust 1.85+, a
  `cargo build --release`, and the same D-Bus access. That's a build-it-yourself
  path and it isn't supported, but nothing stops you. Short section in
  [docs/NOTES.md](docs/NOTES.md).
- **No custom scene files, and no renaming or hiding the built-in effects yet.**
  You get the 57 effects the firmware has, under the names the vendor app gives
  them.
- **`unavailable` means the radio, not the app.** Availability follows the
  Bluetooth link rather than the process, so a strip that is switched off at the
  wall reads as unavailable. That is deliberate — it genuinely cannot be
  controlled — but it does mean the entity goes dark when the TV does.
- **Alerts don't queue.** They all arrive at the same priority, so an alert that
  fires while another is showing is dropped, not stacked behind it.

## Version History

**v0.1.0** — first release. Light entity, effects, per-segment colour, alerts with
automatic restore, availability, web interface, amd64 container image.

Older detail lives in [CHANGELOG.md](CHANGELOG.md).

## Dependencies

- [btleplug](https://crates.io/crates/btleplug) — the Bluetooth layer.
- [rumqttc](https://crates.io/crates/rumqttc) — MQTT.
- [axum](https://crates.io/crates/axum) — the optional web interface.
- [BlueZ](http://www.bluez.org/) — the Linux Bluetooth stack, on your host.
- [Mosquitto](https://mosquitto.org/) or any MQTT 3.1.1 broker, on your network.
- [Home Assistant](https://www.home-assistant.io/) — optional, but it's why you're
  here.

## Future plans

- arm64 images — waiting on this repository being public and the CI minutes that
  come with it.
- Custom scenes in a config file, plus an editorial layer to hide or rename the
  built-in effects you never use.
- More alert patterns than "flash red, three times".

No dates. This is a hobby project with a real strip attached to it, and hobbies
have a way of pausing.

## Acknowledgements

- The [BlueZ](http://www.bluez.org/) project, without which nothing on Linux talks
  to anything over Bluetooth.
- The maintainers of [btleplug](https://crates.io/crates/btleplug) and
  [rumqttc](https://crates.io/crates/rumqttc), for making the hard parts boring.
- NEEWER's own application, without which there would have been nothing to learn
  from — and which we'd rather not have needed.
- Anyone who has ever written up a reverse-engineered protocol so the next person
  didn't have to start from a packet capture and a bad mood.

## Saying Thank You

This was written by me, initially for me. I put in the extra work to make it usable
by someone who isn't me, which is the tedious part. If you find it useful and want
to say thanks, you can [sponsor it on GitHub](https://github.com/sponsors/Hyper-Dragon)
or [bung a few quid in the hat](https://www.paypal.com/donate/?hosted_button_id=QNKADKV5BAM5C).
No pressure.

## Licence

[MIT](LICENSE).

## Software Support

Report defects on the [issues page](https://github.com/Hyper-Dragon/NeewerTV/issues/new/choose).
Say which strip you have if you can — the 3 m and the 5 m are different enough to
matter, and "it doesn't work" is a hard bug report to action.
