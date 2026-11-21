# MixerCraft — context for AI coding sessions

Cross-platform Voicemeeter alternative in Rust. Read this first, then
`docs/architecture/overview.md` and `docs/roadmap.md`.

## Priorities (in order)
1. Audio stability and correctness 2. Reliable routing 3. Low resource use,
predictable latency 4. Fast startup, automatic recovery 5. Cross-platform
6. Maintainable idiomatic Rust 7. Professional minimal UI 8. Testing,
distribution, updates 9. Extensibility.

## Layout
* `crates/mixercraft-core` — real-time engine, no OS code; dependencies
  `rtrb` and `mixercraft-denoise`. Mixer, resampler, drift controller,
  FIFOs, processing cycle, strip FX/noise suppression, bus keep-awake.
* `crates/mixercraft-denoise` — vendored RNNoise port (nnnoiseless 0.5.2,
  BSD-3) with allocation-free FFTs; keep it close to upstream.
* `crates/mixercraft-config` — TOML schema v5, migrations, atomic saves.
* `crates/mixercraft-ipc` — NDJSON protocol v1, local socket transport.
* `crates/mixercraft-platform` — capability model + cpal device I/O +
  virtual devices (`virtual_dev`, ids `virtual:<name>`; Linux: PipeWire
  nodes in `pipewire_dev`, ADR 0009).
* `crates/mixercraft-usbip` — user-space USB/IP server emulating USB audio
  devices (Windows virtual devices via usbip-win2, ADR 0007).
* `crates/mixercraft-winaudio` — `unsafe` (COM): restores Windows' default
  devices.
* `crates/mixercraft-hotkey` — `unsafe` (Win32): key combinations, global
  hotkey listener used by the engine, key recording (ADR 0011).
* `crates/mixercraft-engine` — `mixercraftd`: supervisor, recovery, clocks.
* `crates/mixercraft-cli` — `mixercraft` client.
* `crates/mixercraft-gui` — desktop mixer (Slint, software renderer). UI in
  `ui/*.slint` (colours in `ui/theme.slint`), IPC thread in `src/engine.rs`.
* `crates/mixercraft-update` — update feed check, download, SHA-256
  verification, MSI hand-off (ADR 0006); used by the GUI and CLI.
* `installer/windows/` + `scripts/build-installer.ps1` — per-user WiX 5 MSI
  (`docs/development/releasing.md`).
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
See `docs/adr/`. The owner wants zero recurring cost: Windows virtual endpoints follow the
free path in ADR 0007: emulated USB audio devices over USB/IP
(`mixercraft-usbip`, accepted and implemented); never ask users for test signing.
License MIT OR Apache-2.0 is accepted (ADR 0008). GUI: Slint with the software renderer (ADR 0005); faders need numeric `accessible-value`.

## Known risks
Windows virtual devices depend on the third-party usbip-win2 driver
(~95 ms app to app; no own signed driver); startup-sound routing feasibility (needs a
service plus a timing test); cpal's 10 ms WASAPI periods; PipeWire graph-node
backend complexity. Details: `docs/research/feasibility.md` §8.
