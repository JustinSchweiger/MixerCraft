# 0006 — Distribution and update architecture

**Status:** Proposed (implementation in phases 8–9)

## Context
Users must install without compiling, drivers and plug-ins must be provisioned
safely, updates must be authenticated, and a UI-only update must not
interrupt audio.

## Decision
* **Release source:** GitHub Releases. Releases are created only by a
  manually approved workflow (protected `release` environment) from a semver
  tag — never automatically from a push to `main`.
* **Installers**
  * Windows: WiX MSI (engine service, GUI, driver package via `pnputil`),
    Authenticode-signed.
  * macOS: Developer ID-signed, notarized `.pkg` (app, HAL plug-in, LaunchAgent
    registration) and a `.dmg` for the app; universal binaries.
  * Linux: `.deb` and `.rpm` (systemd user unit, PipeWire dependency) and an
    AppImage. Flatpak only if PipeWire node/link management works inside the
    sandbox without broad permission holes.
  * `cargo-dist` is not used for these installers because it does not handle
    drivers, services or HAL plug-ins. It may still produce plain CLI archives.
* **Updater (own crate)** fetches a channel manifest (`stable.json`,
  `beta.json`) from the release, verifies its **Ed25519 (minisign)** signature
  with a public key compiled into the application, compares versions,
  downloads the platform artifact and verifies its signature and SHA-256 before
  handing it to the platform installer. HTTPS and checksums alone are not
  treated as proof of authenticity; platform code signing is checked in
  addition where the OS supports it.
* **Components are versioned separately** in the manifest: GUI, engine, IPC
  protocol, driver/plug-in, configuration schema. The updater decides per
  update whether the engine must restart (announced and user-confirmed, never
  forced during an active session) or the driver must be replaced (explicitly
  not interruption-free). Routing and device selection are never changed by an
  update; configuration migrations keep a backup for rollback.
* **Distribution-managed Linux packages** only notify; the package manager
  installs.
* Signing keys live only in the protected release environment; pull-request
  workflows never see them.

## Consequences
More packaging work than a single cross-platform bundler, in exchange for
correct handling of drivers, services and per-platform update rules.
