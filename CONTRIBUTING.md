# Contributing to MixerCraft

Thanks for helping. A few things matter more here than in most projects,
because this is real-time audio software that people leave running for days.

## Before you start
* Read [CLAUDE.md](CLAUDE.md) (short project rules, written for humans too)
  and [the architecture overview](docs/architecture/overview.md).
* For anything larger than a small fix, open an issue first so the approach
  can be discussed.

## Workflow
1. Create a feature branch (`feat/…`, `fix/…`, `docs/…`).
2. Keep changes small and focused; one concern per pull request.
3. Run locally:

   ```bash
   cargo fmt --all
   ```

   ```bash
   cargo clippy --workspace --all-targets
   ```

   ```bash
   cargo test --workspace
   ```

4. Open a pull request using the template. Describe how you verified the
   change, including the hardware and OS if you tested with real devices.

## Rules for audio code
* No allocation, locking, I/O or logging on the audio path. If you add
  real-time code, extend `crates/mixercraft-core/tests/realtime_safety.rs`.
* Test DSP with generated signals and measurable assertions (levels, SNR,
  continuity), not just "it runs".
* Ramp every gain change.
* `unsafe` needs a documented invariant and a test; FFI belongs in clearly
  separated modules.

## Reporting results honestly
Do not describe something as working on a platform unless you ran it there.
Update `docs/feature-matrix.md` and the hardware test log in
`docs/roadmap.md` when you verify something on real hardware.

## License
By contributing you agree that your contributions are licensed under the
project's license (MIT OR Apache-2.0, see ADR 0008).
