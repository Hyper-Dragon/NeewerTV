# Contributing

Thanks for looking. This is a small daemon with one job: take a command from MQTT
or HTTP, put it on a Bluetooth link, and report what happened. Keep it that
narrow.

## Before you start

Read [README.md](README.md) for what the thing does, and
[SECURITY.md](SECURITY.md) for the deployment assumptions and the deliberate
non-features. If you are planning anything that touches the wire protocol, read
`docs/PROTOCOL.md` first — that document is the contract the code implements, and
it is descriptive: where the two disagree, the code is wrong.

## Building and testing

Rust is pinned in `rust-toolchain.toml`; `rustup` will fetch the right version
automatically the first time you build.

```bash
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo test
```

Then as the container sees it:

```bash
docker build -t neewertv:local .
docker compose -f docker-compose.example.yml up -d
docker logs -f neewertv
```

**Tests must not need the strip.** Anything that requires real hardware is
`#[ignore]`d and listed in the README's manual checklist, so `cargo test` on a
machine with no Bluetooth radio stays green. If you add a test that only passes
with a device plugged in, CI will fail and so will everyone else's morning.

Please run all three commands above before opening a pull request.

## Tests come first

**Write the test before the code.** Not as a formality — it is the only thing
that makes the byte layouts safe to touch.

In practice this matters most in `protocol/`, which is pure and therefore
completely testable without a strip:

1. Write a test that states the exact expected bytes, copied from the observed
   wire behaviour — `assert_eq!(encode(&cmd), [0x7A, 0x0A, 0x01, 0x01, 0x86])`.
2. Run it. It must fail, and fail for the right reason (not compiling counts,
   but check the message is the assertion you wrote).
3. Write the smallest code that makes it pass.
4. Only then refactor.

A test written after the code tends to assert whatever the code happens to do,
which is worthless: it locks in a bug and calls it a specification. A test
written first is the only place the intended wire format is actually written
down, and when a later change breaks it, the diff says which byte moved.

**Where a bug is found on real hardware, the regression test is written to the
captured frame** before the fix — that frame is the only evidence of what the
device really does, and it is worth more than any amount of reasoning about what
it should do.

Tests that need the strip are the exception. They go on the README's manual
checklist, not in CI.

## What belongs here

- Talking to the strip: framing, checksums, command construction, the state
  report walk.
- Holding one Bluetooth link, and recovering it when it drops.
- MQTT in, state out, Home Assistant discovery.
- The alert lifecycle — save what the strip was doing, do something louder, put
  it back.
- Filtering and renaming effects, custom scene definitions from the config file.

What does not:

- **A Home Assistant add-on, a HACS integration, or a Supervisor wrapper.** The
  container is standalone and speaks MQTT; anything else is a fork.
- **A music visualiser.** Interesting, and out of scope — a visualiser wants a
  per-frame audio stream, and this daemon is deliberately a command bridge.
- **Cloud anything.** No account, no relay, no vendor API.
- **Firmware flashing.** Never. The upgrade commands are documented only so it is
  clear they must not be sent.
- **Support for other NEEWER hardware.** Other models share the framing and
  nothing else. A pull request adding a second device family will be asked to
  become a second driver with its own module, not a pile of `if` statements.

## Style

- `cargo fmt`, and clippy clean with `-D warnings`. There is no style debate
  here; the tool decides.
- `protocol/` is **pure**: no async, no I/O, no logging. It turns intent into
  bytes and bytes into state. That is what makes it testable without a strip.
- Comments explain *why*, and where a byte layout is stated, state it in full —
  offset, width, range, and the field it maps to. A comment that says "see the
  spec" is worthless to someone who does not have it.
- Errors are typed in `protocol` (`thiserror`) and contextual in the binary
  (`anyhow`, with `.context()`). Never swallow an error into a log line and carry
  on as if the command landed.
- Fail loudly at startup on a bad config; never degrade silently at runtime.

## Commit messages and pull requests

One logical change per pull request. Say what broke and what now happens instead
— a reviewer should not need the diff to understand the problem. If a change
alters observable behaviour (a topic, a payload field, a default), say so
explicitly; that is what the changelog entry gets written from.

By contributing you agree your work is licensed under the [MIT licence](LICENSE).
