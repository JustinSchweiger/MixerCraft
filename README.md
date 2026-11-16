# MixerCraft

A cross-platform virtual audio mixer written in Rust — a modern, lightweight
alternative to VB-Audio Voicemeeter for Windows 11, Linux (PipeWire) and
macOS.

> **Status: early development (v0.1, pre-release).** The audio engine,
> routing, drift-compensated multi-device I/O, recovery, configuration, IPC,
> autostart and a first desktop mixer window work and have been exercised on
> Windows hardware, including per-strip gate/compressor/EQ and, on Windows,
> virtual devices (emulated USB audio devices; requires the free
> [usbip-win2](https://github.com/vadimgrn/usbip-win2) driver). Windows has
> a per-user MSI installer with in-app updates
> ([building and releasing](docs/development/releasing.md)). See the
> [roadmap](docs/roadmap.md) and the [feature matrix](docs/feature-matrix.md)
> for exactly what works where.

## Why

* **Fast startup** – a small headless engine (`mixercraftd`); audio was
  running ~210 ms after launch on the development machine.
* **Stays clean for days** – every non-master device runs through a
  drift-compensated, band-limited resampler, so independent hardware clocks
  can never slowly drain or overflow a buffer. NaN protection, adaptive
  buffering, a stall watchdog and per-device recovery handle the rest, and
  every event is counted and visible.
* **Engine independent of the UI** – clients talk to the engine over a
  versioned IPC protocol; closing or crashing a client never touches audio.

## Try it (developers)

Requirements: Rust ≥ 1.95. On Linux also `libasound2-dev` (and optionally
`libpipewire-0.3-dev libclang-dev` for the native PipeWire host).

```bash
cargo build --release
```

```bash
./target/release/mixercraftd --list-devices
```

```bash
./target/release/mixercraftd --print-default-config
```

Run the engine. On first start it writes a default configuration with the
default input on a strip (metered, not routed) and a bus on the default
output; route the input when you want to hear it, keeping in mind that a
microphone routed to speakers feeds back:

```bash
./target/release/mixercraftd
```

Open the mixer window (it connects to the running engine, or offers to start
it). On Windows and macOS it lives in the system tray: closing the window
hides it, and `--tray` starts it hidden:

```bash
./target/release/mixercraft-gui
```

Or control the engine from a terminal:

```bash
./target/release/mixercraft status
```

```bash
./target/release/mixercraft gain 0 -6
```

See `mixercraft --help` for routing, mute, solo and meters.

Start the engine automatically when you sign in (Task Scheduler on Windows,
systemd user unit on Linux, LaunchAgent on macOS):

```bash
./target/release/mixercraft autostart enable --now
```

Release builds of `mixercraftd` on Windows run without a console window;
engine logs are written to `%LOCALAPPDATA%\MixerCraft\data\logs` (Windows),
`~/.local/share/mixercraft/logs` (Linux) or
`~/Library/Application Support/MixerCraft/logs` (macOS).

## Documentation

* [Architecture overview](docs/architecture/overview.md)
* [Technical feasibility report](docs/research/feasibility.md)
* [Virtual device strategy](docs/architecture/virtual-devices.md)
* [Architecture Decision Records](docs/adr/README.md)
* [Building and platform setup](docs/development/building.md)
* [Contributing](CONTRIBUTING.md)

## License

Dual-licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE) at your
option (see [ADR 0008](docs/adr/0008-license.md)).
