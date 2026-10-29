---
name: Feature request
about: Suggest something the daemon should do
title: ''
labels: enhancement
assignees: ''
---

**What problem are you trying to solve?**

Describe the situation, not the solution. "The doorbell rings and I want to know
about it from the sofa" is a problem statement. "Add a blink command" is a
solution that may not be the right one.

**What you think would fix it**

Your suggestion, if you have one. Include what you would send over MQTT or press
in the web interface, if that is the shape you have in mind.

**What it would have to touch**

Roughly — a new MQTT topic, a change to a payload, a new config key, a new
command on the wire. A guess is fine; it just helps to know the blast radius.

**Could it be done outside the daemon?**

A lot can: an automation in Home Assistant, a script around `mosquitto_pub`, a
template sensor. Say what you have already tried and why it did not fit — if the
answer is "it works fine", this is probably a documentation issue rather than a
feature.

**Scope check**

These are deliberately out of scope and will be closed, kindly. Please confirm
yours is not one of them:

- [ ] Not an HA add-on / HACS integration wrapper
- [ ] Not music visualisation
- [ ] Not cloud, account or relay anything
- [ ] Not firmware flashing
- [ ] Not support for a different NEEWER model

If it *is* one of those, open it as a discussion or a fork instead — and if you
fork it, that is a success, not a rejection.
