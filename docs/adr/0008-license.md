# 0008 — Project license

**Status:** Accepted (2026-11-16, confirmed by the project owner)

## Context
The license determines which dependencies and samples can be used (BlackHole
is GPL-3.0; SysVAD is MIT; Slint's royalty-free license requires attribution)
and how others may reuse MixerCraft.

## Decision
Dual license **MIT OR Apache-2.0**, the Rust ecosystem default. It permits
every dependency currently in use (enforced by `deny.toml`) and keeps
commercial distribution possible.

## Consequences
* Copyleft code (e.g. BlackHole) must not be copied into the repository;
  it may be studied for architecture only.
* New dependencies must pass `cargo deny check licenses`.
