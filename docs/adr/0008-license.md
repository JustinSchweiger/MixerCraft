# 0008 — Project license

**Status:** Proposed — needs confirmation by the project owner

## Context
The license determines which dependencies and samples can be used (BlackHole
is GPL-3.0; SysVAD is MIT; Slint's royalty-free license requires attribution)
and how others may reuse MixerCraft.

## Proposal
Dual license **MIT OR Apache-2.0**, the Rust ecosystem default. It permits
every dependency currently in use (enforced by `deny.toml`) and keeps
commercial distribution possible. Copyleft code (e.g. BlackHole) must not be
copied into the repository.

If the owner prefers a copyleft license (e.g. GPL-3.0), `deny.toml`, the
license files and this record change accordingly.
