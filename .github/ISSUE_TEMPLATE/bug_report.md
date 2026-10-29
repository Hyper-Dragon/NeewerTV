---
name: Bug report
about: Something behaves wrongly, or the strip does not do what you asked
title: ''
labels: bug
assignees: ''
---

**What happened, and what you expected instead**

A clear description of the actual and expected behaviour. "Lights go red instead
of blue" is useful. "Doesn't work" is not — there is nothing to reproduce.

**Which part of it is wrong?**

Tick the one that is actually misbehaving, because they fail in different ways:

- [ ] The strip does nothing (no reaction to any command)
- [ ] The strip does the wrong thing (right command, wrong result on the strip)
- [ ] MQTT state in Home Assistant drifts from reality
- [ ] Home Assistant never discovered the device
- [ ] The daemon loses the connection and does not get it back
- [ ] The web interface
- [ ] The container will not start
- [ ] Something else

**How to reproduce it**

1.
2.
3.

Say whether it happens every time or intermittently. An intermittent bug that
takes an hour to appear is a different bug from one that happens on every write,
even if the symptom looks the same.

**Does it recover on its own?**

- [ ] Yes, after a few seconds
- [ ] Only after `docker restart neewertv`
- [ ] Only after power-cycling the strip
- [ ] Only after rebooting the host (BlueZ wedged)
- [ ] No, it stays broken

That answer alone often identifies the cause.

**Versions**

- Image tag or commit:
- Host OS and kernel: (`uname -a`)
- BlueZ version: (`bluetoothctl --version`)
- Bluetooth adapter: (`hciconfig -a` or `bluetoothctl list`)
- Home Assistant version (if the problem is discovery or state):
- MQTT broker and version:

**Logs**

`docker logs neewertv --tail 200`, with `LOG_LEVEL=debug` if the problem is on
the Bluetooth side.

```
paste logs here
```

Please remove your broker hostname, username and password first. If you have
`LOG_FORMAT=json`, paste it as-is — it is easier to follow.

**Configuration**

The `environment:` block from your compose file, with secrets removed. Include
the two lines that matter most:

```yaml
user: ""
volumes:
  - /run/dbus/...
```

**Anything else**

Screenshots, a packet capture, the vendor app behaving differently, the moment it
started happening. If the daemon used to work and stopped, the change in between
is worth more than the log.
