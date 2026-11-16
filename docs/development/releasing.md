# Windows installer and releases

Decisions: [ADR 0006](../adr/0006-distribution-and-updates.md). Everything here
is free tooling and runs without administrator rights.

## Build the installer

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
  Windows (tray app via `HKCU\…\Run`, engine via the "MixerCraft Engine"
  scheduled task); the install folder on the user `PATH`.
* Checks for usbip-win2 (`%ProgramFiles%\USBip\usbip.exe`); if missing, the
  last page explains that virtual devices need it and where to get it
  (<https://github.com/vadimgrn/usbip-win2/releases>). Drivers are never
  bundled or installed.
* Upgrade: runs the installed `mixercraft close-app` (asks the app to quit and
  stops this installation's engine; an engine started from another folder,
  such as a development build, is left alone), removes the old version,
  installs the new one. The configuration in `%APPDATA%\MixerCraft` is kept.
* Uninstall: closes the app, removes the autostart task if it starts this
  installation's engine, removes the files, shortcuts, `Run` entry and `PATH`
  entry. Configuration and logs stay (delete `%APPDATA%\MixerCraft` and
  `%LOCALAPPDATA%\MixerCraft` to remove them).

Unattended: `msiexec /i MixerCraft-0.2.0-x64.msi /passive` (all default
features), `ADDLOCAL=Main,Autostart` to choose features, `/x` to uninstall.

## Publish a release

1. Bump `version` in the workspace `Cargo.toml`, update `CHANGELOG.md`.
2. Build (the default `-Feed` is
   `https://github.com/JustinSchweiger/MixerCraft/releases/latest/download/latest.json`,
   ADR 0006), optionally with `-Notes "…" -NotesUrl https://github.com/JustinSchweiger/MixerCraft/releases/tag/v<version>`.
3. On GitHub, draft a release with tag `v<version>`, attach the `.msi`,
   `.msi.sha256` and `latest.json`, and publish it as the latest release.
   Clients follow `releases/latest`, so a release only reaches them once it is
   published (drafts and pre-releases are invisible to them).

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
