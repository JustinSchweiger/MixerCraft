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
| Device selection at runtime | ✅ GUI and CLI switching verified on hardware | ✅ same code, CI only | ✅ same code, CI only |
| Add / rename / remove inputs and outputs at runtime | ✅ verified on hardware through the GUI | ✅ same code, CI only | ✅ same code, CI only |
| Strip processing: gate, compressor, 3-band EQ | ✅ engine verified with test signals, GUI knobs verified | ✅ same code, CI only | ✅ same code, CI only |
| Strip noise suppression (RNNoise, 48 kHz engine only) | ✅ verified on hardware through virtual devices (noise floor −30 dB, speech unchanged) | ✅ same code, CI only | ✅ same code, CI only |
| Output keep-awake tone (auto-standby speakers) | 🟡 tone verified on the playback path (15 Hz, −45 dBFS, after 10 s); effect on real speakers depends on their detection threshold, not verified | ✅ same code, CI only | ✅ same code, CI only |
| Metering | ✅ | ✅ | ✅ |
| Persistent configuration with migrations | ✅ | ✅ | ✅ |
| Automatic device recovery | ✅ reopen with backoff, stall watchdog | ✅ | ✅ |
| Engine independent of GUI (IPC) | ✅ | ✅ | ✅ |
| Low-latency shared mode (IAudioClient3 / MMCSS) | 🗓 native WASAPI backend | n/a | n/a |
| Virtual endpoints | 🟡 emulated USB audio devices via usbip-win2 (user installs it); verified on hardware; ~95 ms app → MixerCraft → app; default devices restored | 🗓 PipeWire null-sink nodes | 🗓 AudioServerPlugIn |
| Per-application capture | 🗓 WASAPI process loopback | 🗓 PipeWire stream links | 🗓 process taps (macOS 14.2+) |
| Per-application output routing | 🗓 undocumented API, feature-flagged | 🗓 `target.object` metadata | 🗓 taps + mute original |
| Autostart with the user session | 🟡 Task Scheduler logon task: registration, settings, start via Task Scheduler and removal verified; start at actual sign-in not yet verified. Desktop app in the tray at sign-in (second task, `--tray`, from the Settings tab or the installer): registration and removal verified through the installer; start at actual sign-in not yet verified | 🟡 engine: systemd user unit; unit file unit-tested, not run on hardware. Desktop app in the tray at sign-in: XDG autostart entry (`~/.config/autostart/mixercraft.desktop`, Settings tab or `mixercraft autostart enable --app`); written, validated and removed in WSL2, start at an actual sign-in not verified | 🟡 engine: LaunchAgent; plist unit-tested, not run on hardware. Desktop app in the tray at sign-in: second LaunchAgent `org.mixercraft.app` (written now, loaded at the next sign-in); unit-tested only |
| Engine without console window | ✅ release builds use the GUI subsystem (verified) | n/a (no console by design) | n/a (no console by design) |
| Engine log files | ✅ daily rotation, 7 days (verified) | ✅ CI only | ✅ CI only |
| Engine before login | 🗓 service; startup-sound routing to be verified | ⛔ PipeWire is per user session | ⛔ firmware chime not routable |
| Desktop GUI (mixer, routing, mute/solo, meters, knobs, device pickers, diagnostics, settings, custom title bar) | 🟡 verified against the engine, including the Settings tab (engine rate/latency/clock with restart, close button, peak hold); sign-in toggles show the state but switching them was not exercised | 🟡 builds in CI only | 🟡 builds in CI only |
| Tray icon, close-to-tray, single instance | 🟡 icon registration, hide/show, second-launch activation and `--tray` verified; menu clicks not yet exercised | 🟡 StatusNotifierItem tray (`ksni`, no GTK) for KDE, GNOME with the AppIndicator extension (Ubuntu's default), Xfce and others: registration, tooltip, menu (mute toggled the engine, quit stopped app and engine) verified in WSL2 over D-Bus against a stand-in watcher, not on a desktop; without a tray host (plain GNOME) closing quits as before | 🟡 same code as Windows, untested |
| App icon and version info | ✅ embedded in all three executables; Explorer, taskbar (frameless window) and tray verified | 🟡 `.desktop` file and hicolor PNGs in `assets/linux/`, not installed until packages exist | 🟡 `.icns` in `assets/macos/`, no app bundle yet |
| Installers | 🟡 per-user MSI (WiX 5, unsigned): install, upgrade with running app/engine, uninstall verified; shortcuts, start with Windows (the Settings tab's sign-in task; kept as the user set it across upgrades), PATH, usbip-win2 notice; no engine service, no per-machine variant | 🗓 .deb / .rpm / AppImage | 🗓 .pkg / .dmg |
| In-app updates | 🟡 feed check on start + daily (can be turned off), download, size + SHA-256 check, MSI install with app/engine restart: verified against a local fake feed; v0.1.0 published on GitHub Releases and read by the app; manifests not yet signed (ADR 0006) | 🗓 (notify-only for distro packages; checks run, no installer target) | 🗓 (checks run, no installer target) |

The capability model in `crates/mixercraft-platform/src/capabilities.rs`
reports these states at runtime and must be kept in sync with this table.
