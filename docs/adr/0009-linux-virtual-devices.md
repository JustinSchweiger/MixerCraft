# 0009 — Linux virtual devices as PipeWire stream nodes

**Status:** Accepted (2026-11-17)

## Context
Virtual devices (`virtual:<name>` selectors) must appear to other
applications as a playback device (a strip's input) or a recording device
(a bus's output). On Linux, PipeWire can do this in user space without any
driver. The original plan ([virtual devices](../architecture/virtual-devices.md),
feasibility §3) was to create `support.null-audio-sink` adapter nodes with
`object.linger = true` and reconcile them by name on every engine start; the
engine would then still need to capture from the sink's monitor ports and
play into the source through separate streams and links.

## Decision
Each virtual device is **one native `pw_stream` of the engine** whose
`media.class` makes it a device, as `pw-loopback` does:

* strip input: `media.class = Audio/Sink`, direction input,
  `node.name = mixercraft.sink.<slug>`;
* bus output: `media.class = Audio/Source`, direction output,
  `node.name = mixercraft.source.<slug>`;
* F32LE stereo at 48 kHz (PipeWire converts for applications),
  `node.always-process = true` so the nodes are scheduled while no
  application is connected (otherwise the engine's stall watchdog would
  reopen them every few seconds);
* one thread with its own PipeWire main loop per device; the process
  callback runs there, converts between bytes and `f32` in a preallocated
  buffer and calls the engine's FIFO — no allocation, locks or I/O;
* built with the safe API of the `pipewire` crate 0.10 (MIT; already in the
  lock file through cpal's optional PipeWire host), feature `v0_3_49` for
  `pw_buffer.requested`. No `unsafe` code.

The nodes live exactly as long as the engine's streams: closing the stream,
stopping or crashing the engine removes them (PipeWire drops a client's
objects with its connection). WirePlumber remembers application routing by
`node.name`, so it returns when the engine starts again.

## Consequences
* No lingering nodes to reconcile or clean up; uninstalling leaves nothing
  behind; nothing in the user's or distribution's PipeWire configuration is
  edited.
* Applications lose the device while the engine is stopped (same as on
  Windows) and WirePlumber moves their streams to the default device.
* The engine links against `libpipewire-0.3` on Linux (build: headers and
  libclang; run: PipeWire 0.3.49 or newer). Without a PipeWire socket the
  capability model reports virtual devices as unsupported and the engine
  still runs with physical devices.
* A strip or bus rename reaches the node description the next time the
  device opens: the safe API cannot update a stream's properties.
* Verified against a real PipeWire 1.0 daemon with its dummy driver (WSL2
  and CI, `scripts/test-pipewire-virtual.sh`), not yet on a desktop with
  audio hardware.

## Rejected alternatives
* **Lingering `support.null-audio-sink` nodes**: extra monitor capture and
  links, nodes survive crashes and need reconciliation.
* **`pactl load-module module-null-sink`**: depends on pipewire-pulse and the
  `pactl` tool; modules outlive the engine.
* **cpal's native PipeWire host** for these streams: it opens streams that
  connect *to* devices; it cannot create device-class nodes.
