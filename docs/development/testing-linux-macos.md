# Testing on Linux and macOS hardware

The Linux and macOS builds are tested in CI and (Linux) in WSL2 against a
real PipeWire without sound hardware. Nothing has run on a Linux desktop or a
Mac with real devices yet. This checklist is for the first runs on real
machines; record each run in the hardware test log of
[roadmap.md](../roadmap.md) (date, distribution/macOS version, devices,
what worked, what did not).

**Safety:** never route a microphone to your speakers or headphones (feedback).
Route microphones to a bus without a device (metering only) or a muted bus.

Get the packages from a release (Release packages workflow) or build them:
`scripts/package-linux.sh` / `scripts/package-macos.sh`
([releasing.md](releasing.md)).

## Linux (Ubuntu 24.04, Debian 12, Fedora 37+ with PipeWire)

1. **Install.** Debian/Ubuntu: `sudo apt install ./mixercraft_<version>_amd64.deb`.
   Elsewhere: unpack the `.tar.gz` and run `./install.sh`.
   Check: "MixerCraft" appears in the application menu with its icon.
2. **Start** it from the menu. Check: the window opens with the MixerCraft
   icon in the dock or taskbar (not a generic one); the engine starts
   (status bar). `mixercraft status` in a terminal shows the backend (ALSA)
   and your devices.
3. **Physical devices.** Choose your microphone on an input and your
   speakers on an output; meters move, faders, mute, solo, EQ and noise
   suppression work; unplug and replug a USB device and watch it recover
   (Diagnostics tab).
4. **Virtual devices.** On an input choose "New virtual playback device",
   on an output "New virtual recording device". Check:
   * `wpctl status` lists "MixerCraft <name>" under Sinks and Sources;
     GNOME/KDE sound settings and `pavucontrol` show them;
   * play music in a browser into the virtual playback device → the input's
     meter moves; route it to your speakers bus → you hear it;
   * a call or recording app (Discord, OBS, `pw-record --target
     mixercraft.source.<name> test.wav`) records the virtual recording device;
   * quit the app (engine stops) → the devices disappear; start again → they
     come back and the browser plays into them again (WirePlumber remembers);
   * `systemctl --user restart pipewire wireplumber` → MixerCraft recovers
     within a few seconds.
   Note the latency you hear or measure and any crackles (Diagnostics: gaps,
   xruns).
5. **Tray** (KDE Plasma; GNOME needs the AppIndicator extension, which
   Ubuntu has): the icon appears; left click opens the window; the menu mutes
   an input, stops/starts the engine and quits. Closing the window hides it
   to the tray (Settings → Close button). On plain GNOME without the
   extension there is no tray and closing quits — expected.
6. **Sign-in start** (Settings tab): turn on "Open MixerCraft when you sign
   in" (creates `~/.config/autostart/mixercraft.desktop`), sign out and in →
   the app starts hidden in the tray with the engine. Then try "Start only
   the audio engine" instead (`systemctl --user status mixercraft`). Turn both
   off again.
7. **Updates** (About tab): "Check now" reports the latest version; with an
   older build installed it shows the new version and "Release notes" opens
   the release page.
8. **Uninstall:** turn off sign-in start, then `sudo apt remove mixercraft`
   (or `./install.sh --uninstall`). Configuration stays in
   `~/.config/MixerCraft`.

## macOS (11 or newer, Apple silicon or Intel)

1. **Install.** Open the `.dmg`, drag MixerCraft into Applications, start it.
   macOS blocks it the first time: System Settings → Privacy & Security →
   "MixerCraft was blocked…" → **Open Anyway** (then confirm). Note exactly
   what macOS showed (for the docs).
2. **Microphone permission.** Choose your microphone on an input. macOS
   should ask whether MixerCraft may use the microphone (the engine, started
   by the app). After allowing, the meter moves. If it stays silent without a
   prompt, check System Settings → Privacy & Security → Microphone and note
   which entry appears ("MixerCraft" or "mixercraftd").
3. **Physical devices**: as on Linux (step 3).
4. **Menu bar icon:** appears; the menu mutes, stops/starts the engine and
   quits; closing the window hides it.
5. **Sign-in start** (Settings tab): "Open MixerCraft when you sign in"
   writes `~/Library/LaunchAgents/org.mixercraft.app.plist`; macOS may show a
   "Background item added" notice. Sign out and in → the app starts hidden
   in the menu bar. Repeat with "Start only the audio engine" (LaunchAgent
   `org.mixercraft.engine`) and check that the engine can use the microphone
   when started this way (permission entry for the engine).
6. **Updates:** as on Linux (step 7). After installing a newer `.dmg`, note
   whether macOS asked for microphone access again.
7. Virtual devices do not exist on macOS yet (ADR 0010); the Settings tab
   should say so.
