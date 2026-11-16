# Spikes

Throwaway prototypes used to make decisions. They are excluded from the
workspace (`exclude = ["spikes"]`), not built by CI and not maintained.
Results are recorded in the corresponding ADR.

## GUI framework (ADR 0005)

Two implementations of the same mixer view: 24 input strips and 8 bus strips
(32 stereo meters animated at 60 Hz), each with a custom fader, mute/solo
toggles, dB readout and accessibility metadata.

| Prototype | Path | Stack |
|---|---|---|
| Slint | `gui-slint/` | Slint 1.19, `.slint` markup in the `slint!` macro |
| egui | `gui-egui/` | eframe/egui 0.37, custom-painted widgets |

Both accept `--mode active|idle` (animated meters or silence) and
`--seconds N` (quit after N seconds) and print `first_frame_ms=<n>`.
Slint's renderer can be switched with `SLINT_BACKEND=winit-software`.

Build each with `cargo build --release` in its directory, then run the
harness on Windows:

```bash
powershell -ExecutionPolicy Bypass -File spikes/measure.ps1 -OutDir spike-results
```

It measures, per prototype and mode: CPU (average over 20 s, in % of one
core), private and working-set memory, time to first frame, a window
screenshot and the UI Automation tree (what Narrator and other screen
readers see).
