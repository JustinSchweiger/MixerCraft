# 0006 — Distribution and update architecture

**Status:** Accepted for Windows (per-user MSI and in-app updater implemented,
2026-11-16); macOS and Linux packages still proposed.

## Context
Users must install without compiling, drivers and plug-ins must be provisioned
safely, updates must be authenticated, and a UI-only update must not
interrupt audio. The owner wants zero recurring cost, and the source
repository is private, so its GitHub Releases cannot be downloaded without a
token. (The owner made the repository public on 2026-11-16; see "Release
source".)

## Decision
* **Release source:** GitHub Releases of the public MixerCraft repository: a
  JSON feed manifest (`latest.json`) and the installer as assets of the
  latest release; the app reads
  `https://github.com/JustinSchweiger/MixerCraft/releases/latest/download/latest.json`
  (compiled in by the build script, overridable in `updates.toml`). Releases are
  built and published manually (`scripts/build-installer.ps1`, see
  `docs/development/releasing.md`) — never automatically from a push to
  `main`. Once CI minutes are available, the same script runs in a manually
  approved workflow (protected `release` environment) from a semver tag.
* **Installers**
  * Windows: WiX MSI, **per-user** (`%LOCALAPPDATA%\Programs\MixerCraft`, no
    administrator rights, so the updater runs it without a UAC prompt). Built
    with WiX Toolset **5.0.2** (MS-RL, pinned as a local dotnet tool; WiX 6+
    ships under the Open Source Maintenance Fee EULA, which we avoid). The
    package contains the GUI, the engine and the CLI, a Start-menu shortcut
    and optional features: desktop shortcut, start with Windows (tray app via
    the `Run` key, engine via the Task Scheduler task of `mixercraft autostart
    enable`), CLI on the user `PATH`. It **does not bundle or install
    drivers**: virtual devices need usbip-win2 (ADR 0007), which the user
    installs; the installer detects it and explains where to get it. No
    engine service yet (the engine runs per user; service mode is roadmap
    item 6). Upgrades remove the old version first (`MajorUpgrade` after
    `InstallInitialize`), after `mixercraft close-app` has closed the app and
    stopped this installation's engine; `%APPDATA%\MixerCraft` (configuration)
    is never touched. Uninstall removes the autostart task only if it starts
    this installation's engine.
  * Signing: binaries and MSI are **unsigned** for now. A free Authenticode
    route is SignPath Foundation for open-source projects, which needs a
    public repository — possible now that the repository is public; to be
    applied for. SmartScreen warns on first download until then.
  * macOS: Developer ID-signed, notarized `.pkg` (app, HAL plug-in, LaunchAgent
    registration) and a `.dmg` for the app; universal binaries.
  * Linux: `.deb` and `.rpm` (systemd user unit, PipeWire dependency) and an
    AppImage. Flatpak only if PipeWire node/link management works inside the
    sandbox without broad permission holes.
  * `cargo-dist` is not used for these installers because it does not handle
    drivers, services or HAL plug-ins. It may still produce plain CLI archives.
* **Updater (`mixercraft-update` crate)** fetches the feed manifest
  (`latest.json`; one URL per channel), compares the semantic version with its
  own, and — only after the user clicks "Download and install" — downloads the
  installer for its platform target, checks **size and SHA-256** against the
  manifest and hands it to the platform installer (Windows: `msiexec /i …
  /passive LAUNCHAPP=1`, after which the app quits; the MSI stops the engine,
  replaces the files and starts the app again). Checks run 20 s after start
  and daily; the user can turn them off (About tab, stored in
  `%APPDATA%\MixerCraft\config\updates.toml`). Only `https://`, `file://` and
  loopback `http://` URLs are accepted; TLS uses rustls with the operating
  system's certificate store. Only copies installed by the MSI (marker file
  `installed-by-msi.txt`) install updates; development builds only report
  them.
* **Deviation — authenticity:** the original decision required an Ed25519
  (minisign) signature on the manifest with a key compiled into the app. That
  is **not implemented yet**. Today the checksum proves integrity (no
  corrupt or truncated installer), while authenticity rests on HTTPS and on
  the release host account: whoever can publish a release can ship code to
  users. Signed manifests are the next step (follow-up in the roadmap); the
  manifest `schema` field lets a signed format be introduced.
* **Components are versioned separately** in the manifest (later): GUI, engine,
  IPC protocol, driver/plug-in, configuration schema. Today one MSI carries
  all three programs, so every update restarts the engine; the GUI says so
  before the click ("stops the audio engine for a few seconds"). Routing and
  device selection are never changed by an update; configuration migrations
  keep a backup for rollback.
* **Distribution-managed Linux packages** only notify; the package manager
  installs.
* Signing keys (when introduced) live only with the release maintainer or the
  protected release environment; pull-request workflows never see them.

## Release source

The app needs a URL that serves `latest.json` and the MSI **without
credentials** (a token compiled into the app would leak and grant access to
the private source). Options considered while the repository was private:

| Option | Cost | Effort | Notes |
|---|---|---|---|
| **A. Public releases-only repository** (e.g. `MixerCraft-releases`) with GitHub Releases; feed `https://github.com/<owner>/MixerCraft-releases/releases/latest/download/latest.json` | free | low: create repo once, upload MSI + `latest.json` per release | Source stays private. Stable URLs, GitHub CDN, release notes page. Also makes SignPath possible later. **Recommended.** |
| B. Make the main repository public | free | none | Releases work as-is; exposes the source (the license already allows that). |
| C. Static host (GitHub Pages of a public repo, Cloudflare Pages/R2 free tier, Netlify) | free within limits | medium | Pages limits (Cloudflare Pages: 25 MB per file); another account to manage. |
| D. Token in the app / private repo | — | — | Rejected: the token leaks with every copy. |

**Chosen: B** — the owner made the repository public (2026-11-16). The
client does not depend on this: `-Feed` of the build script sets the
built-in URL (empty = no checks; the About page then says so), `updates.toml`
(`feed_url = "…"`) overrides it, and a `file://` folder feed works for local
testing. Moving to option A or C later only changes that URL.

## Consequences
More packaging work than a single cross-platform bundler, in exchange for
correct handling of drivers, services and per-platform update rules. The
per-user MSI cannot install a pre-login engine service; a per-machine variant
(elevated) is needed once service mode exists. Without signed manifests and
Authenticode, the security of updates equals that of the release host
account.
