# NEEWER NS03 Series TV Backlight — BLE Protocol Reference

> ## Read this first
>
> **This document was deduced from real hardware.** It was not supplied by the
> vendor and no vendor documentation was used: the GATT profile, framing and
> command set were recovered by capturing the traffic of the vendor's Android
> application and by replaying packets against a strip with an independent BLE
> client.
>
> Everything in it is therefore **best-effort, and some of it is wrong.**
>
> * **Inference is marked, and inference is not measurement.** Commands marked
>   *Derived* were reconstructed from the client's logic and agree with observed
>   traffic, but were never independently round-tripped. Commands marked
>   *Unverified* were seen on the wire with no established meaning.
> * **A firmware update may invalidate all of it.** Identifiers, payload layouts,
>   segment numbering and effect indices belong to the firmware and can change
>   without notice. Nothing here is recorded against a firmware version.
> * **It describes one unit of each model.** Behaviour may differ by batch,
>   region or board revision.
> * **Sending an identifier the device does not know is not safe.** There is no
>   error channel worth the name. See §13 before experimenting.
> * **Not affiliated with, endorsed by, or supported by NEEWER.** Names taken from
>   the vendor application appear only so that identifiers can be recognised; no
>   vendor code, binary, asset or decompiled output is included.
>
> If you build against this: verify the parts you depend on, and treat silence
> from the device as a reason to stop rather than a reason to guess.

* **Version:** 1.1
* **Date:** 2026-10-29
* **Applies to:** `PD20250145` (NS03, 5 m), `PD20250103` (NS03, 3 m)
* **Transport:** Bluetooth Low Energy, GATT
* **Licence:** MIT

---

## Contents

| | |
|---|---|
| [1](#1-introduction) | Introduction, terminology, confidence markers, provenance |
| [2](#2-transport) | GATT profile, discovery, connection model |
| [3](#3-packet-framing) | Short and long framing, checksum |
| [4](#4-numeric-conventions) | Units, ranges, HSI colour |
| [5](#5-data-identifier-registry) | Every data identifier, used and unused |
| [6](#6-command-definitions) | Byte layout of each command |
| [7](#7-effect-identifier-registry) | Presets, scenes, DIY patterns |
| [8](#8-mode-registry) | Operating modes and music modes |
| [9](#9-state-report--0x09) | Reading state back from the device |
| [10](#10-segments) | Segment numbering and colour mapping |
| [11](#11-device-limits) | Hard limits |
| [12](#12-implementation-guidance) | What makes an implementation behave |
| [13](#13-security-considerations) | What this link does and does not protect |
| [14](#14-summary-of-constants) | Constants at a glance |

---

## 1. Introduction

### 1.1 Purpose and scope

This document describes the binary protocol used by the NEEWER NS03 family of TV
backlights over Bluetooth Low Energy: the GATT profile, the packet framing, the
command identifiers these models use, the byte layout of each payload, and the
registries that give meaning to the identifier values.

That is enough to build a working controller — discovery, power and brightness,
colour, effect and scene selection, timers and per-segment colour — without any
other material. It is not a vendor specification and it is not complete; §1.3
marks how much of it has actually been confirmed.

The Rust daemon in this repository is one implementation of it, and deliberately a
partial one: power, brightness, colour, segments, effects, state queries and
alerts. Where this document describes something the daemon does not send, the
daemon's silence is not evidence that the command does not work.

### 1.2 Terminology

| Term | Meaning |
|---|---|
| **Command** | A host-to-device packet, identified by a *data identifier*. |
| **Notification** | A device-to-host packet, identified by a *data identifier*. |
| **data identifier** | The single byte at frame offset 1 that selects the command or notification type. |
| **effect identifier** | The value carried in `model` (byte 7 of a `0x0F` payload) that selects a preset, scene, or DIY pattern. |
| **segment** | One of the 20 addressable zones of the strip, numbered 0–19. |
| **mode** | The device's current operating engine, reported as the first byte of a state response. |
| **vendor application** | The phone application supplied with the device. Its traffic is the primary evidence for everything here. |

### 1.3 Confidence markers

Every command and registry entry carries a status, because a protocol recovered
from traffic is not uniformly trustworthy:

| Status | Meaning |
|---|---|
| **Verified** | Confirmed by round-trip exchange with the device. |
| **Derived** | Reconstructed from the vendor application's construction logic; consistent with observed traffic, but never independently round-tripped. |
| **Unverified** | Observed on the wire, semantics not established. |

If a value you depend on is marked *Derived* or *Unverified*, confirm it against
your own device before building on it.

### 1.4 Provenance, accuracy, and licence

This document describes the observable wire behaviour of a single unit of each
listed model. It was produced independently, without vendor documentation, by
capturing the traffic of the vendor application and replaying packets against the
device with an independent BLE client. It is **not published, endorsed, supported,
or reviewed by NEEWER**.

Three consequences follow, and they are not formalities:

* **It is best-effort.** A protocol recovered from traffic describes what one
  device did, not what every device must do. A command that worked here may be
  ignored elsewhere, and a value that means one thing on this unit may mean
  another on a later revision.
* **Firmware can invalidate it.** Identifiers, payload layouts, segment numbering
  and effect indices belong to the firmware. An update may add, move or redefine
  any of them, and nothing in this document records a firmware version to compare
  against.
* **Undefined input is not safe.** The device has no error channel worth the name.
  An identifier it does not recognise may be silently ignored, or may leave it in
  a mode it cannot be talked out of. §13 lists what not to send.

Two further points about the vendor's own material:

* **Scene, preset and pattern names** in §7 are the vendor application's UI
  labels, reproduced verbatim so that identifiers can be recognised and mapped.
  They record *the name the vendor uses for a given byte*, not any assertion of
  rights over them. An implementation is free to display different names.
* **No vendor code, binary, asset or decompiled output forms part of this
  document.** Only observable wire behaviour is described.

This document and the accompanying implementation are both [MIT](../LICENSE).
Nothing here is a statement about the device's fitness for any purpose, and no
warranty of any kind is given. Transmitting undefined identifiers, or any of the
firmware-upgrade identifiers in §5, can render a device inoperable (§13).

---

## 2. Transport

### 2.1 GATT profile

| Role | UUID |
|---|---|
| Primary service | `69400001-b5a3-f393-e0a9-e50e24dcca99` |
| Write characteristic (host → device) | `69400002-b5a3-f393-e0a9-e50e24dcca99` |
| Notify characteristic (device → host) | `69400003-b5a3-f393-e0a9-e50e24dcca99` |

Command packets are written to the write characteristic. State responses and
acknowledgements arrive as notifications on the notify characteristic, so a host
that does not subscribe to it will issue commands and hear nothing back.

### 2.2 Discovery and addressing

The advertised local name is `NH-PD20250145` for the 5 m model and
`NH-PD20250103` for the 3 m model — the name is derived from the **model**, not
the unit, so two strips of the same model advertise identically.

**Status: Verified.** Measured on real hardware:

* BlueZ reports the device address as **`(public)`** — a burned-in address rather
  than a resolvable random one;
* the address was **unchanged across a power cycle**.

**Scope of that measurement.** The power cycle was a few seconds long. A device
left unpowered for hours — overnight, or after a long power cut — has **not** been
tested, and a long outage is exactly the condition under which a pseudo-random
address, if there is one anywhere in the boot path, would be most likely to be
re-seeded differently. The public-address classification and the unchanged value
both argue that it is burned in and will not move, but that is inference from two
data points, not a guarantee. Treat "the address is stable" as *probably true*,
and make any implementation behave sensibly if it turns out to be false.

The address is therefore a **stable per-unit identifier** at least on the unit
measured, which makes it usable to tell two strips apart. A host can bind by
address.

Do not assume every unit behaves this way: some firmware builds in this family may
advertise a random address, and this has been observed on one strip only. The
robust rule is: **prefer the address when it is known and present, otherwise
discover by name, and never assume a name identifies a unit.**

Remaining caveat, measured: a **connected device stops advertising.** A scan that
does not see the strip may mean it is out of range, already connected to another
client, or already connected to you — not only that it is absent.

### 2.3 Connection model

The device accepts **exactly one** connection at a time. A second client cannot
connect while a first is active; connection attempts while the device is engaged
fail at the link layer.

Consequently:

* A host that issues repeated commands should hold a single connection open.
  Re-establishing per command works, but costs a full discovery-and-connect cycle
  each time.
* Disconnect before another client is expected to use the device, or that client
  cannot connect at all.

---

## 3. Packet framing

All packets begin with the header byte `0x7A` and end with a checksum byte.

### 3.1 Short form

```
 +--------+------------+--------+------------------+-----------+
 |  0x7A  | dataIdent  | length | data (length B)  | checksum  |
 +--------+------------+--------+------------------+-----------+
     0          1         2          3 .. 2+len        3+len
```

`length` is the number of data bytes that follow, as an unsigned 8-bit integer.

### 3.2 Long form

```
 +--------+------------+-----------+-----------+------------------+-----------+
 |  0x7A  | dataIdent  | lengthHi  | lengthLo  | data (length B)  | checksum  |
 +--------+------------+-----------+-----------+------------------+-----------+
     0          1           2           3          4 .. 3+len       4+len
```

`length` is the 16-bit **big-endian** number of data bytes that follow.

### 3.3 Checksum

The checksum is the low-order 8 bits of the unsigned sum of every preceding byte in
the frame, including the header and the length field(s):

```
checksum = ( sum(frame[0 .. n-1]) ) & 0xFF
```

Discard any frame whose checksum does not match.

### 3.4 Worked example

Power on:

```
7A 0A 01 01 86
│  │  │  │  └─ checksum = (0x7A + 0x0A + 0x01 + 0x01) & 0xFF = 0x86
│  │  │  └──── data[0] = 0x01 (on)
│  │  └─────── length  = 1
│  └────────── data identifier 0x0A (power)
└───────────── header
```

### 3.5 Which commands use which form

The framing form is a property of the data identifier, not of the packet contents.

| Form | Data identifiers used by these models |
|---|---|
| Short | `0x08` `0x0A` `0x0B` `0x0C` `0x0E` `0x10` `0x16` `0x40` `0x41` `0x42` `0x43` |
| Long | `0x09` `0x0D` `0x0F` `0x11` `0x15` |

The form is fixed per identifier. Sending one in the wrong form produces a frame
the device ignores.

---

## 4. Numeric conventions

| Quantity | Range | Encoding |
|---|---|---|
| Brightness | 0–1000 | Two bytes, `(value / 10, value % 10)`. Nominally 0–100 %, represented as 0–1000 units. |
| Hue | 0–360 | 16-bit big-endian. |
| Saturation | 0–100 | Unsigned 8-bit. |
| Lightness | 0–100 | Unsigned 8-bit. `0` renders the segment dark. |
| Speed, sensitivity, expand | 0–100 | Unsigned 8-bit. |
| Duration, counters | — | 32-bit big-endian where stated, otherwise 16-bit big-endian. |

All multi-byte integers are **big-endian** unless explicitly stated otherwise.

Colour is transmitted in **HSI**, not RGB. A host converting from RGB maps to a
hue in degrees, a saturation in 0–100, and a lightness in 0–100.

Ranges are what the device accepts, not what it validates. It does not reject an
out-of-range value; it does something undefined with it, which is worse.

---

## 5. Data identifier registry

Identifiers listed as *not used* belong to other NEEWER device families that share
this framing. Do not send them to an NS03 series device.

| Identifier | Direction | Name | Form | Status |
|---|---|---|---|---|
| `0x08` | host → device | Query device state | Short | Verified |
| `0x09` | device → host | State report | Long | Verified |
| `0x0A` | host → device | Power | Short | Verified |
| `0x0B` | host → device | Brightness | Short | Verified |
| `0x0C` | host → device | Colour temperature | Short | Derived |
| `0x0D` | host → device | Colour (HSI) | Long | Verified |
| `0x0E` | host → device | Music reactive | Short | Verified |
| `0x0F` | host → device | Display engine (presets, scenes, DIY) | Long | Verified |
| `0x10` | device → host | Write acknowledgement | Short | Verified |
| `0x11` | host → device | Mixed / multi-panel — not used | Long | — |
| `0x12` | host → device | Generic scene effect — not used | Long | — |
| `0x15` | host → device | Capability request | Long | Unverified |
| `0x16` | device → host | Capability response | Short | Unverified |
| `0x17` | host → device | Firmware upgrade start — not used | Long | — |
| `0x1C` | host → device | Time synchronisation — not used | Short | — |
| `0x1D`, `0x1F` | host → device | Generic timers (other models) — not used | Short | — |
| `0x21` | host → device | Firmware upgrade block — not used | Long | — |
| `0x23`, `0x25`, `0x26`, `0x28`, `0x2B`, `0x2D`, `0x2F`, `0x30`, `0x32`, `0x36`, `0x3A`, `0x3C`, `0x3F` | host → device | Other NEEWER device families — not used | — | — |
| `0x40` | host → device | Set sleep timer | Short | Derived |
| `0x41` | device → host | Timer state report | Short | Derived |
| `0x42` | host → device | Cancel sleep timer | Short | Derived |
| `0x43` | host → device | Query sleep timer | Short | Derived |
| `0xF0` | host → device | Factory test — not used | Short | — |
| `0xF3` | host → device | Matter indicator — not used | Short | — |

`0x1B` is **not assigned**. The vendor application cannot emit it and the device does
not respond to it.

---

## 6. Command definitions

### 6.1 Power — `0x0A`

Short form. One data byte.

```
data[0] = 0x01  power on
          0x02  power off
```

Example — power on: `7A 0A 01 01 86`.

A refused or unsupported value is not acknowledged.

### 6.2 Brightness — `0x0B`

Short form. Three data bytes.

```
data[0] = 0x00                 (reserved, always 0)
data[1] = brightness / 10
data[2] = brightness % 10      brightness in 0..1000
```

Example — 80 %: brightness = 800 →

```
7A 0B 03 00 50 00 D8
```

### 6.3 Colour temperature — `0x0C`

Short form. Six data bytes. **The NS03 series is RGB-only**: the command is defined
and accepted but has no visible effect, and the vendor application exposes no
temperature control for these models.

```
data[0] = brightness / 10
data[1] = brightness % 10
data[2] = temperature         0..100
data[3] = 0x00
data[4] = 0x01
data[5] = 0x00
```

### 6.4 Colour — `0x0D`

Long form. Two mutually exclusive payload shapes, selected by `lightColorType` at
`data[2]`.

#### 6.4.1 Single-colour form — `lightColorType = 0x01`

```
data[0]  brightness / 10
data[1]  brightness % 10
data[2]  0x01                        lightColorType
data[3]  0x64                        constant 100
data[4]  hue >> 8
data[5]  hue & 0xFF
data[6]  saturation                  0..100
data[7]  0x00
data[8]  0x01
data[9]  panelIndex                  0 for these models
```

Length is always 10. Example — hue 240, saturation 100, brightness 80 %:

```
7A 0D 00 0A 50 00 01 64 00 F0 64 00 01 00 9B
```

#### 6.4.2 Segmented form — `lightColorType = 0x02`

Assigns one colour per segment. See §10 for segment numbering.

```
data[0]      brightness / 10
data[1]      brightness % 10
data[2]      0x02                    lightColorType
data[3]      count >> 8              number of colour entries
data[4]      count & 0xFF
data[5..]    count × colour record, each 4 bytes:
                 [0] lightness 0..100
                 [1] hue >> 8
                 [2] hue & 0xFF
                 [3] saturation 0..100
data[n-3]    0x00
data[n-2]    0x01
data[n-1]    panelIndex              0 for these models
```

Total data length is `8 + 4 × count`.

* `count` is in 1..20.
* `count == 20` maps one entry to one segment, in order.
* `count < 20` distributes the entries across the full strip; the device
  interpolates between adjacent entries.

Example — two entries (hue 0, hue 240), brightness 100 %:

```
7A 0D 00 10 64 00 02 00 02 64 00 00 64 64 00 F0 64 00 01 00 80
```

#### 6.4.3 Reserved form — `lightColorType = 0x04`

The vendor application defines a gradient form carrying a 16-bit segment count and a
gradient mode byte. **These models ignore it**: no change results for any
combination of segment count (2–8 tested) or mode (0–2 tested). Do not use `0x04`
on this family.

#### 6.4.4 panelIndex

`panelIndex` is the final byte of both forms. Its effect on these models is
**unverified**; it is not required for per-segment addressing, which is achieved by
entry position. Send `0x00`.

### 6.5 Music reactive — `0x0E`

Short form. Eleven data bytes.

```
data[0]   0x01                constant
data[1]   isOpen             1 = music mode active, 0 = inactive
data[2]   brightness / 10
data[3]   modeId             see §8.2
data[4]   0x64               constant 100
data[5]   sensitivity        0..100
data[6..10]  0x00 0x01 0x00 0x00 0x00
```

Example — mode 0, brightness 60 %, sensitivity 50:

```
7A 0E 0B 01 00 3C 00 64 32 00 01 00 00 00 67
```

### 6.6 Display engine — `0x0F`

Long form. Variable length. This single command drives the effect presets, the
scene catalogue, and the custom (DIY) patterns.

```
data[0]      brightness / 10
data[1]      brightness % 10
data[2]      0x01
data[3]      0x01
data[4]      0x00
data[5]      0x00
data[6]      0x00
data[7]      model            effect identifier, see §7
data[8]      speed            0..100
data[9]      style            0x00 = Full Spectrum, 0x01 = Segment
data[10]     direction        0x00 or 0x01
data[11]     expand           1..3; 4 or 5 for DIY Unfold; 6 or 7 for DIY Bounce;
                              0x32 (50) for the two-way presets
data[12]     count            number of colour records
data[13..]   count × colour record, 4 bytes each:
                 [0] lightness 0..100
                 [1] hue >> 8
                 [2] hue & 0xFF
                 [3] saturation 0..100
```

Total data length is `13 + 4 × count`. All built-in effects use `count = 1` with
the record `64 00 00 3F` (lightness 100, hue 0, saturation 63).

Example — Superfluid preset (`model = 0xFC`), brightness 60 %, speed 50, full
spectrum, direction 0:

```
7A 0F 00 11 3C 00 01 01 00 00 00 FC 32 00 00 32 01 64 00 00 3F DC
```

#### 6.6.1 Field semantics

* **style** selects how the colour palette is applied to the animation. It is a
  rendering option and does **not** address segments; per-segment colour is carried
  by `0x0D` (§6.4.2). On these models changing `style` alone produces no observable
  difference for the patterns tested.
* **direction** inverts the animation's direction. On the two-way presets it is the
  mirroring flag.
* **expand** widens the spread of the animation. Its usable range depends on the
  pattern (§7.3).

#### 6.6.2 Device mode after a write

A `0x0F` write places the device in a mode determined by the effect identifier:

| Effect identifier | Resulting mode |
|---|---|
| `0x31`–`0x48` (scene) | mode 3 — Scene |
| `1`–`16` (DIY pattern) | mode 5 — DIY |
| `0xF1`, `0xF2`, `0xF8`–`0xFF` (preset) | mode 9 — TV Screen |

Sending `0x08` and reading the mode back is the only way to confirm that a write
took effect.

### 6.7 Query device state — `0x08`

Short form. One data byte.

```
data[0] = 0x00   report every mode's parameters
data[0] = 0x01   report current-mode parameters
```

Both values are accepted. Prefer `0x00`: the resulting report contains segments
for several modes (see §9), whereas `0x01` returns only the active one. **Status:
Derived** — the distinction follows from the vendor application's two query
variants and is consistent with observed reports, but the exact difference in
segment count has not been isolated.

The device replies with `0x09` (§9). Example — request all: `7A 08 01 00 83`.

### 6.8 Write acknowledgement — `0x10`

Short form, device to host. Two data bytes, observed as `01 00`.

The device returns `0x10` after every accepted `0x0F` write. If no acknowledgement
arrives, treat the write as not applied.

### 6.9 Sleep timer — `0x40`–`0x43`

Device-side countdown. Setting a timer needs no further host involvement; the
device acts on expiry independently of the connection.

| Identifier | Direction | Data | Meaning |
|---|---|---|---|
| `0x40` | host → device | `[seconds:4 BE][lightOpen:1]` | Start or overwrite the countdown |
| `0x41` | device → host | `[duration:4 BE][totalTime:4 BE][lightOpen:1]` | Timer state report |
| `0x42` | host → device | `[lightOpen:1]` | Cancel the countdown |
| `0x43` | host → device | `[reportState:1]` | Request a `0x41` report |

* `seconds` is a countdown in **seconds**.
* `duration` counts down to zero; zero means the timer has expired.
* `totalTime` is the originally requested duration.
* `lightOpen` selects the action on expiry: `0` turns the light off, `1` turns it
  on.

Example — one-hour countdown (3600 s = `00 00 0E 10`), light off on expiry:

```
7A 40 05 00 00 0E 10 00 DD
```

---

## 7. Effect identifier registry

`model` (data byte 7 of `0x0F`) draws from a single identifier space shared by
presets, scenes, and DIY patterns. Values not listed are not defined.

### 7.1 Display presets

Status: Verified.

| `model` | Name |
|---|---|
| `0xF8` | Ice Grains |
| `0xF9` | Rice Snow |
| `0xFA` | Northern Lights |
| `0xFB` | Starstream |
| `0xFC` | Superfluid |
| `0xFD` | Meteor |
| `0xFE` | Liquid Crystal |
| `0xFF` | Southern Lights |

`0xF0`–`0xF7` are not defined for these models.

### 7.2 Two-way (screen sync)

| `model` | `direction` | Name |
|---|---|---|
| `0xF1` | `0x00` | TV Screen |
| `0xF1` | `0x01` | TV Screen, mirrored |
| `0xF2` | `0x00` | Two-Way Off |

`direction` is ignored when `model = 0xF2`; the device reports it back as `0x00`.

The vendor's names here describe the entries badly. `Two-Way Off` names a *state*
— and it is the base preset, with the two-way split disabled — while `TV Screen`
is the two-way one. Two-way is a modifier and mirroring is a modifier on top of
it, so mirroring never appears alone: it only decides which side gets which
colour.

The implementation in this repository therefore publishes:

| `model` | `direction` | Published name | Behaviour |
|---|---|---|---|
| `0xF2` | — | `TV Screen` | The sensor drives the colour |
| `0xF1` | `0x00` | `TV Screen (two-way)` | The sensor drives each side separately |
| `0xF1` | `0x01` | `TV Screen (two-way, mirrored)` | The same, with the sides swapped |

Wire values are untouched; this changes only the label Home Assistant shows.

### 7.3 Scenes

Status: Verified. Twenty-four scenes in four categories of six, occupying
`0x31`–`0x48` with no gaps and no other values defined. Scenes are sent as `0x0F`
with the value below as `model` and `expand = 0x01`.

| Category | `model` | Name |
|---|---|---|
| Natural | `0x31` | Icelandic Blue |
| Natural | `0x32` | Glacier Express |
| Natural | `0x33` | Cloud Sea |
| Natural | `0x34` | Snow Cabin |
| Natural | `0x35` | Sea Fire |
| Natural | `0x36` | Firefly Night |
| Life | `0x37` | Game |
| Life | `0x38` | Holiday |
| Life | `0x39` | Working |
| Life | `0x3A` | Party |
| Life | `0x3B` | Fashion |
| Life | `0x3C` | Sport |
| Festive | `0x3D` | Christmas |
| Festive | `0x3E` | Valentine's Day |
| Festive | `0x3F` | Halloween |
| Festive | `0x40` | Thanksgiving |
| Festive | `0x41` | Mother's Day |
| Festive | `0x42` | Father's Day |
| Emotional | `0x43` | Summer Song |
| Emotional | `0x44` | Sea Dream |
| Emotional | `0x45` | Love & Dream |
| Emotional | `0x46` | Spring Fishing |
| Emotional | `0x47` | Neon World |
| Emotional | `0x48` | Dream Mirror |

### 7.4 DIY patterns

Status: Verified. Sixteen customisable animations. Each is rendered over the colour
records supplied in the same `0x0F` payload.

| `model` | Name | `direction` adjustable | `expand` range |
|---|---|---|---|
| `0x01` | Gradient | no | 1–3 |
| `0x02` | Jump | no | 1–3 |
| `0x03` | Breath | no | 1–3 |
| `0x04` | Blink | no | 1–3 |
| `0x05` | Meteor | yes | 1–3 |
| `0x06` | Stack | yes | 1–3 |
| `0x07` | Drift | yes | 1–3 |
| `0x08` | Chase | yes | 1–3 |
| `0x09` | Sway | yes | 1–3 |
| `0x0A` | Flow | no | 1–3 |
| `0x0B` | Rainbow | yes | 1–3 |
| `0x0C` | Flash | no | 1–3 |
| `0x0D` | Bounce | no | 6–7 |
| `0x0E` | Shuttle | no | 1–3 |
| `0x0F` | Flicker | no | 1–3 |
| `0x10` | Unfold | no | 4–5 |

The visual effect of `direction` and `expand` is not documented here. `direction =
0x00` and `expand = 0x01` are the safe defaults unless you have a specific reason
to vary them.

---

## 8. Mode registry

### 8.1 Operating modes

Reported as `data[0]` of a `0x09` state report.

| Value | Mode | Set by |
|---|---|---|
| `0` | Switch (power) | `0x0A` |
| `1` | Lighting | — |
| `2` | Colour | `0x0D` |
| `3` | Scene | `0x0F` with a scene identifier |
| `4` | Music | `0x0E` |
| `5` | DIY | `0x0F` with a DIY pattern identifier |
| `6` | Mixed | `0x11` |
| `7` | *(not used)* | — |
| `8` | DIY v2 | — |
| `9` | TV Screen | `0x0F` with a preset identifier |
| `10` | Two-dimensional scene | — |
| `11` | Two-dimensional music | — |
| `13` | TV | — |
| `14` | Music variant | — |
| `15` | Display engine | — |

### 8.2 Music mode identifiers

`modeId`, data byte 3 of `0x0E`.

| Value | Name |
|---|---|
| `0` | Energy |
| `1` | Breathing |
| `2` | Beat |
| `3` | Meteor |
| `4` | Starry Sky |
| `5` | Neon |

---

## 9. State report — `0x09`

Long form, device to host, sent in response to `0x08`.

### 9.1 Structure

```
data[0]      mode (§8.1)
data[1..]    zero or more segments, until the data is exhausted
```

Each segment:

```
length:2 BE    total byte count of (type + payload), so payload length = length - 1
type:1         mode identifier (§8.1) this segment describes
payload        length - 1 bytes
```

Walk segments until the remaining data is shorter than the smallest possible
segment (3 bytes), and ignore segment types you do not understand.

### 9.2 Segment payloads

Each payload is a sequence of items. The item grammar varies by segment type; the
following are established:

| Type | Payload length | Layout | Status |
|---|---|---|---|
| `0` Switch | 2 | `[on:1][pad:1]` — `on == 0x01` means powered | Verified |
| `1` Lighting | 3n | `[value:2][channel:1]` repeating | Unverified |
| `2` Colour | variable | `[hi:1][lo:1][subtype:1]` then subtype-specific data | Verified (subtype 1) |
| `3` Scene | 4 | `[pad:1][sceneId:1][b:1][pad:1]` | Verified |
| `4` Music | 10 | `[isOpen][pad][brightness][modeId][0x64][sensitivity][…]` | Unverified |
| `5` DIY | 2 | `[brightness][pad]` | Verified |
| `9` (unnamed) | 9 | Not established | Observed |

**This table is known to be incomplete.** A single `0x08` query against an NS03
returned six segments of types `0`, `9`, `2`, `3`, `4` and `5` — including type
`9`, which the vendor application never constructs. Type `9` presumably
corresponds to mode 9 (TV Screen, §8.1), but its payload was not decoded.

The consequence is a hard rule rather than a footnote: **walk the segment list by
length and skip types you do not recognise.** Assuming a fixed set of segments, a
fixed count, or a fixed order will break on a firmware build that reports one
segment more than this document knows about.

Observed report from a strip in mode 2 (Colour), 58 bytes:

```
7A 09 00 35 02
   00 03 00 | 01 00                    type 0  switch on
   00 0A 09 | 64 00 01 00 01 00 32 00 00   type 9  (undecoded)
   00 08 02 | 2F 00 01 19 00 77 64         type 2  colour
   00 05 03 | 00 31 64 00                  type 3  scene 0x31 (Icelandic Blue)
   00 0B 04 | 01 00 64 00 32 32 01 00 40 64   type 4  music
   00 03 05 | 64 00                        type 5  DIY brightness 100
   1D                                     checksum
```

For type `2` with `subtype = 2`, the item continues with a 16-bit colour count
followed by that many 4-byte colour records in the same
`[lightness, hueHi, hueLo, saturation]` layout used by the commands.

The type `2` segment above decodes as:

```
2F 00        brightness pair, as the write path sends it — 47 (0x2F tens)
01           subtype 1: single colour
19 00 77 64  [lightness 25][hue 0x0077 = 119°][saturation 100]
```

So the strip **does** report its colour. That matters more than it looks: an
alert is a colour written over whatever was showing, and a host that has not
recorded the previous colour cannot put it back — the light simply stays in the
alert's colour afterwards. Read this segment before overwriting the strip and the
restore is exact. The subtype-2 form reports one record per segment and is not
what a single colour restore wants; do not decode it as one.

### 9.3 Reading current state

Locate the segment whose type equals `data[0]` and read that mode's parameters from
it, rather than relying on fixed byte offsets: the number and order of segments
varies with what the device chooses to report.

Known reported quantities:

| Quantity | Source |
|---|---|
| Power | type `0` segment, first payload byte |
| Current mode | `data[0]` |
| Brightness | mode-specific; see the type `5` payload for DIY |
| Scene or effect selected | type `3` segment, second payload byte |
| Colour showing | type `2` segment, subtype 1 — see §9.2 |

### 9.4 Echo behaviour

The device does **not** echo effect parameters. A `0x0F` write is followed by a
`0x09` report that identifies the mode and the selected scene, but not the speed,
style, direction, expand value, or colour records that were sent. A host has to
retain those values locally.

---

## 10. Segments

Status: Verified.

The strip comprises **20 addressable segments**. Segment colour is assigned with
the segmented form of `0x0D` (§6.4.2).

### 10.1 Numbering

* Segment **0** is located at the centre of the top edge of the display, beginning
  just left of centre.
* Numbering increases **leftward** from segment 0.
* Numbering continues around the perimeter; segment **19** terminates just **right**
  of centre at the top edge.

The two ends of the strip therefore meet at the top centre, which is the routing
produced by a single continuous strip installed around a display.

A host mapping screen regions to segments should traverse the perimeter from the
top centre in a single consistent direction.

### 10.2 Entry-to-segment mapping

| Entries supplied | Result |
|---|---|
| 20 | One entry per segment, in order |
| 1–19 | Entries distributed across the strip with interpolation between them |

---

## 11. Device limits

| Limit | Value |
|---|---|
| Simultaneous connections | 1 |
| Segments | 20 |
| Colour records per `0x0D` payload | 20 |
| Colour records per `0x0F` payload | 20 |
| Layers (DIY) | 1 |
| Colour temperature support | none (RGB-only) |

---

## 12. Implementation guidance

The difference between a controller that works and one that is merely
protocol-correct:

* Do not keep a connection open while another client needs the device; there is
  only one, and it is first-come.
* Subscribe to the notify characteristic before issuing commands, or the `0x09`
  and `0x10` traffic you are waiting for goes past unseen.
* Treat a `0x0F` write as applied only once `0x10` arrives.
* Coalesce rapid parameter changes into a single write. The link is single
  connection and per-write latency is not trivial; a slider drag sent verbatim
  finishes long after the finger stopped.
* Expect the link to drop after a power-off command, and treat the commanded state
  as authoritative until the device is reachable again.
* Ask for the state report (`0x08`) on **every** connection, not once per process.
  The device never volunteers it, and a strip that was power-cycled or driven by
  another client while you were disconnected will otherwise be restored to a state
  it is no longer in.
* Do not transmit `0x17` or `0x21` (firmware upgrade) without a recovery plan; see
  §13.

### 12.1 What the link actually does

Measured against a real strip on a USB CSR adapter, and worth writing down because
none of it is in the advertising data:

| Behaviour | Measured |
|---|---|
| Reconnect after a drop | 1.9–2.1 s, first attempt, no rescan |
| Link lifetime with a keep-alive every 20 s | 645 s, then 344 s, then 40 s — same session, same adapter |
| Time to become connectable after an ungraceful detach | ~11 s, preceded by `le-connection-abort-by-local` |
| RSSI reported on connect | never, on this adapter (`None` every time) |

Two consequences for a controller:

* **A drop that is followed by a two-second reconnect is not a range or power
  problem.** The device answered immediately; it was neither out of range nor off.
  Nor is it a timer, because the same link survives ten minutes and then dies
  after forty seconds. Reach for the disconnect reason code rather than a theory —
  the kernel records it in `hci_disconn_complete` with no sniffer running, and
  `0x08` (supervision timeout), `0x13` (peer closed) and `0x16` (host closed) each
  point at a different fix.
* **A write that never completes is worse than one that fails.** Every command is
  an ATT Write Request with a response, so a device that has stopped acknowledging
  leaves the write pending with no protocol-level deadline. A controller that
  awaits it will stop supervising the link altogether, which presents as a daemon
  that works and then quietly stops. Put a deadline on the write.

---

## 13. Security considerations

* The BLE link is **unauthenticated and unencrypted**. Any device within radio range
  can connect and issue any command in this specification, including power and
  brightness. Physical proximity is the only control.
* There is no pairing, no bonding, and no application-layer credential.
* The device address is **stable and public** (§2.2). Any nearby device can track
  this strip, and can address it directly rather than having to find it by scan.
  There is still no access control: knowing the address grants nothing beyond what
  radio range already granted.
* Because a single connection is permitted, an unauthorised client can also deny
  service by holding the connection.
* Firmware upgrade commands are unprotected once connected. A failed or malformed
  image is expected to render the device inoperable with no documented recovery
  path.

---

## 14. Summary of constants

```
HEADER          0x7A
SERVICE         69400001-b5a3-f393-e0a9-e50e24dcca99
WRITE_CHAR      69400002-b5a3-f393-e0a9-e50e24dcca99
NOTIFY_CHAR     69400003-b5a3-f393-e0a9-e50e24dcca99
SEGMENTS        20
BRIGHTNESS_MAX  1000
HUE_MAX         360
SAT_MAX         100
LIGHTNESS_MAX   100
SPEED_MAX       100
```

---

*End of document.*
