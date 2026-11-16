# Architecture overview

```text
 ┌─────────────┐   ┌─────────────┐
 │ Desktop GUI │   │ mixercraft  │   clients: can start, stop, crash and
 │ (phase 7)   │   │ CLI         │   reconnect without affecting audio
 └──────┬──────┘   └──────┬──────┘
        └──── IPC: NDJSON over per-user local socket / named pipe ────┐
                                                                      │
 ┌────────────────────────── mixercraftd (engine process) ────────────▼───┐
 │  Supervisor thread (control plane, never real-time)                    │
 │   • device recovery state machine   • IPC request handling             │
 │   • stall watchdog                  • config persistence (atomic)      │
 │   • software clock management       • diagnostics / recovery log       │
 │            │ lock-free command queue ▼   ▲ garbage queue (frees)       │
 │  EngineCore (real-time; driven by exactly one clock at a time)         │
 │   master device callback ──or── software clock thread                  │
 │     ├─ FifoReader per input  (resample + drift control) → strips       │
 │     ├─ Mixer: strips → faders → routing matrix → buses                 │
 │     ├─ master bus → directly into the master device buffer             │
 │     └─ other buses → FifoWriter → device callback (resample + drift)   │
 └────────────────────────────────────────────────────────────────────────┘
        │ mixercraft-platform: capability model + native streams
   WASAPI (Windows) · PipeWire / ALSA (Linux) · Core Audio (macOS)
```

## Crates

| Crate | Responsibility | Platform code |
|---|---|---|
| `mixercraft-core` | Real-time audio: buffers, gain smoothing, meters, channel maps, mixer, resampler, drift control, FIFOs, processing cycle | none |
| `mixercraft-config` | Versioned TOML schema, migrations, validation, atomic persistence | none |
| `mixercraft-ipc` | Wire protocol, framing, per-user endpoint, client and server helpers | endpoint naming only |
| `mixercraft-platform` | Capability model, device discovery, native capture/playback streams | yes (via cpal now; native modules later) |
| `mixercraft-engine` | `mixercraftd`: supervisor, recovery, clocks, IPC server | no direct OS calls |
| `mixercraft-cli` | `mixercraft` command-line client | none |

Further crates are added only when they carry their own weight (GUI in phase
7, updater in phase 9, native virtual-device integrations in phase 5).

## Real-time rules

Code on the audio path (`EngineCore::run_cycle`, `render_master`,
`FifoReader::read`, `FifoWriter::write`, everything they call) must not
allocate or free memory, take blocking locks, perform I/O or logging, or run
unbounded loops. Concretely:

* All buffers are sized at construction on the control thread.
* Configuration changes arrive as `Command`s through a bounded SPSC queue
  (`rtrb`), at most 64 per cycle.
* Objects removed on the audio thread (detached FIFOs, replaced mixers) go
  back through a garbage queue and are freed by the supervisor. If that queue
  were ever full the object is freed in place and `rt_deallocations` counts
  it — this is a diagnostic, expected to stay 0.
* Statistics and meters are atomics; readers never block writers.
* The integration test `crates/mixercraft-core/tests/realtime_safety.rs`
  runs thousands of cycles — with resampling, drift control, parameter
  changes and attach/detach — under an allocator that aborts on any
  allocation.

### Driving the core from two possible clocks
`EngineCore` lives in an `Arc<Mutex<_>>`. Its drivers (the master device
callback and the software clock) only ever call `try_lock`; the supervisor
never locks it. In steady state there is one driver and the lock is
uncontended (no system call). During a master switch both may briefly exist;
the loser of `try_lock` outputs silence for that callback and increments
`skipped_cycles`. No thread ever blocks on the lock.

## Clock domains

See [feasibility §4](../research/feasibility.md#4-clocks-drift-and-latency).
The engine runs at the master device's native rate, so the master path has no
resampler. If the master is unavailable or later reopens at a different rate,
its bus is served through a FIFO like any other output and a software clock
drives the engine.

## Recovery

Faults are isolated per device stream:

| Signal | Source | Reaction |
|---|---|---|
| device lost / stream invalidated / other error | backend error callback | close stream, detach FIFO, reopen with backoff |
| stream stalled (no callbacks for 2 s) | watchdog on FIFO/cycle counters | same as above |
| reroute (default device changed) | backend | logged; stream continues |
| backend xrun | backend | logged |
| FIFO underrun | reader | silence for the missing part, re-prime, raise target (bounded) |
| FIFO overrun | writer/reader | drop/trim to target, counted |
| NaN/Inf in audio | mixer | replaced with silence, counted |
| master lost | any of the above | software clock takes over; master returns automatically |

Backoff: 0.25 s doubling to 10 s. After 10 consecutive failures the device is
reported **failed** and retried only every 60 s, so a broken device is visible
in diagnostics rather than hidden by endless fast restarts. A stream that
runs 30 s without faults resets its counter.

## IPC

Newline-delimited JSON over a local socket: a named pipe on Windows,
a Unix domain socket in `$XDG_RUNTIME_DIR` (or `$TMPDIR`) elsewhere. The first
message must be `hello` with the protocol version; mismatches are rejected
explicitly. Changes within a protocol version are additive only, and unknown
fields are ignored. Binding the endpoint doubles as the single-instance guard.
At most 8 clients are served concurrently.

**Before the engine runs as a Windows service**, the named pipe needs an
explicit security descriptor that admits only the interactive user(s) it
serves; the default DACL is acceptable for a per-user process only.

## Configuration

`mixercraft-config` stores strips, buses, routes, device references (stable
backend id plus display name as fallback) and engine settings in TOML with a
`schema_version`. Documents from newer versions are refused, older ones
upgraded step by step. Writes are atomic with a `.bak` copy; the engine falls
back to the backup if the main file is unreadable and never replaces the
user's routing with defaults silently.

## Known limitations of the current implementation

* Physical I/O goes through cpal; Windows periods are therefore 10 ms and
  MMCSS registration is cpal's choice.
* Meters are peak-hold-since-last-read; with several clients polling, they
  share one hold value.
* Device hot-plug is handled by retrying; there is no device-arrival
  notification yet (the retry cadence is ≤10 s, ≤60 s for failed devices).
* No virtual endpoints, background service or autostart yet (phases 5–6).
