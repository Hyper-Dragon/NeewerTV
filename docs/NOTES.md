# Notes

The operational detail that used to be in the README, plus the reasoning behind
the decisions that look arbitrary until you've been bitten by them.

If you just want to run the thing, the [README](../README.md) is the shorter read.
This file is for when something's gone wrong, or you're about to try two strips,
or you're wondering why a container needs to be root.

## Status

Working, and verified against a real strip. The daemon holds the Bluetooth link,
presents the strip to Home Assistant over MQTT, and restores the previous state
after an alert.

| | Area | State |
|---|---|---|
| ✅ | Build, CI, multi-stage image, `/healthz` | done |
| ✅ | Bluetooth link: scan, connect, notify, write, recover | done, verified on hardware |
| ✅ | MQTT light: state, `set`, availability, HA discovery | done, verified on hardware |
| ✅ | Effects: presets, scenes, DIY patterns, per-segment colour | done |
| ✅ | Alerts: fire a pattern, then put the strip back | done, verified on hardware |
| ✅ | Web interface: status page and log tail, read-only | done |
| ⬜ | arm64 images | waiting on the repository being public |

## Why this exists at all

Three things make this light annoying to automate, and none of them are the
light's fault:

- **The vendor app owns the device.** One Bluetooth connection at a time, so the
  app and your automation can never both have it.
- **There is no API.** Not local, not cloud. The only way to change anything is to
  *be* the connected client.
- **Scenes are a sticky selection, not an event.** "Set effect X" leaves it set.
  What you actually want for a doorbell is *flash, then go back to whatever it was
  doing* — which needs something to remember the previous state, and that
  something is this daemon.

## Where it lives

The container needs exactly two things: a Bluetooth adapter within radio range of
the strip, and a network route to the broker. Nothing else.

"Within radio range" is doing real work in that sentence. The machine has to be one
that a scan actually finds the strip from — 2.4 GHz does not go through walls or a
metal TV cabinet generously, and it does not care how good the adapter looked on
paper. If `bluetoothctl --timeout 15 scan on` returns nothing from a machine, that
machine is not a candidate, whatever else is installed on it. A USB dongle on a
short extension usually beats a better adapter buried inside a case.

So it does **not** have to run on the Home Assistant host, and usually shouldn't —
put it wherever the radio and the strip already are, and let Home Assistant sit
wherever it likes. Same room, different room, different building; the only link
between the two is MQTT. There's no add-on to install, no shared volume, no port to
forward between them, and no reason to run it somewhere with no Bluetooth in it.

## The two load-bearing lines in the compose file

Changing either of these does not produce an error message that mentions them. It
produces "no adapter found", and then you have a bad evening.

### `user: "0"`

BlueZ's D-Bus policy on the host grants `org.bluez` to root and to members of the
`bluetooth` group. A container's uid *is* the host's uid — there's no remapping
between them — so an unprivileged uid is simply refused at the bus.

If running as root bothers you (it should bother you a little), the alternative is
to add a `<policy user="…">` block to the host's
`/etc/dbus-1/system.d/bluetooth.conf` and set `user:` to match. That is the only
alternative; there is no third.

### `/run/dbus/system_bus_socket`

A single socket, deliberately — not the whole `/run/dbus` directory. Mounting the
directory hands the container every bus on the host, which is a much bigger gift
than the radio access you were trying to give it.

And that really is all. **Bluetooth is a radio in the host, not a device node**, so
there is nothing else to pass through: no `/dev` entry, no `--privileged`, no host
networking. If a guide tells you to add those, it's solving a different problem.

## Two strips

Not supported by a single container, and it's worth knowing how it fails, because
it's the first thing someone with a big TV and a small one will try. **One
container, one strip** — run a second container for the second strip.

Discovery matches on the advertised name, and that name comes from the *model*, not
the unit, so two strips of the same model advertise identically. The daemon cannot
tell them apart by name, and it will not guess: if more than one matches and no
address is set, it refuses to start and lists every candidate it found.

Set `BLE_DEVICE_ADDRESS` to pick one. It's a public address that survived a power
cycle in testing, so this is normally a set-once value — but a long power-off is
the untested case, so if your strip ever comes back with a different address, the
daemon logs the address it found on every connect and you can update the value.

Running two containers needs two different `MQTT_BASE` values *and* two different
`DEVICE_ID` values. Two instances sharing one base collide on the retained state
topic; sharing a `DEVICE_ID` collides on the discovery `unique_id`. Home Assistant
renders either of those as one light flickering between two devices:

```yaml
MQTT_BASE: neewertv/lounge   # and neewertv/bedroom on the other
DEVICE_ID: neewer_lounge     # and neewer_bedroom
```

Both status pages carry the Bluetooth address and the device id of the strip they
belong to, so two browser tabs are never ambiguous even when both containers are
called the same thing.

## Home Assistant

Discovery is automatic. The daemon publishes a retained discovery payload and the
device appears under MQTT with no manual YAML, no custom component and no restart.

It also publishes a **button per alert colour** — `ALERT_BUTTONS`, `red,blue,green`
by default. Pressing one flashes the strip that colour for `ALERT_BUTTON_LENGTH`
seconds and then puts it back, which is the whole "turn the backlight red when the
doorbell goes" job with no automation behind it. The buttons attach to the light's
own device, so they appear on the same card rather than as loose entities.

Colours you take out of `ALERT_BUTTONS` are **withdrawn** on the next start: the
daemon publishes an empty retained payload to their discovery topics, which is the
only way to delete a retained message and therefore the only way to take a
discovered entity away. Without that, a button you had switched off would sit there
looking functional and publish an alert nobody was listening for.

If it doesn't appear:

1. Check that Home Assistant's MQTT integration points at the same broker.
2. `docker logs neewertv` — the daemon logs the discovery topic it published.
3. Subscribe to `<base>/availability` by hand. If it's silent, the daemon isn't
   connected to the broker; if it says `offline`, the daemon is up but can't reach
   the strip (see below) — neither is a discovery problem.

### What `unavailable` actually means

Availability follows the Bluetooth link, not the process. So Home Assistant showing
the light as unavailable means one of:

- the strip has no power (TV off, plug pulled — it's USB-powered, so this is
  normal);
- another client is holding the connection (the vendor app);
- the strip is out of radio range; or
- something is genuinely wrong.

The daemon holds the link whenever it can and retries on a backoff indefinitely, so
a strip that *has* power should be connected. If it has power and HA says
unavailable, look at `docker logs neewertv` and `bluetoothctl show` on the host.

## Troubleshooting

| Symptom | Usually |
|---|---|
| `no adapter found`, or `org.bluez` permission denied | The `user:` line, or the D-Bus socket isn't mounted. See the two load-bearing lines above. |
| Strip unreachable, other Bluetooth devices fine | The vendor app is still connected. It allows one client. |
| Works, then stops after a while, needs a host reboot | BlueZ has wedged. `bluetoothctl show` will look wrong; restarting `bluetooth` on the host usually clears it, and the daemon reconnects on its own. |
| Home Assistant shows the light as unavailable | See *What `unavailable` actually means*, above. |
| Light blinks off and on once in a while, log alternates `link lost` / `link up` | Something is dropping the link, but establish *what* before blaming the radio: see *Diagnosing a dropped link*, below. |
| Two lights flickering between each other | Two containers sharing a `MQTT_BASE` or a `DEVICE_ID`. |
| Web page shows the light but nothing can change it | By design: the page is read-only, and always was in `observe`. Control lives on MQTT. |

### Diagnosing a dropped link

Three questions, in order. The first is the cheapest and it is the one that catches
a false alarm, because from Home Assistant a **restarted container** and a **dropped
link** are indistinguishable: the light goes unavailable, then comes back.

**1. Did the process restart at all?** A restart begins with the banner again and
its connection attempts count from 1; a dropped link is followed by
`link lost; reconnecting` with a `held_seconds` and attempts that carry on
climbing.

```bash
docker inspect neewertv --format 'started={{.State.StartedAt}} restarts={{.RestartCount}}'
docker logs neewertv | grep -c 'neewertv starting'
```

More than one banner, or a `RestartCount` that climbs between checks, means the
container went down and the radio was never at fault. A clean exit names its
reason in the log; a kill does not, so ask the host:

```bash
journalctl -k --since "2 hours ago" | grep -iE 'oom|killed process'
```

**2. Is the adapter healthy?** `journalctl -k` will never show a disconnect — the
kernel's Bluetooth layer logs those at debug level — but it *does* show the dongle
falling off the USB bus, reloading firmware, or failing a transfer.

```bash
journalctl -k --since "2 hours ago" | grep -i bluetooth
```

Nothing but the boot-time setup lines (`Bluetooth: hci0: CSR: Setting up dongle …`)
means the adapter never misbehaved, which rules out the USB path and leaves the
radio link. Anything newer there — an `urb … failed to resubmit`, a second
"Setting up dongle" — is the fault, not the strip.

**3. What did the controller say?** Only now is the reason code worth reading.
Leave this running while the strip is idle and let a drop happen:

```bash
sudo timeout 180 btmon | grep -A4 -i 'Disconnect Complete'
```

| Reason | Meaning | Likely fix |
|---|---|---|
| `0x08` supervision timeout | The air was not there: range, obstruction, 2.4 GHz congestion | Move the dongle clear of the chassis and any USB 3 hub; a short extension lead does more than an antenna |
| `0x13` remote user terminated | The strip hung up | Another client, usually the vendor app, or the strip's own idle timer |
| `0x16` local host terminated | BlueZ closed the link | The daemon dropped the handle: look for a `link lost` immediately after a command |
| `0x3e` connection failed to establish | The attempt never completed | The aborts while retrying — often the strip still believes the old link is alive |

`held_seconds` on each `link lost` line is the other half of the picture: a
consistent ~41 s used to mean an idle timeout — `BLE_KEEPALIVE_SECONDS` exists for
that — but the same strip has since been seen dropping at 344 s and at 40 s with
the keep-alive firing throughout, so a duration is a hint and not a diagnosis.
What does discriminate: a link that comes back in about two seconds was never a
range or power problem, and an interval that matches a `RestartCount` increment was
never a link problem at all.

An adapter that never reports `rssi` on `link up` (`rssi=None` every time) is worth
suspicion in its own right: a healthy controller answers with a reading, and the
common CSR clones (`manufacturer=10` in the kernel's setup line) are known to
connect and then drop. If the reason code points at the air but the strip sits in
sight of the host, try a different dongle before moving furniture.

## Manual checklist

These need the physical strip, so they aren't automated and never will be. They're
the things worth checking before a release:

- [ ] Cold start with the strip powered off, then powered on.
- [ ] Unplug the strip's power and restore it: the daemon reconnects by itself.
- [ ] While it is unplugged, watch the log: attempts should be numbered 1, 2, 3
      and so on. A count that jumps (1, 2, 4) means each failure is being
      recorded twice, which makes the backoff escalate at twice its rate.
- [ ] Switch the strip off at the wall mid-effect, then back on.
- [ ] Hold the connection with the vendor app, then close it: the daemon recovers.
- [ ] Per-segment colour across all 20 zones.
- [ ] Fire an alert while the strip is off: it lights up, then goes back off.

## Running it without Docker

Possible, not supported. You need:

- Rust 1.85 or newer;
- `libdbus-1-dev` and `pkg-config` to build;
- `libdbus-1-3` and a running `bluetoothd` at runtime;
- the same D-Bus access the container gets.

```bash
cargo build --release
MQTT_HOST=… ./target/release/neewertv
```

If it fails at the bus with a permission error, that's the `org.bluez` policy again
— the container solves it with `user: "0"`, and you'll need the `bluetooth.conf`
policy block instead.

## Contributing

See [CONTRIBUTING.md](../CONTRIBUTING.md). Tests must not need the strip — anything
that does belongs on the manual checklist above.
