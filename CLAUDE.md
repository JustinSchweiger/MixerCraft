# MixerCraft — context for AI coding sessions

Cross-platform Voicemeeter alternative in Rust. Read this first, then
`docs/architecture/overview.md` and `docs/roadmap.md`.

## Priorities (in order)
1. Audio stability and correctness 2. Reliable routing 3. Low resource use,
predictable latency 4. Fast startup, automatic recovery 5. Cross-platform
6. Maintainable idiomatic Rust 7. Professional minimal UI 8. Testing,
distribution, updates 9. Extensibility.

## Layout
* `crates/mixercraft-core` — real-time engine, no OS code, one dependency
  (`rtrb`). Mixer, resampler, drift controller, FIFOs, processing cycle.
* `crates/mixercraft-config` — TOML schema v1, migrations, atomic saves.
* `crates/mixercraft-ipc` — NDJSON protocol v1, local socket transport.
* `crates/mixercraft-platform` — capability model + cpal device I/O.
* `crates/mixercraft-engine` — `mixercraftd`: supervisor, recovery, clocks.
* `crates/mixercraft-cli` — `mixercraft` client.
* `docs/` — research, architecture, ADRs, feature matrix, roadmap.

## Commands
`cargo fmt --all` · `cargo clippy --workspace --all-targets` (CI: `-D
warnings`) · `cargo test --workspace` · `cargo test -p mixercraft-core
--release` · `cargo deny check`.

## Non-negotiable rules
* Nothing on the audio path allocates, frees, blocks, logs or does I/O.
  Extend `crates/mixercraft-core/tests/realtime_safety.rs` when adding
  real-time code.
* Control → audio thread only through `engine::Command`; freed objects come
  back as `engine::Garbage`.
* Drivers of `EngineCore` only `try_lock`; the supervisor never locks it.
* Every gain change is ramped (`dsp::SmoothedGain`).
* `unsafe_code` is denied workspace-wide. FFI crates must opt in and document
  invariants.
* Never fake functionality: unsupported features are reported through
  `mixercraft-platform::capabilities` and `docs/feature-matrix.md`.
* Never claim hardware verification that was not done; record hardware runs
  in `docs/roadmap.md`.
* IPC changes within protocol v1 must be additive (`#[serde(default)]`).
  Config changes need a schema bump plus a migration and test.
* When testing on real hardware, never route a microphone to speakers;
  use a metering-only bus.

## Decisions and open questions
See `docs/adr/`. Open owner decisions: Windows driver signing (ADR 0007),
license confirmation (ADR 0008). GUI framework pending a spike (ADR 0005).

## Known risks
Windows kernel-driver signing; startup-sound routing feasibility (needs a
service plus a timing test); cpal's 10 ms WASAPI periods; PipeWire graph-node
backend complexity. Details: `docs/research/feasibility.md` §8.
