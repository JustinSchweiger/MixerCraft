# Feature matrix

Support states: ✅ **Full** · 🟡 **Partial** (limitations noted) ·
🗓 **Planned** · ⛔ **Unsupported** (platform restriction).

"Verified" means exercised on real hardware, not only compiled. CI compiles
and runs the hardware-independent tests on all three platforms.

| Feature | Windows | Linux (PipeWire) | macOS |
|---|---|---|---|
| Shared Rust engine (mixing, routing, DSP, drift control) | ✅ verified | ✅ CI only | ✅ CI only |
| Physical inputs/outputs | ✅ WASAPI shared mode, verified on hardware | 🟡 ALSA (via pipewire-alsa) or native PipeWire host; not yet hardware-verified | 🟡 Core Audio via cpal; not yet hardware-verified |
| Multiple devices with independent clocks | ✅ verified | ✅ same code, CI only | ✅ same code, CI only |
| Sample-rate conversion | ✅ | ✅ | ✅ |
| Follow default device | ✅ (WASAPI reroutes) | 🟡 resolved at stream open | 🟡 resolved at stream open |
| Gain / mute / solo / routing / channel mapping | ✅ | ✅ | ✅ |
| Metering | ✅ | ✅ | ✅ |
| Persistent configuration with migrations | ✅ | ✅ | ✅ |
| Automatic device recovery | ✅ reopen with backoff, stall watchdog | ✅ | ✅ |
| Engine independent of GUI (IPC) | ✅ | ✅ | ✅ |
| Low-latency shared mode (IAudioClient3 / MMCSS) | 🗓 native WASAPI backend | n/a | n/a |
| Virtual endpoints | 🗓 needs signed kernel driver | 🗓 PipeWire null-sink nodes | 🗓 AudioServerPlugIn |
| Per-application capture | 🗓 WASAPI process loopback | 🗓 PipeWire stream links | 🗓 process taps (macOS 14.2+) |
| Per-application output routing | 🗓 undocumented API, feature-flagged | 🗓 `target.object` metadata | 🗓 taps + mute original |
| Autostart with the user session | 🟡 Task Scheduler logon task: registration, settings, start via Task Scheduler and removal verified; start at actual sign-in not yet verified | 🟡 systemd user unit; unit file unit-tested, not run on hardware | 🟡 LaunchAgent; plist unit-tested, not run on hardware |
| Engine without console window | ✅ release builds use the GUI subsystem (verified) | n/a (no console by design) | n/a (no console by design) |
| Engine log files | ✅ daily rotation, 7 days (verified) | ✅ CI only | ✅ CI only |
| Engine before login | 🗓 service; startup-sound routing to be verified | ⛔ PipeWire is per user session | ⛔ firmware chime not routable |
| Desktop GUI (mixer, routing, mute/solo, meters, diagnostics) | 🟡 verified against the engine; tray, strip processing and settings pending | 🟡 builds in CI only | 🟡 builds in CI only |
| Installers | 🗓 MSI | 🗓 .deb / .rpm / AppImage | 🗓 .pkg / .dmg |
| In-app updates | 🗓 | 🗓 (notify-only for distro packages) | 🗓 |

The capability model in `crates/mixercraft-platform/src/capabilities.rs`
reports these states at runtime and must be kept in sync with this table.
