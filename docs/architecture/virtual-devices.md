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

## Windows

* Kernel-mode WaveRT miniport derived from SysVAD (or ACX), in
  `platform/windows/virtual-audio-driver/` (C/C++, WDK). The driver only
  loops render DMA data to the paired capture endpoint; it contains no mixing
  logic.
* Endpoint count fixed per driver version (initially 2 cables + 1 "system"
  cable), names configurable via registry (INF `AddReg`).
* Installation: driver package installed by the MSI via `pnputil
  /add-driver ... /install`; removal via `pnputil /delete-driver ...
  /uninstall`. Upgrade replaces the package; the engine re-opens streams when
  the endpoints reappear (normal recovery path).
* Signing: attestation-signed via Partner Center (see ADR 0007). Test-signed
  builds on dedicated development machines only.

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

## macOS

* AudioServerPlugIn bundle (`MixerCraft.driver`) in
  `/Library/Audio/Plug-Ins/HAL`, based on Apple's NullAudio sample, written
  in C with a minimal surface: device/stream properties and a lock-free ring
  per cable between the output and input sides.
* Installed by the signed `.pkg` (post-install: restart `coreaudiod`).
  Removal deletes the bundle and restarts `coreaudiod`.
* Per-application capture via Core Audio process taps on macOS 14.2+.

## Status

Planned for phase 5. Nothing in this document is implemented yet.
