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
* Timing under load: Windows' audio engine runs a virtual device on the
  completions of its USB transfers, which travel through usbip-win2. Windows
  keeps only about one 10 ms transfer of reserve (submits arrive ~8.5 ms
  before they are due). In usbip-win2's default "zero-copy" receive mode
  that reserve ran out several times a minute under game load (Windows-side
  stalls of more than 12 ms; MixerCraft's own packet timer stayed within
  1.6 ms and loopback TCP within 9 ms), so applications got late or skipped
  audio requests and crackled. Devices are therefore attached with
  `--receive-mode low-latency` when the installed client offers it
  (`attach --help`); completing transfers earlier did not help.
* Shutdown, sleep and reconnects: Windows must remove a device's old copy
  before it can detect the device again. After "Shut down" with Fast
  Startup (sign-out, then hibernation of Windows itself; `Kernel-Boot` 27
  reports boot type 0x1 at the next start), usbip-win2 starts with an empty
  port table while Windows still has the previous session's devices plugged
  into ports 1–3 of its virtual hub: present, with the previous session's
  arrival time and no removal time. Applications get
  `AUDCLNT_E_DEVICE_INVALIDATED`, and anything attached to those ports is
  ignored, even a device Windows never saw (a test device on port 4, behind
  placeholders on 1–3, worked at once). A normal user cannot remove the
  stale copies (`usbip detach`, `CM_Request_Device_Eject` → problem 47,
  `CM_Reenumerate_DevNode` → access denied); restarting the virtual host
  controller (`pnputil /restart-device /deviceid ROOT\USBIP_WIN2\UDE`) does,
  and the devices come back with their identities. So (ADR 0012):
  1. A device whose old copy stays present for 3 s, or that Windows does
     not detect again after an attach (`LastArrivalDate` older than the
     attach, Configuration Manager via `mixercraft-winaudio::device_state`),
     counts as blocked. The worker then starts the repair helper
     (`crate::repair`: Task Scheduler task "MixerCraft Repair Virtual
     Devices", that `pnputil` command as SYSTEM, startable by the user who
     set it up), at most every 2 minutes. Before that it unplugs its own
     devices and waits for Windows to remove them (an open device makes the
     audio engine veto the restart, which then waits for a Windows restart),
     waits for the task's result, and afterwards retries failed attaches
     quietly for 30 s. Results 3010/50 ("restart pending") stop further
     repairs and are reported.
  2. Without the helper, a blocked device is reported as failed after 20 s
     with a message that says how to set it up; the status
     (`EngineStatus::virtual_repair`) makes the app show a Repair banner.
  3. At sign-out and shutdown the engine (hidden top-level window,
     `mixercraft-winaudio::watch_session_end`) stops connecting devices and
     unplugs them (`Server::unplug_all`, waiting up to 4 s for the removal).
     This alone did not prevent the stale copies (0.3.0, seen on hardware).
  4. Nothing is connected between the suspend notification and 3 s after
     resuming (`PowerRegisterSuspendResumeNotification`).
* The default-device guard only undoes a switch to an endpoint that did not
  exist before the attach; switches to known endpoints are the user's own
  default returning and are left alone.
* Limitations: about 95 ms from an application through MixerCraft to another
  application (USB transfers are queued in 10 ms chunks on both sides); the
  pid.codes *test* product id is used until the project has its own; the
  usbip-win2 dependency (0.9.x) is a third-party kernel driver.

A native WaveRT driver (lower latency) remains possible later if signing is
arranged (OSSign or an EV certificate).

## Linux (implemented: PipeWire stream nodes)

Decision and alternatives: [ADR 0009](../adr/0009-linux-virtual-devices.md).

* `mixercraft-platform`'s `pipewire_dev` gives every virtual device its own
  native PipeWire stream whose `media.class` makes it a device: a strip with
  source `virtual:<name>` gets an `Audio/Sink` node "MixerCraft <name>"
  (`node.name = mixercraft.sink.<slug>`) that applications play into; a bus
  with sink `virtual:<name>` gets an `Audio/Source` node
  (`mixercraft.source.<slug>`) that applications record from. Desktop mixers
  (pavucontrol, GNOME and KDE sound settings, `wpctl`) list them like any
  other device; `pw-play --target mixercraft.sink.game` plays into one.
* F32LE stereo at 48 kHz; PipeWire converts for applications. The nodes
  follow the graph's driver (the sound card, or PipeWire's dummy driver),
  and the engine's drift-compensated FIFOs bridge to its own clock like for
  any device. `node.always-process` keeps them scheduled while no
  application is connected. A virtual device never clocks the engine.
* The nodes exist while the engine's streams are open and vanish with them,
  also on a crash: there is nothing to reconcile and nothing left after
  uninstalling; no configuration of the user or the distribution is edited.
  WirePlumber remembers application routing by `node.name`.
* A PipeWire restart is reported as a device fault; the engine's recovery
  reopens the nodes (measured: 0.5 s).
* Needs a running PipeWire (0.3.49+); without it the capability model
  reports virtual devices as unsupported. A rename of the strip or bus
  reaches the node description when the device next opens.
* Test: `scripts/test-pipewire-virtual.sh` (also a CI job) plays a tone into
  the sink node and records it from the source node.

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
hardware 2026-11-16. Linux: implemented (PipeWire stream nodes, 2026-11-17),
tested against PipeWire without audio hardware (WSL2, CI), not yet on a
desktop with sound hardware. macOS: planned.
