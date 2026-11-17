#!/usr/bin/env bash
# Builds the MixerCraft macOS app bundle and disk image (universal: Apple
# silicon and Intel):
#
#   target/packages/MixerCraft-<version>-macos-universal.dmg   (+ .sha256)
#
# The app is ad-hoc signed only (no paid Apple Developer ID, see ADR 0006):
# Gatekeeper blocks the first start until the user allows it in System
# Settings → Privacy & Security ("Open Anyway"). Needs Xcode command-line
# tools and the Rust targets aarch64-apple-darwin and x86_64-apple-darwin.
# See docs/development/releasing.md.
#
# Options:
#   --version X.Y.Z   bundle version (default: workspace version in Cargo.toml)
#   --feed URL        update feed compiled into the app; "" for none.
#                     Default: the latest GitHub release.
#   --native-only     build only for this Mac's architecture (faster, for tests)
#   --skip-cargo      package the existing builds
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"

feed="https://github.com/JustinSchweiger/MixerCraft/releases/latest/download/latest.json"
version=""
skip_cargo=0
native_only=0
while [ $# -gt 0 ]; do
    case "$1" in
        --version) version="$2"; shift 2 ;;
        --feed) feed="$2"; shift 2 ;;
        --native-only) native_only=1; shift ;;
        --skip-cargo) skip_cargo=1; shift ;;
        -h|--help) sed -n '2,18p' "$0"; exit 0 ;;
        *) echo "unknown option: $1" >&2; exit 2 ;;
    esac
done

cargo_version="$(awk '/^\[workspace\.package\]/{w=1;next} /^\[/{w=0} w && /^version *=/{gsub(/.*= *"|".*/,""); print; exit}' Cargo.toml)"
if [ -z "$version" ]; then
    version="$cargo_version"
elif [ "$version" != "$cargo_version" ]; then
    echo "warning: bundle version $version differs from the programs' version $cargo_version (Cargo.toml); only use this for tests" >&2
fi
if ! [[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
    echo "version '$version' must be major.minor.patch" >&2
    exit 2
fi

if [ "$native_only" -eq 1 ]; then
    case "$(uname -m)" in
        arm64) targets=(aarch64-apple-darwin); flavour=arm64 ;;
        *) targets=(x86_64-apple-darwin); flavour=x86_64 ;;
    esac
else
    targets=(aarch64-apple-darwin x86_64-apple-darwin)
    flavour=universal
fi
bins=(mixercraft-gui mixercraftd mixercraft)
# Oldest macOS the binaries run on (winit and Core Audio taps aside, 11 is
# the first release with Apple silicon).
export MACOSX_DEPLOYMENT_TARGET=11.0

if [ "$skip_cargo" -eq 0 ]; then
    for t in "${targets[@]}"; do
        MIXERCRAFT_UPDATE_FEED="$feed" cargo build --release --locked --target "$t" \
            -p mixercraft-gui -p mixercraft-engine -p mixercraft-cli
    done
fi

out="$root/target/packages"
work="$out/work-macos"
rm -rf "$work"
mkdir -p "$work" "$out"

app="$work/MixerCraft.app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
for b in "${bins[@]}"; do
    inputs=()
    for t in "${targets[@]}"; do
        f="target/$t/release/$b"
        [ -x "$f" ] || { echo "$f is missing (build first or drop --skip-cargo)" >&2; exit 1; }
        inputs+=("$f")
    done
    lipo -create -output "$app/Contents/MacOS/$b" "${inputs[@]}"
done
cp assets/macos/mixercraft.icns "$app/Contents/Resources/mixercraft.icns"
cp LICENSE-MIT LICENSE-APACHE "$app/Contents/Resources/"

cat > "$app/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleDevelopmentRegion</key>
  <string>en</string>
  <key>CFBundleDisplayName</key>
  <string>MixerCraft</string>
  <key>CFBundleExecutable</key>
  <string>mixercraft-gui</string>
  <key>CFBundleIconFile</key>
  <string>mixercraft</string>
  <key>CFBundleIdentifier</key>
  <string>org.mixercraft.MixerCraft</string>
  <key>CFBundleInfoDictionaryVersion</key>
  <string>6.0</string>
  <key>CFBundleName</key>
  <string>MixerCraft</string>
  <key>CFBundlePackageType</key>
  <string>APPL</string>
  <key>CFBundleShortVersionString</key>
  <string>$version</string>
  <key>CFBundleVersion</key>
  <string>$version</string>
  <key>LSApplicationCategoryType</key>
  <string>public.app-category.music</string>
  <key>LSMinimumSystemVersion</key>
  <string>$MACOSX_DEPLOYMENT_TARGET</string>
  <key>NSHighResolutionCapable</key>
  <true/>
  <key>NSHumanReadableCopyright</key>
  <string>MixerCraft contributors, MIT OR Apache-2.0</string>
  <key>NSMicrophoneUsageDescription</key>
  <string>MixerCraft mixes and routes the audio of your input devices.</string>
</dict>
</plist>
EOF
plutil -lint "$app/Contents/Info.plist" >/dev/null

# Ad-hoc signatures ("-"): required for arm64 code and keeps the bundle
# sealed; not a Developer ID, so Gatekeeper still asks once. Helpers first,
# with the identifiers macOS uses for their permissions, then the bundle.
codesign --force --sign - --identifier org.mixercraft.engine "$app/Contents/MacOS/mixercraftd"
codesign --force --sign - --identifier org.mixercraft.cli "$app/Contents/MacOS/mixercraft"
codesign --force --sign - "$app"
codesign --verify --strict --verbose=1 "$app"

dmg_src="$work/dmg"
mkdir -p "$dmg_src"
mv "$app" "$dmg_src/"
ln -s /Applications "$dmg_src/Applications"
dmg="$out/MixerCraft-$version-macos-$flavour.dmg"
rm -f "$dmg"
hdiutil create -quiet -volname "MixerCraft $version" -srcfolder "$dmg_src" -fs HFS+ -format UDZO -ov "$dmg"
(cd "$out" && shasum -a 256 "$(basename "$dmg")" > "$(basename "$dmg").sha256")
rm -rf "$work"

echo "Disk image: $dmg"
