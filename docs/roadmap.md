# Roadmap and status

Last updated: 2026-11-16. "Verified" means exercised on real hardware.

| Phase | Scope | Status |
|---|---|---|
| 1 | Research and technical design | ✅ [feasibility report](research/feasibility.md), [feature matrix](feature-matrix.md), [ADRs](adr/README.md) |
| 2 | Repository foundation: workspace, CI, linting, docs | ✅ |
| 3 | Shared audio engine: mixer, routing, DSP, resampling, drift control, FIFOs | ✅ unit, simulation and allocation tests |
| 4 | Native backends: physical I/O | 🟡 cpal-based: **verified on Windows** (WASAPI, 3 devices with independent clocks); Linux/macOS compile and test in CI only |
| 4b | Native WASAPI (IAudioClient3, MMCSS), PipeWire graph node | 🗓 |
| 5 | Virtual devices | 🗓 blocked on Windows by driver signing ([ADR 0007](adr/0007-windows-driver-signing.md)); Linux and macOS can start |
| 6 | Background engine and recovery | 🟡 done: IPC, single instance, recovery state machine, watchdog, software clock fallback, diagnostics, config persistence. Missing: service/autostart integration, device-arrival notifications, sleep/resume tests |
| 7 | Production GUI | 🗓 framework spike first ([ADR 0005](adr/0005-gui-framework.md)) |
| 8 | Packaging and release pipeline | 🗓 ([ADR 0006](adr/0006-distribution-and-updates.md)) |
| 9 | Automatic updates | 🗓 |
| 10 | Hardening: long soak tests, self-hosted hardware runners | 🗓 |

## Next steps, in order

1. Linux and macOS hardware verification of the current engine.
2. Engine autostart per platform (Windows: per-user first, service mode
   with startup-sound timing test; Linux: systemd user unit; macOS:
   LaunchAgent).
3. Linux virtual devices (PipeWire null-audio-sink nodes) — the first
   platform where virtual endpoints need no signing.
4. GUI framework spike, then the mixer view.
5. Native WASAPI backend for low latency.
6. macOS HAL plug-in; Windows driver once signing is arranged.

## Hardware test log

| Date | Platform | Devices | Result |
|---|---|---|---|
| 2026-11-16 | Windows 11 Pro 26300, x86_64 | master: onboard HDA S/PDIF; input: Scarlett Solo USB; output: Voicemeeter VAIO virtual input | Startup to audio 208–234 ms. Gain, mute, solo, routing and meters verified over IPC. Config persisted atomically. 7-minute release run: 0 errors; 1 input and 2 output FIFO underruns while the machine was compiling on all cores, after which adaptive targets settled at 30/45 ms; drift corrections within ±5 ppm. |
