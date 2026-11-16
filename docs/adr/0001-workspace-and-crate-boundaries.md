# 0001 — Workspace and crate boundaries

**Status:** Accepted (2026-11-16)

## Context
The initial project brief sketched about 14 crates. Many of those boundaries
(devices vs. platform vs. per-OS crates; separate DSP, routing and core
crates) would be empty or trivially thin today and would force churn across
crate APIs while the design is still settling.

## Decision
Start with six crates split along *real* boundaries — what must be real-time
and platform-free, what is persisted, what crosses a process boundary, what
touches the OS, and the two binaries:

`mixercraft-core`, `mixercraft-config`, `mixercraft-ipc`,
`mixercraft-platform`, `mixercraft-engine` (`mixercraftd`),
`mixercraft-cli` (`mixercraft`).

Per-OS code lives in modules of `mixercraft-platform` behind `cfg`. Native
non-Rust components (Windows driver, macOS HAL plug-in) go under
`platform/<os>/` when they are written. New crates are introduced when a
component has its own consumers or build requirements (GUI, updater).

## Consequences
* `mixercraft-core` has one dependency (`rtrb`) and no OS code, so all DSP
  and routing logic is deterministically testable on any CI runner.
* Splitting further later is cheap; merging prematurely split crates is not.
* `unsafe_code = "deny"` workspace-wide; crates that need FFI opt in
  explicitly and document their invariants.
