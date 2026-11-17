# Installers and releases

Decisions: [ADR 0006](../adr/0006-distribution-and-updates.md). Everything here
is free tooling; nothing is signed with a paid certificate.

| Platform | Package | Built by | Updates |
|---|---|---|---|
| Windows x64 | per-user MSI + `latest.json` | `scripts/build-installer.ps1` (locally) | in-app: download, verify, install |
| Linux x86_64 / ARM64 | `.deb`, portable `.tar.gz` | `scripts/package-linux.sh` (release workflow) | in-app check, links to the release page |
| macOS (universal) | `.dmg` with `MixerCraft.app` (ad-hoc signed) | `scripts/package-macos.sh` (release workflow) | in-app check, links to the release page |

## Windows: build the installer

Needs the Rust MSVC toolchain and a .NET SDK (6 or newer). WiX Toolset 5.0.2
is pinned in `.config/dotnet-tools.json` and restored by the script.

```powershell
powershell -ExecutionPolicy Bypass -File scripts\build-installer.ps1
```

Output in `target\installer\`:

| File | Purpose |
|---|---|
| `MixerCraft-<version>-x64.msi` | per-user installer |
| `MixerCraft-<version>-x64.msi.sha256` | checksum (`sha256sum` format) |
| `latest.json` | release feed manifest for the in-app updater |

Options: `-Version 0.2.0` (MSI version, default from `Cargo.toml`), `-Feed
<url>` (feed compiled into the app), `-Notes "…"`, `-NotesUrl https://…`,
`-AssetUrl <url>` (default: relative file name), `-SkipCargo` (package the
existing `target\release` binaries).

The build is repeatable from a clean checkout (pinned WiX, `cargo build
--locked`), but not bit-for-bit identical: every MSI gets a new package code
and timestamps. Compare releases by the programs inside, not the MSI hash.

## What the MSI does

* Installs `mixercraft-gui.exe`, `mixercraftd.exe`, `mixercraft.exe` to
  `%LOCALAPPDATA%\Programs\MixerCraft` and a Start-menu entry.
* Optional features (all on by default, chosen on the "Custom Setup" page,
  changeable later in Apps & features → Modify): desktop shortcut; start with
  Windows (the "MixerCraft" scheduled task that opens the app in the tray, the
  same switch as on the Settings tab; set on a new install only, so a later
  choice on the Settings tab survives updates); the install folder on the
  user `PATH`.
* Checks for usbip-win2 (`%ProgramFiles%\USBip\usbip.exe`); if missing, the
  last page explains that virtual devices need it and where to get it
  (<https://github.com/vadimgrn/usbip-win2/releases>). Drivers are never
  bundled or installed.
* Upgrade: runs the installed `mixercraft close-app` (asks the app to quit and
  stops this installation's engine; an engine started from another folder,
  such as a development build, is left alone), removes the old version,
  installs the new one. The configuration in `%APPDATA%\MixerCraft` is kept.
* Uninstall: closes the app, removes the sign-in tasks ("MixerCraft",
  "MixerCraft Engine") if they start this installation's programs, removes
  the files, shortcuts and `PATH` entry. Configuration and logs stay (delete `%APPDATA%\MixerCraft` and
  `%LOCALAPPDATA%\MixerCraft` to remove them).

Unattended: `msiexec /i MixerCraft-0.2.0-x64.msi /passive` (all default
features), `ADDLOCAL=Main,Autostart` to choose features, `/x` to uninstall.

## Linux: build the packages

On the target architecture (x86_64 or ARM64), with the build dependencies
from [building.md](building.md) plus `dpkg-dev`:

```bash
scripts/package-linux.sh
```

Output in `target/packages/`:

| File | Purpose |
|---|---|
| `mixercraft_<version>_<amd64\|arm64>.deb` | Debian/Ubuntu package: programs in `/usr/bin`, launcher, icons; dependencies from `dpkg-shlibdeps` |
| `mixercraft-<version>-linux-<x86_64\|aarch64>.tar.gz` | portable archive; `./install.sh` installs to `~/.local` (no root), `--prefix DIR`, `--uninstall` |
| `*.sha256` | checksums (`sha256sum -c`) |

Options: `--version X.Y.Z`, `--feed <url>` (compiled-in update feed),
`--skip-cargo`. Release packages are built in a Debian 12 container, so they
run on distributions with glibc 2.36 and PipeWire 0.3.49 or newer (Debian 12,
Ubuntu 24.04, Fedora 37 and later; Ubuntu 22.04's PipeWire is too old for the
virtual devices). There is no AppImage: autostart
entries and the engine the app starts would point into the AppImage's
temporary mount, which disappears when the app exits.

The `.deb` and the archive do not register autostart; the app's Settings tab
or `mixercraft autostart enable` does, per user. Remove that registration
(`mixercraft autostart disable`) before uninstalling.

## macOS: build the disk image

With Xcode command-line tools and both Rust targets
(`rustup target add aarch64-apple-darwin x86_64-apple-darwin`):

```bash
scripts/package-macos.sh            # universal (Apple silicon + Intel)
scripts/package-macos.sh --native-only   # this Mac's architecture only, faster
```

Output: `target/packages/MixerCraft-<version>-macos-universal.dmg` and its
`.sha256`. The bundle holds all three programs in `Contents/MacOS` (the app
starts the engine next to itself; the CLI is
`/Applications/MixerCraft.app/Contents/MacOS/mixercraft`). Its Info.plist and
the engine's embedded Info.plist carry the microphone usage text, so macOS
asks for microphone access instead of delivering silence.

**Signing:** the bundle is only ad-hoc signed — there is no paid Apple
Developer ID (zero recurring cost, ADR 0006), so it is not notarized.
Gatekeeper blocks the first start of a downloaded copy: open it once, then
**System Settings → Privacy & Security → "MixerCraft was blocked…" → Open
Anyway** (macOS 15 removed the right-click → Open shortcut). Alternatively
`xattr -dr com.apple.quarantine /Applications/MixerCraft.app`. Because the
ad-hoc signature changes with every build, macOS may ask for microphone
access again after an update.

## Release workflow (Linux and macOS)

`.github/workflows/release.yml` builds and smoke-tests the Linux packages
(x86_64 and ARM64) and the macOS disk image: it installs and removes the
`.deb`, runs `install.sh` into a temporary prefix, mounts the disk image,
checks signatures, architectures and the embedded Info.plist, and runs the
programs' `--version`. On pull requests that touch packaging it is a dry run
(packages kept as workflow artifacts for 14 days). Started by hand (Actions →
Release packages → Run workflow) with a release tag, it builds from that tag
and attaches the packages to the existing GitHub release of that tag. It
never runs on a push to `main`.

## Publish a release

1. Bump `version` in the workspace `Cargo.toml`, update `CHANGELOG.md`.
2. Build the Windows installer (the default `-Feed` is
   `https://github.com/JustinSchweiger/MixerCraft/releases/latest/download/latest.json`,
   ADR 0006), optionally with `-Notes "…" -NotesUrl https://github.com/JustinSchweiger/MixerCraft/releases/tag/v<version>`.
   Without `-NotesUrl`, apps reading a GitHub feed link to
   `…/releases/latest` instead.
3. On GitHub, draft a release with tag `v<version>`, attach the `.msi`,
   `.msi.sha256` and `latest.json`.
4. Run the **Release packages** workflow with the tag `v<version>`; it
   attaches the `.deb`, `.tar.gz` and `.dmg` files (and their `.sha256`) to
   the draft.
5. Publish the release as the latest release. Clients follow
   `releases/latest`, so a release only reaches them once it is published
   (drafts and pre-releases are invisible to them). `latest.json` only lists
   the MSI: Linux and macOS apps show the new version and link to the release
   page (notify only, ADR 0006).

## Test the updater with a local feed

Any folder works as a feed:

```powershell
# a newer "release" from the current binaries
powershell -ExecutionPolicy Bypass -File scripts\build-installer.ps1 -SkipCargo -Version 9.9.9
mixercraft update --feed file:///E:/MixerCraft/target/installer/latest.json --download
```

For the app, set the feed in `%APPDATA%\MixerCraft\config\updates.toml`:

```toml
check_automatically = true
feed_url = "file:///E:/MixerCraft/target/installer/latest.json"
```

The About tab then shows "Version 9.9.9 is available"; "Download and install"
appears only in a copy installed by the MSI. The installer log of an in-app
update is `%LOCALAPPDATA%\MixerCraft\data\updates\install.log`.
