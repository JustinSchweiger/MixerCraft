# Technical feasibility report

Status: initial research, November 2026. Statements marked **[verify]** are
engineering judgement that still needs confirmation on real hardware or
against primary documentation before we rely on them.

## 1. What Voicemeeter does, and what we must match

Voicemeeter is a virtual mixing console: hardware inputs and *virtual
inputs* (playback endpoints other applications send audio to) are mixed on
strips and routed to hardware outputs (A buses) and *virtual outputs*
(recording endpoints other applications record from, B buses). The pieces are:

| Piece | Voicemeeter | MixerCraft |
|---|---|---|
| Mixing engine | user-mode process | `mixercraft-core` + `mixercraftd` |
| Virtual endpoints | kernel driver (VAIO) | per-OS, see §3 |
| Hardware I/O | WASAPI/KS/MME/ASIO | WASAPI · PipeWire · Core Audio |
| Clock handling | A1 is master, others resampled | same model, explicit drift control (§4) |
| UI | in-process | separate process over IPC |

Reported pain points and the design answer:

* **Slow start** — the engine is a small headless daemon. Measured on the
  development machine (Windows 11, debug and release builds): ~210–230 ms
  from process start to audio running with three devices.
* **Degradation after hours (crackle, robotic sound, dropouts)** — classic
  symptoms of unmanaged clock drift (a FIFO between two devices slowly
  over/underflows) and of state corruption (NaN/denormal in DSP state). The
  core counters these with a PI-controlled adaptive resampler per
  device, NaN sanitising, measured-block-size FIFO targets, overrun trimming
  and a stall watchdog. Every one of these is counted and exposed.

## 2. Physical audio I/O per platform

### Windows
* **WASAPI shared mode**, event-driven, is the right default: it coexists
  with every other application. Exclusive mode is optional later.
* **IAudioClient3** (`InitializeSharedAudioStream`) allows shared-mode
  periods below 10 ms on drivers that support it. `cpal` uses the classic
  `IAudioClient` path, so its periods are typically 10 ms (480 frames at
  48 kHz) — confirmed on the development machine. A native WASAPI backend
  with IAudioClient3 and MMCSS ("Pro Audio" task) is planned for the
  low-latency profile.
* cpal 0.18 follows the default device for streams opened on it
  (`ErrorKind::DeviceChanged` signals the reroute).

### Linux
* **PipeWire** is the primary target. cpal 0.18 ships a native PipeWire host
  (feature `pipewire`, needs `libpipewire-0.3-dev`); without it, cpal's ALSA
  host works on PipeWire systems through `pipewire-alsa`.
* Important difference from Windows/macOS: PipeWire is itself a graph with a
  clock-domain scheduler that already resamples followers. The long-term
  Linux backend should therefore run the mixer **inside** the PipeWire graph
  as a `pw_filter` node and let PipeWire link it to devices, instead of
  opening one stream per device. That removes our FIFOs/resamplers for
  PipeWire-managed devices entirely. **[verify]** latency and scheduling
  behaviour of a filter node with many ports.

### macOS
* Core Audio HAL via cpal (AUHAL). Aggregate devices exist, but we keep our
  own drift handling so behaviour matches the other platforms.

## 3. Virtual endpoints (highest-risk area)

### Windows — kernel driver required
There is no user-mode API that creates playback/recording endpoints visible
to all applications. Options:

| Option | Notes |
|---|---|
| SysVAD-derived WaveRT miniport (PortCls) | Microsoft sample, MIT licensed, proven model (VB-Cable, Voicemeeter VAIO work this way) |
| ACX (Audio Class eXtensions, KMDF) | Microsoft's modern framework for new audio drivers on Windows 11 |

Plan: one driver package exposing *N* virtual cables, each a render endpoint
("MixerCraft Input n") plus a capture endpoint ("MixerCraft Output n"). The
render side's DMA buffer is looped to the capture side. The engine attaches
to the endpoints with ordinary WASAPI streams, so no custom kernel↔user IPC
is needed initially. Each cable runs on its own (system-timer derived) clock
and is handled by the normal drift compensation.

**Signing is the dominant risk.** Since the April 2026 Windows update,
Windows 11 24H2/25H2/26H1 trust only kernel drivers signed through the
Windows Hardware Compatibility Program (attestation or HLK); cross-signed
drivers are no longer trusted. Attestation signing requires a Partner Center
hardware account, which in turn requires an **EV code-signing certificate**
held by a registered organisation. From March 2027 submissions must include
an SPDX SBOM. Development builds can use test signing (`bcdedit
/set testsigning on`) on dedicated test machines only — never instruct users
to do that. **Decision needed from the project owner**: who owns the EV
certificate and Partner Center account (see ADR 0007).

Without a driver, Windows can still offer: physical mixing and routing,
per-application *capture* via WASAPI process loopback
(`AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK`, Windows 10 2004+), and
redirecting applications to a chosen output via the undocumented
`IAudioPolicyConfig` interface used by the Windows volume mixer
(**[verify]** stability across builds; undocumented → feature-flagged).

### Linux — no driver needed
PipeWire provides virtual devices natively:

* `support.null-audio-sink` adapter nodes with `media.class =
  Audio/Sink` (virtual playback) or `Audio/Source/Virtual` (virtual
  recording), created at runtime with `object.linger = true` so they survive
  engine restarts and applications stay connected.
* They appear in desktop audio settings and in PulseAudio-compatible apps via
  `pipewire-pulse` without further work.
* WirePlumber owns policy (default device, automatic linking). We set node
  properties and metadata (`target.object`) instead of fighting its policy,
  and never edit the user's global configuration files.

### macOS — Audio Server Plug-in
* An **AudioServerPlugIn** (HAL plug-in) bundle in
  `/Library/Audio/Plug-Ins/HAL` is loaded by `coreaudiod` in user space.
  Installation needs administrator rights and a `coreaudiod` restart; no SIP
  changes. The bundle must be Developer ID signed and notarized. This is how
  BlackHole works. BlackHole is GPL-3.0, so we study its architecture but do
  **not** copy code; Apple's NullAudio plug-in sample is the starting point.
* **AudioDriverKit** targets hardware-backed drivers and requires
  Apple-granted entitlements — not suitable for pure virtual devices.
* **Core Audio process taps** (macOS 14.2+) capture individual applications or
  the whole system without a driver, and can mute the original output
  (`CATapDescription` mute behaviour). Together with a virtual device this
  gives true per-application routing on macOS 14.2+. Requires the "System
  Audio Recording" privacy permission. Forum reports mention all-zero
  buffers and per-output-pair attenuation — **[verify]**.

## 4. Clocks, drift and latency

Two devices that both report 48 kHz differ by tens to hundreds of ppm.
Over an hour, 100 ppm accumulates 17 280 frames (360 ms) — any fixed buffer
between them must eventually overflow or underflow. MixerCraft's model:

* One **master** device clocks the engine; its bus is rendered directly in
  its callback (no extra buffering, no resampling).
* Every other device is connected through a lock-free SPSC FIFO whose
  *reader* runs a band-limited variable-ratio resampler steered by a PI
  controller on the FIFO fill level (natural frequency 0.1 rad/s, ±2000 ppm
  authority). The learned ratio is the clock ratio; steady-state pitch error
  is zero and ripple is well below audibility (tests bound it to ±150 ppm ≈
  ±0.26 cent under adversarial block patterns).
* If the master disappears, a software clock keeps the engine running until
  it returns.
* FIFO targets start from the latency profile and adapt to the block sizes
  the devices actually deliver (measured, not configured).

Measured on the development machine: three devices with 480-frame periods
settle at a 960-frame (20 ms) FIFO target per non-master device, with zero
underruns after warm-up and drift corrections in the single-digit ppm range.

## 5. Startup analysis

### Windows: can the startup sound go through the mixer?
The Windows startup sound is played by the system during sign-in to the
*default render endpoint*. For it to pass through MixerCraft:

1. the default endpoint must be a MixerCraft virtual endpoint (requires the
   driver, §3; PnP drivers load during boot, so the endpoint exists early), and
2. the engine must already be forwarding that endpoint's loopback to
   hardware, i.e. run as a **Windows service** that starts with
   `Audiosrv`/`AudioEndpointBuilder` as dependencies — a per-user autostart
   at login is too late, as the requirement anticipated.

Services in session 0 can use WASAPI. Open questions **[verify]**: the
ordering between service start and the logon sound, behaviour with Fast
Startup (which commonly suppresses the startup sound), and per-user settings
when a machine-wide service owns the routing. Conclusion: plausible, only
with the driver + service architecture; to be validated by a timing test on
real hardware before promising it. No boot-configuration changes are needed
or acceptable.

### Linux
PipeWire runs per user session; there is no supported way for a system
service to use a user's PipeWire graph before login. The engine runs as a
**systemd user service** ordered after `pipewire.service` and
`wireplumber.service`, and restores its nodes/links when the session audio
becomes available.

### macOS
The boot chime is produced by firmware before macOS runs and cannot be
routed. The engine runs as a per-user LaunchAgent registered via
`SMAppService`; the HAL plug-in's devices exist as soon as `coreaudiod` runs,
so applications can open them before the engine starts (they hear silence
until it does).

## 6. GUI technology

See ADR 0005. Summary: Tauri is rejected for the mixer UI (web view memory
footprint and 60 Hz meter traffic through a JS bridge); Slint is the
preferred candidate, with egui as the fallback, to be decided by a short
spike measuring idle CPU with 32 animated meters, text quality, and screen
reader output on all three platforms.

## 7. Distribution and updates

See ADR 0006. Native installers per platform (WiX MSI with driver package,
signed/notarized `.pkg` + `.dmg`, `.deb`/`.rpm`/AppImage), a custom updater
verifying Ed25519 (minisign) signatures over a release manifest published on
GitHub Releases, separate component versions (GUI, engine, driver, IPC
protocol, config schema) so that a UI-only update never restarts audio, and
notify-only behaviour for distribution-managed Linux packages.

## 8. Risk register

| # | Risk | Impact | Mitigation |
|---|---|---|---|
| R1 | Windows driver signing needs EV cert + Partner Center | No virtual devices on Windows | Owner decision (ADR 0007); test-signed builds for development; ship physical mixing + process loopback meanwhile |
| R2 | Kernel driver bugs crash the OS | Severe | Keep driver minimal (loopback only), all logic in user mode; Driver Verifier + HLK tests on dedicated machines |
| R3 | macOS plug-in notarization / coreaudiod restart UX | Install friction | Signed `.pkg` with post-install restart of coreaudiod and clear messaging |
| R4 | Startup-sound routing on Windows not achievable | Requirement unmet | Timing test early in phase 6; document outcome honestly |
| R5 | cpal limits (no IAudioClient3/MMCSS control) | Higher minimum latency on Windows | Native WASAPI backend behind the same interface |
| R6 | PipeWire filter-node backend complexity | Linux schedule | Keep cpal streams as a working fallback |
| R7 | Undocumented Windows per-app routing API | Breakage on updates | Feature flag + capability model reports it as Partial |
| R8 | Hardware-dependent behaviour not covered by hosted CI | Regressions | Self-hosted runners with loopback hardware (phase 10) |

## Sources

* cpal 0.18 changelog and upgrade guide (native PipeWire and PulseAudio
  hosts, unified errors): https://docs.rs/crate/cpal/latest
* Windows kernel driver trust policy, April 2026:
  https://www.theregister.com/2026/03/27/microsoft_kernel_trust/ and
  https://www.thefpsreview.com/2026/03/30/microsofts-april-2026-windows-update-ends-trust-for-cross-signed-kernel-drivers/
* Driver SBOM requirement from March 2027:
  https://www.elevenforum.com/t/windows-11-driver-signing-will-require-sboms-and-vex-statements-starting-march-2027.49183/
* SysVAD sample: https://github.com/microsoft/Windows-driver-samples/tree/main/audio/sysvad
* Core Audio process taps (macOS 14.2+): https://www.recall.ai/blog/core-audio-taps ,
  https://github.com/insidegui/AudioCap
* BlackHole: https://github.com/ExistentialAudio/BlackHole
* PipeWire loopback and nodes: https://pipewire.pages.freedesktop.org/pipewire/page_module_loopback.html
