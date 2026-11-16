# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project uses
[Semantic Versioning](https://semver.org/).

## [Unreleased]

### Changed
- Minimum supported Rust version is now 1.95 (required by Slint).

### Fixed
- A second engine start now reports "already running" instead of a Windows
  "access denied" error.
- FIFO latency now recovers after stalls instead of staying raised forever;
  latency raises and recoveries appear in the engine's recovery log.
- Device streams get a 1 s startup grace: early shortfalls re-prime without
  counting as underruns or raising latency, and startup buffer sizing no
  longer causes a seconds-long pitch correction.

### Added
- Desktop mixer window `mixercraft-gui` (Slint): input strips and output
  buses with pill faders, LED meters with peak hold, routing, mute and solo
  buttons; diagnostics tab; starts the engine when it isn't running;
  reconnects automatically.
- Tray icon (Windows, macOS) with per-input mute, engine start/stop and
  quit; closing the window hides it to the tray; a second launch shows the
  running window; `--tray` starts hidden.
- Repeated identical recovery events are merged into one entry with a count.
- `mixercraft autostart enable|disable|status`: Task Scheduler logon task
  (Windows), systemd user unit (Linux), LaunchAgent (macOS).
- Engine logs to a daily rotating file (7 days) in the per-user data
  directory; Windows release builds of `mixercraftd` open no console window.
- `mixercraft local-devices` and `mixercraft default-config`.
- Real-time audio core: planar buffers, click-free gain smoothing, lock-free
  peak/RMS meters, channel mapping, strip/bus mixer with mute, solo and
  routing matrix, NaN containment.
- Band-limited variable-ratio resampler and PI clock-drift controller;
  drift-compensated lock-free FIFOs with adaptive, measured-block-size
  targets and warm-up handling.
- Engine processing cycle driven by a master device or a software clock, with
  lock-free command and garbage queues; allocation-free hot path verified by
  test.
- Versioned TOML configuration with validation, migration framework and
  atomic saves with backup.
- Versioned NDJSON IPC protocol over per-user local sockets/named pipes with
  handshake and size limits.
- Platform capability model; device discovery and capture/playback via cpal
  (WASAPI, Core Audio, ALSA, optional native PipeWire).
- `mixercraftd` engine daemon: single-instance guard, device recovery state
  machine with backoff, stall watchdog, software clock fallback, recovery
  log, diagnostics, debounced config persistence.
- `mixercraft` CLI: status, meters, devices, gain, mute, solo, routing,
  capabilities, shutdown.
- CI on Linux (x86_64, ARM64), Windows, macOS, cross-checks for Windows ARM64
  and macOS x86_64, MSRV check, cargo-deny supply-chain checks, Dependabot.
- Research report, architecture docs, feature matrix, ADRs 0001–0008.
