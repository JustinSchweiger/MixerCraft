# 0003 — cpal for physical device I/O (initially)

**Status:** Accepted (2026-11-16)

## Context
The engine needs capture and playback on WASAPI, Core Audio and
PipeWire/ALSA. Writing three native backends first would delay a working
end-to-end engine by weeks.

## Decision
Use `cpal` 0.18 for physical device I/O behind the narrow API of
`mixercraft-platform` (`probe_*`, `open_input`, `open_output` with `f32`
callbacks and a fault callback). On Linux, cpal's native PipeWire host is
available behind the `pipewire` feature.

## Consequences
* A cross-platform engine works today and has been verified on Windows
  hardware.
* cpal does not expose IAudioClient3 low-latency periods, MMCSS task
  selection or PipeWire graph-node integration. Those come as native backends
  behind the same API (phase 4b), keeping cpal as the fallback.
* cpal streams are not `Send` on every backend, so the supervisor owns all
  streams on one thread.
