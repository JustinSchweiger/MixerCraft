# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project uses
[Semantic Versioning](https://semver.org/).

## [Unreleased]

### Changed
- Minimum supported Rust version is now 1.95 (required by Slint).
- Configuration schema 2 (per-strip `fx`); schema 1 files are migrated
  automatically.

### Fixed
- The default-device guard only undoes Windows switching to a newly
  appeared virtual device; it no longer overrides a virtual device the user
  chose as default when that device returns after an engine restart.
- Virtual recording devices get their own USB serial numbers (a playback
  and a recording device of the same name no longer collide).
- If the virtual-device server cannot start (USB/IP port in use), the next
  attempt retries instead of the engine remembering the failure.
- `usbip attach` runs with `--once`, so the driver makes no background
  reattach attempts that could race the engine's own recovery.
- A second engine start now reports "already running" instead of a Windows
  "access denied" error.
- FIFO latency now recovers after stalls instead of staying raised forever;
  latency raises and recoveries appear in the engine's recovery log.
- Device streams get a 1 s startup grace: early shortfalls re-prime without
  counting as underruns or raising latency, and startup buffer sizing no
  longer causes a seconds-long pitch correction.

### Added
- Virtual devices on Windows without a driver of our own: a strip whose
  source is a "new virtual playback device" (`virtual:<name>`) creates a
  playback endpoint "MixerCraft <name>" that applications play into; a bus
  with a virtual sink creates a recording endpoint applications record from.
  They are emulated USB audio devices served over USB/IP (`mixercraft-usbip`)
  and attached through the Microsoft-signed usbip-win2 driver, which the user
  installs. Windows' default devices are restored after it switches to a new
  virtual device (`mixercraft-winaudio`). About 95 ms from one application
  through MixerCraft to another.
- Virtual devices are named after their strip or bus in Windows (e.g.
  "Browser (MixerCraft Input 2)") and follow renames within a second; the
  device identity stays the same, so Windows keeps app assignments and
  default-device choices across engine restarts.
- Inputs and outputs can be added, renamed and removed while audio runs
  (GUI "+" cards and right-click menu; `mixercraft add-strip`, `add-bus`,
  `rename-strip`, `rename-bus`, `remove-strip`, `remove-bus`). The engine
  swaps in the resized mixer and renumbers device FIFOs in one audio-thread
  step (`Command::Reshape`, allocation-free). The master bus cannot be
  removed. Gate/compressor/EQ state of other strips restarts on such a
  change.
- Device selection per strip and bus at runtime (GUI dropdowns,
  `mixercraft source` / `mixercraft sink`, IPC `SetStripSource` /
  `SetBusSink`); only the changed device is reopened.
- GUI redesign: frameless window with a custom title bar (drag, double-click
  to maximise, edge resizing), larger strips, device pickers, About tab
  (licence and Slint attribution instead of a footer), mouse-wheel control
  of faders (2 dB per notch) and knobs, engine refusals shown as a banner.
- The engine logs control requests at debug level.
- Per-strip gate, compressor (one "amount" control) and 3-band EQ in the
  engine, before the fader; bit-exact when off; configurable from the GUI
  (rotary knobs), the CLI (`mixercraft fx`) and the configuration file.
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
