# 0002 — Clock master and drift-compensated FIFOs

**Status:** Accepted (2026-11-16)

## Context
Every audio device has its own crystal. A mixer combining several devices
must reconcile clock rates that differ by tens to hundreds of ppm, or a buffer
between them eventually over- or underflows. That is how long-running mixers
start crackling after hours.

## Decision
* One output device is the **master**. Its callback drives the engine and its
  bus is rendered directly into the device buffer.
* Every other device stream connects through an SPSC FIFO. The reader side
  owns a windowed-sinc variable-ratio resampler (64 taps, 256 phases, Kaiser
  window) and a PI controller on the FIFO fill level that steers the ratio
  (ωₙ = 0.1 rad/s, ζ = 1, ±2000 ppm authority, anti-windup). The fill level
  includes audio buffered inside the resampler.
* FIFO targets start from the latency profile, adapt to measured block sizes
  after a 250 ms warm-up and grow by 50 % (bounded) after a genuine underrun.
* Without a master, a software clock drives the engine.

## Consequences
* Long-term stability does not depend on device clocks matching.
* Non-master devices add FIFO latency (measured: 20 ms with 10 ms device
  periods). Shorter device periods (IAudioClient3, PipeWire quantum) reduce it.
* The fill measurement aliases against bursty producers, which causes a small
  ratio ripple (bounded at ±150 ppm ≈ 0.26 cent in tests). Interpolating the
  fill level from callback timestamps is a possible refinement.
* On Linux, a future PipeWire graph-node backend can delegate clock domains to
  PipeWire for the devices it manages.

## Alternatives considered
* Aggregate devices / OS-level drift correction: only available on macOS and
  behaves differently per platform.
* Fixed large buffers: only delay the failure.
* `rubato`: capable, but its chunk-oriented API complicates continuous ratio
  steering. Our resampler is ~250 lines with quality tests (> 90 dB SNR for
  1 kHz at 44.1 → 48 kHz, > 70 dB alias rejection when downsampling).
