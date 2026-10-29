# Security policy

NeewerTV sits between your Bluetooth light strip and your MQTT broker. The
security surface is small, and most of it belongs to the hardware rather than to
this code — but "small" is not "none", so please read the last section before
exposing it to anything.

## Reporting a vulnerability

Please **do not** open a public issue for a security problem. Use GitHub's
private vulnerability reporting on this repository, or email the address on the
maintainer's profile. Include:

- what an attacker can do that they should not be able to do,
- the smallest reproduction you have,
- the image tag or commit, and your host's BlueZ version (`bluetoothctl --version`).

You will get an acknowledgement, and either a fix or a written explanation of why
it is not one. There is no bounty; there is gratitude.

## What is protected, and how

| Asset | Position |
|---|---|
| MQTT credentials | Read from the environment, held in memory, never logged and never written to disk. Prefer `MQTT_PASSWORD_FILE` over `MQTT_PASSWORD` so it is not visible in `docker inspect`. |
| Web interface | **No authentication by design** — see below. It is a local observability page: a status readout and the daemon's log tail. It has no route that can change the strip, so an open page is a disclosure question, not a control one. |
| Logs | Contain command summaries and connection state. They never contain the MQTT password or the broker URI's userinfo. |
| Persistent storage | The daemon has none. Nothing survives a restart, so there is no store to leak. |

## Deliberate non-features

Please do not file these as vulnerabilities. They are choices, and changing them
would make the daemon worse at its job:

- **The web interface is unauthenticated.** It is meant to be reachable on a
  trusted LAN, behind whatever reverse proxy or network policy you already have.
  If that is not acceptable, set `WEB_INTERFACE=off`. The only thing exposed is
  read-only: the state the daemon believes, and the tail of its own log. Control
  lives entirely on MQTT, which is where it can be authenticated by the broker.
- **MQTT topics are not authenticated per-client.** Authorisation is your
  broker's job. Use broker ACLs to scope the daemon's account to its own topic
  tree if you care about that boundary.
- **There is no pairing, bonding, or handshake with the strip.** Neither is there
  one in the vendor app. See below.

## Deployment expectations

1. **Do not publish the web port to an untrusted network.** Bind it to a LAN
   address, or leave `WEB_INTERFACE=off` in a container with no port mapping.
2. **Give the daemon its own MQTT account**, restricted to `neewertv/#` (or
   whatever `MQTT_BASE` you set) plus the Home Assistant discovery prefix. Do not
   hand it an admin account — discovery writes to a shared prefix.
3. **Keep the D-Bus mount as narrow as possible.** The example compose mounts the
   single `system_bus_socket` rather than `/run/dbus`, so a compromised container
   cannot walk away with every other bus on the host.
4. **Terminate TLS at the broker** if it is off-host. `MQTT_TLS=true` is
   supported; the certificate bundle comes from the image.

## The hardware is the weak point, and it is not ours to fix

This matters more than any of the above, so it gets its own section.

The Bluetooth Low Energy link to this family of strips is **unauthenticated and
unencrypted**. Any device within radio range can connect and issue any command —
power, brightness, colour, scenes. There is no pairing, no bonding, and no
application-layer credential. Physical proximity is the only control.

Two consequences worth understanding:

- **A stranger cannot reach your strip over the internet**, because the strip has
  no network interface. They would have to be in radio range of your living room.
- **An attacker in range can also deny service**, because the device accepts
  exactly one connection at a time and will happily be held open by someone else.

A bug report saying "anyone can control the lights over Bluetooth" will be closed
as a property of the device. A bug report saying **"the daemon keeps retrying a
command forever and holds the link so the vendor app can never connect"** is a
real bug, and we want it.
