# Assets

Icons and desktop integration files, generated from the master icon
`crates/mixercraft-gui/ui/icon.png` (256×256, also the window and tray icon)
by `python assets/generate-icons.py` (needs Pillow). Regenerate and commit
all outputs whenever the master icon changes.

| Path | Used by |
|---|---|
| `windows/mixercraft.ico` | Embedded into `mixercraft-gui.exe`, `mixercraftd.exe` and `mixercraft.exe` at build time (16–64 px BMP + 256 px PNG) |
| `windows/resources.rs` | Build-script helper (`include!`d by the three `build.rs`) that embeds the icon and version info via `winresource` |
| `linux/mixercraft.desktop` | Application launcher entry; packages install it to `share/applications/` |
| `linux/hicolor/<n>x<n>/apps/mixercraft.png` | Icon theme files; packages install them to `share/icons/hicolor/` |
| `macos/mixercraft.icns` | App bundle icon (`CFBundleIconFile`) for the future macOS bundle |

No package or bundle installs the Linux and macOS files yet (see
`docs/feature-matrix.md`, "Installers").
