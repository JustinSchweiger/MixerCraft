# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project uses
[Semantic Versioning](https://semver.org/).

## [Unreleased]

### Added
- Windows installer: per-user MSI (no administrator rights) built with
  `scripts/build-installer.ps1` (WiX Toolset 5, free). Start-menu entry;
  optional desktop shortcut, start with Windows (tray app and engine
  autostart task) and `mixercraft` on the user PATH. Explains when usbip-win2
  (virtual devices) is missing, without installing drivers. Upgrades close the
  app and this installation's engine first and keep the configuration;
  uninstall removes the autostart task. See `docs/development/releasing.md`.
- In-app updates: the app checks a release feed 20 s after start and daily
  (can be turned off on the About tab), shows "Update available" in the title
  bar, and on "Download and install" downloads the MSI, verifies its size and
  SHA-256 and runs it; the app and engine restart afterwards. New crate
  `mixercraft-update`; `mixercraft update [--feed URL] [--download]` checks
  from the command line. Releases come from the GitHub Releases of the
  public repository (ADR 0006); the feed URL can be changed in
  `updates.toml`.
- `mixercraft close-app` closes the app and stops the engine of this
  installation (used by the installer); `mixercraft autostart disable
  --if-under <folder>`.

### Changed
- Minimum supported Rust version is now 1.95 (required by Slint).
- Configuration schema 3 (schema 2 added per-strip `fx`; schema 3 adds
  `fx.noise_suppression` and per-bus `keep_awake`); older files are migrated
  automatically.

### Fixed
- Crackling and short hangs during CPU spikes: audio threads now run in the
  operating system's real-time audio class (MMCSS "Audio" on Windows): cpal's
  device callbacks (`realtime` feature), the software clock and the virtual
  devices' USB/IP threads. Virtual devices also move audio per 1 ms packet
  instead of per 10 ms transfer, so the engine's buffers no longer see 10 ms
  jumps or overlapping silence after a late transfer. Under full load on all
  12 cores the app-to-app round trip through two virtual devices stayed at
  86–154 ms (before: up to 420 ms) with at most one engine underrun per run
  (before: up to 13 gaps and 51 xruns).
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
- Noise suppression per input strip (RNNoise, "Denoise" in the app,
  `mixercraft fx <strip> --denoise on`): removes steady background noise and
  much of the noise between words while leaving speech untouched. Needs the
  engine at 48 kHz (otherwise reported as unavailable); adds 20 ms of
  latency; about 1 % of the cycle budget per stereo strip in a release
  build. The RNNoise port is vendored as `mixercraft-denoise` (nnnoiseless
  0.5.2, BSD-3-Clause) with pre-planned FFTs, because upstream allocated on
  every new audio thread.
- "Keep awake" per output (`mixercraft keep-awake <bus> on`): after 10 s of
  silence the bus plays an inaudible 15 Hz tone at -45 dBFS (adjustable) so
  speakers with auto-standby stay on; it fades out within 100 ms when audio
  returns and is not shown on the meter.
- The desktop app starts the engine automatically when it opens and no
  engine is running (once; an engine stopped later, e.g. from the tray, is
  not restarted). `mixercraftd` also reads `MIXERCRAFT_CONFIG` and
  `MIXERCRAFT_LOG_DIR`, which an engine started by the app inherits.
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
