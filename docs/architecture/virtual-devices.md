# Virtual device strategy

Background and evidence: [feasibility §3](../research/feasibility.md#3-virtual-endpoints-highest-risk-area).

## Common model

A **virtual cable** is a pair of endpoints that other applications see as
ordinary devices:

* a *virtual input* — a playback device apps send audio to; the engine
  captures it and feeds a strip;
* a *virtual output* — a recording device apps record from; a bus feeds it.

The engine treats each cable like any other device stream: its own clock
domain, a drift-compensated FIFO, and the same recovery state machine. The
capability model reports `VirtualEndpoints` per platform; the UI hides the
feature where it is not available instead of showing inert controls.

## Windows (implemented: emulated USB audio devices over USB/IP)

Chosen because it costs nothing and needs no kernel code of our own
(ADR 0007).

* `mixercraft-usbip` is a user-space USB/IP server (127.0.0.1:3240) that
  emulates one full-speed USB Audio Class 1 device per virtual endpoint
  (48 kHz, 16-bit stereo, 1 ms isochronous packets). A strip with source
  `virtual:<name>` gets a *speaker* device "MixerCraft <name>" that
  applications play into; a bus with sink `virtual:<name>` gets a
  *microphone* device applications record from.
* The Microsoft-signed usbip-win2 client driver, installed by the user,
  attaches each device (`usbip attach -r 127.0.0.1 -b 1-N`, run by the
  engine without a console window); Windows' inbox `usbaudio.sys` creates the
  endpoint. Closing the stream unplugs the device.
* The server is the devices' clock: host transfers complete in real time,
  and an idle clock keeps audio moving at 48 kHz while Windows is not
  streaming, so the engine never sees a stalled device. Handover is
  direction-aware (speakers: never leave a hole; microphones: never read
  twice) and immediate on an explicit stop (alternate setting 0).
* The engine treats virtual devices like any other device: drift-compensated
  FIFO, recovery state machine (a device Windows drops is reported as lost and
  reopened). A virtual device never clocks the engine (its server threads are
  not real-time threads); a virtual master bus uses the software clock.
* Windows makes a newly connected USB audio device the default device. The
  attach worker snapshots the defaults first and restores them for 15 s
  afterwards (`mixercraft-winaudio`, the only crate with `unsafe` code: COM and
  the undocumented `IPolicyConfig`). Measured: Windows switched the default
  1.2 s after connecting and MixerCraft switched it back 0.3 s later (before
  the restore check was tightened from 250 to 50 ms).
* MixerCraft's own endpoints are hidden from the device list, so nothing can
  be routed back into itself.
* Identity and names: the USB serial number is derived from the name in the
  id (`virtual:Input 2`; recording devices use a separate serial), so Windows
  sees the same device after every engine start and keeps its endpoint id,
  app assignments and default-device choice (endpoint ids verified stable
  over restarts). Windows keeps the product name it first saw; what users
  see follows the strip or bus name instead, through the endpoint's own name
  ("Browser (MixerCraft Input 2)"), set like a rename in the Sound settings
  (`mixercraft-winaudio::set_description`, applied on open, on rename and
  every 2 s). The `IPolicyConfig` call takes ownership of the string; passing
  a Rust-owned buffer corrupted the heap (found on hardware, fixed).
* The devices exist only while the engine runs; configuration (strips,
  buses, ids, routing) persists in `config.toml` as usual.
* The default-device guard only undoes a switch to an endpoint that did not
  exist before the attach; switches to known endpoints are the user's own
  default returning and are left alone.
* Limitations: about 95 ms from an application through MixerCraft to another
  application (USB transfers are queued in 10 ms chunks on both sides); the
  pid.codes *test* product id is used until the project has its own; the
  usbip-win2 dependency (0.9.x) is a third-party kernel driver.

A native WaveRT driver (lower latency) remains possible later if signing is
arranged (OSSign or an EV certificate).

## Linux

* Runtime-created `support.null-audio-sink` adapter nodes with
  `object.linger = true` and recognisable `node.name`s
  (`mixercraft.virtual.<id>`), created through the PipeWire core API from the
  engine's native PipeWire integration.
* Ownership is tracked by node name so the engine can reconcile on start
  (create missing nodes, keep existing ones, remove nodes no longer
  configured) without touching anything it did not create.
* Uninstall removes the nodes; no configuration files of the user or the
  distribution are edited.

## macOS (planned)

Proposal and plan: [ADR 0010](../adr/0010-macos-virtual-devices.md).

* AudioServerPlugIn bundle (`MixerCraft.driver`) in
  `/Library/Audio/Plug-Ins/HAL`, based on Apple's NullAudio sample, written
  in C with a minimal surface: device/stream properties and a lock-free
  **loopback** ring per device between its output and input sides. The
  engine uses the devices through Core Audio like physical ones (capture a
  strip's device, play into a bus's device), so there is no IPC with the
  sandboxed `coreaudiod`.
* Devices are added and removed through a custom property on the plug-in
  object and persisted with the host's storage API.
* Installed by an unsigned `.pkg` (post-install: restart `coreaudiod`), the
  plug-in ad-hoc signed — to be confirmed in a CI spike first. Removal
  deletes the bundle and restarts `coreaudiod`.
* Per-application capture via Core Audio process taps on macOS 14.2+
  (separate feature).

## Status

Windows: implemented (emulated USB devices via usbip-win2), verified on
hardware 2026-11-16. Linux and macOS: planned.
