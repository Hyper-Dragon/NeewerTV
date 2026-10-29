# Summary

What changed, and why. A reviewer should be able to understand the problem
without reading the diff.

Fixes #(issue)

## Type of change

- [ ] Bug fix
- [ ] New feature
- [ ] Breaking change to a topic, payload or config key
- [ ] Documentation
- [ ] Build, CI or dependency

## How it was tested

Be specific. "Ran it locally" is not a test result.

- [ ] `cargo test`
- [ ] `cargo clippy --all-targets -- -D warnings`
- [ ] `docker build -t neewertv:local .`
- [ ] Ran it against the real strip (say what you did and what the strip did)

**Hardware used**, if any: model (`PD20250145` / `PD20250103`), host OS, BlueZ
version, adapter.

## Checklist

- [ ] Tests cover the change, or it is a change that cannot be tested without a
      device — and it is on the README's manual checklist
- [ ] No new warnings from `clippy -D warnings`
- [ ] `protocol/` is still pure: no async, no I/O, no logging
- [ ] If observable behaviour changed (a topic, a payload field, a default), the
      changelog entry is written and the README table is updated
- [ ] If a byte layout is stated in a comment, the comment states it in full
      rather than pointing at a document the reader may not have
