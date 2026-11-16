# Building and platform setup

## All platforms

* Rust ≥ 1.88 (stable). `rustup` recommended.
* `cargo build --workspace` builds everything; `cargo test --workspace` runs
  all tests that do not need audio hardware.

The real-time core is compiled with `opt-level = 2` even in debug builds
(see the workspace `Cargo.toml`), because unoptimised DSP code is too slow to
run glitch-free.

## Windows

* Visual Studio Build Tools with the "Desktop development with C++" workload
  (MSVC linker and Windows SDK).
* ARM64: add the MSVC ARM64 build tools and `rustup target add
  aarch64-pc-windows-msvc`.
* Future driver work: Windows Driver Kit (WDK) matching the SDK; test-signing
  only on dedicated test machines.

## Linux

```bash
sudo apt-get install libasound2-dev pkg-config
```

Optional native PipeWire host (`--features mixercraft-platform/pipewire`):

```bash
sudo apt-get install libpipewire-0.3-dev libclang-dev
```

Fedora: `alsa-lib-devel pipewire-devel clang-devel`.

## macOS

* Xcode command-line tools.
* Universal builds: `rustup target add x86_64-apple-darwin aarch64-apple-darwin`.
* Microphone access: the first capture triggers the system permission prompt
  for the launching application (Terminal when run from a shell).

## Useful commands

| Command | Purpose |
|---|---|
| `cargo fmt --all` | format |
| `cargo clippy --workspace --all-targets` | lint (CI runs with `-D warnings`) |
| `cargo test --workspace` | all tests |
| `cargo test -p mixercraft-core --release` | DSP tests with optimised code |
| `cargo deny check` | licenses, advisories, sources (`cargo install cargo-deny`) |
| `mixercraftd --list-devices` | device ids for configuration files |
| `MIXERCRAFT_LOG=debug mixercraftd` | verbose engine log |

## Testing against hardware safely

A configuration that routes a microphone to speakers causes feedback. For
experiments, route inputs to a bus without a sink (metering only) or to a muted
bus, as in this example:

```toml
schema_version = 1

[engine]
master_bus = "A1"

[[strips]]
name = "Mic"
source = { kind = "default-input" }
routes = ["M1"]

[[buses]]
id = "A1"
name = "Main"
sink = { kind = "default-output" }

[[buses]]
id = "M1"
name = "Meter only"
```
