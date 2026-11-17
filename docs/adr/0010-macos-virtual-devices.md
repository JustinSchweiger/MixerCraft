# 0010 — macOS virtual devices: loopback Audio Server Plug-in

**Status:** Proposed (2026-11-17). Not implemented; the capability model
reports virtual endpoints on macOS as planned.

## Context
On Windows virtual devices are emulated USB devices (ADR 0007), on Linux
PipeWire nodes (ADR 0009). macOS has no user-space API that lets an
application publish a playback or recording device to other applications.
The options:

| Option | Gives other apps a device? | Cost / constraints |
|---|---|---|
| **Audio Server Plug-in** (HAL plug-in bundle in `/Library/Audio/Plug-Ins/HAL`, loaded by `coreaudiod`) | yes: output and input devices | C/C++ against the CoreAudio plug-in interface; admin rights to install; `coreaudiod` restart; must be code-signed (see Signing) |
| AudioDriverKit (DriverKit extension) | yes | needs Apple-granted DriverKit entitlements, i.e. a paid account and Apple's approval; meant for hardware |
| Core Audio process taps (macOS 14.2+) | no — captures other apps' output, cannot act as a microphone | no driver; "System Audio Recording" permission |
| Aggregate / multi-output devices | no | only combine existing devices |

BlackHole and Loopback-style products use the HAL plug-in route. BlackHole
is GPL-3.0, so its code must not be copied (ADR 0008); Apple's *NullAudio*
sample (`Creating an Audio Server Driver Plug-in`, Apple Sample Code
License: redistribution with the notice) is the starting point.

## Decision (proposed)
Build `MixerCraft.driver`, a minimal **loopback** Audio Server Plug-in:

* **Loopback per device, no IPC.** Every MixerCraft device has an output
  stream and an input stream joined by a lock-free ring inside the plug-in
  (the BlackHole model). For a strip's `virtual:<name>`, applications play
  into "MixerCraft <name>" and the engine *captures* the same device through
  Core Audio (cpal) like a physical input; for a bus, the engine *plays* into
  the device and applications record from it. The engine needs no new audio
  path: the device is just another cpal device with its own clock, bridged by
  the existing drift-compensated FIFOs. The plug-in runs in `coreaudiod`'s
  sandbox, so avoiding sockets or shared memory with the engine avoids the
  sandbox's IPC restrictions.
* **Devices created at run time through Core Audio properties.** The plug-in
  object exposes a custom property (`kAudioObjectPropertyCustomPropertyInfoList`)
  holding the list of device names; the engine writes it with
  `AudioObjectSetPropertyData`, the plug-in adds/removes devices and stores
  the list with the host's `WriteToStorage` so the devices exist from the
  next boot without the engine. Device UIDs derive from the names (stable app
  assignments and defaults, like the USB serials on Windows).
* **Format:** 48 kHz (44.1 kHz selectable), stereo, `Float32`; the plug-in
  is its own clock (host ticks from `mach_absolute_time`), never the
  engine's master (as on Windows and Linux).
* **Code:** C, in `drivers/macos/MixerCraftDriver/` (outside the Cargo
  workspace), built with `clang` by `scripts/package-macos.sh`; real-time
  rules as in the engine (no allocation, locks or logging in the I/O
  callbacks). The Rust side goes into `mixercraft-platform`
  (`macos_virtual`): set the custom property (an FFI crate with documented
  `unsafe`, like `mixercraft-winaudio`, or the `coreaudio-sys` bindings cpal
  already uses), map `virtual:<name>` selectors to the device UIDs, hide the
  devices from the engine's own device list.
* **Installation:** a `.pkg` (`pkgbuild`/`productbuild`, unsigned) next to the
  `.dmg`, installing the bundle to `/Library/Audio/Plug-Ins/HAL` and
  restarting `coreaudiod` (`launchctl kickstart -k system/com.apple.audio.coreaudiod`)
  in its postinstall; the app explains it like the usbip-win2 notice on
  Windows. Uninstall: delete the bundle, restart `coreaudiod`.
* **Signing (to verify first):** Apple silicon runs only signed code;
  `coreaudiod` is expected to load an **ad-hoc signed** plug-in (locally
  built HAL plug-ins load this way), and files installed by Installer.app
  carry no quarantine. The unsigned `.pkg` itself needs "Open Anyway" once,
  like the app. A Developer ID and notarization are not needed for this
  path; if `coreaudiod` refuses ad-hoc code on current macOS, the decision
  must be revisited (owner decides on a paid account).

## Plan
1. **Feasibility spike in CI** (macOS 15 runner, has sudo, no audio
   hardware): build the unmodified NullAudio-style skeleton with one
   loopback device, ad-hoc sign, install, restart `coreaudiod`, check that
   `mixercraftd --list-devices` shows it and that a tone written to its output
   comes back on its input. This answers the signing question without a Mac.
2. Plug-in with the custom device-list property and storage; Rust side;
   capability `VirtualEndpoints` = Partial on macOS when the plug-in is
   installed (Unsupported with an explanation otherwise).
3. `.pkg` in the release workflow; docs; feature matrix.
4. Owner test on a Mac: devices appear in System Settings → Sound, a browser
   plays into "MixerCraft Game", a call app records from "MixerCraft Chat",
   permissions survive an app update, reboot keeps the devices.

## Consequences
* Admin rights once (installing into `/Library`), a `coreaudiod` restart (all
  audio stops for about a second) on install and uninstall.
* C code in the repository with its own review burden; crashes in the
  plug-in take `coreaudiod` (all audio on the Mac) down, so the surface stays
  minimal and fuzz/unit tests cover the ring buffer.
* Per-application capture stays a separate feature (process taps,
  macOS 14.2+) and does not need the plug-in.
