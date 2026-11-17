#!/usr/bin/env bash
# Builds the MixerCraft Linux packages for the machine's architecture:
#
#   target/packages/mixercraft_<version>_<deb-arch>.deb      Debian/Ubuntu package
#   target/packages/mixercraft-<version>-linux-<arch>.tar.gz portable archive with install.sh
#   *.sha256                                                 checksums (sha256sum format)
#
# Needs Rust, the build dependencies from docs/development/building.md,
# dpkg-deb and dpkg-shlibdeps (package dpkg-dev). Nothing is signed.
# See docs/development/releasing.md.
#
# Options:
#   --version X.Y.Z   package version (default: workspace version in Cargo.toml)
#   --feed URL        update feed compiled into the app (MIXERCRAFT_UPDATE_FEED);
#                     "" for none. Default: the latest GitHub release.
#   --skip-cargo      package the existing target/release binaries
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"

feed="https://github.com/JustinSchweiger/MixerCraft/releases/latest/download/latest.json"
version=""
skip_cargo=0
while [ $# -gt 0 ]; do
    case "$1" in
        --version) version="$2"; shift 2 ;;
        --feed) feed="$2"; shift 2 ;;
        --skip-cargo) skip_cargo=1; shift ;;
        -h|--help) sed -n '2,17p' "$0"; exit 0 ;;
        *) echo "unknown option: $1" >&2; exit 2 ;;
    esac
done

cargo_version="$(awk '/^\[workspace\.package\]/{w=1;next} /^\[/{w=0} w && /^version *=/{gsub(/.*= *"|".*/,""); print; exit}' Cargo.toml)"
if [ -z "$version" ]; then
    version="$cargo_version"
elif [ "$version" != "$cargo_version" ]; then
    echo "warning: package version $version differs from the programs' version $cargo_version (Cargo.toml); only use this for tests" >&2
fi
if ! [[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
    echo "version '$version' must be major.minor.patch" >&2
    exit 2
fi

arch="$(uname -m)"
case "$arch" in
    x86_64) deb_arch=amd64 ;;
    aarch64|arm64) arch=aarch64; deb_arch=arm64 ;;
    *) echo "unsupported architecture $arch" >&2; exit 2 ;;
esac

bins=(mixercraft-gui mixercraftd mixercraft)
if [ "$skip_cargo" -eq 0 ]; then
    MIXERCRAFT_UPDATE_FEED="$feed" cargo build --release --locked \
        -p mixercraft-gui -p mixercraft-engine -p mixercraft-cli
fi
for b in "${bins[@]}"; do
    [ -x "target/release/$b" ] || { echo "target/release/$b is missing (build first or drop --skip-cargo)" >&2; exit 1; }
done

out="$root/target/packages"
work="$out/work-linux"
rm -rf "$work"
mkdir -p "$work" "$out"

# Installs the program files into <dir> with the layout of <prefix>
# (bin/, share/applications/, share/icons/, share/doc/mixercraft/).
stage() {
    local dir="$1"
    install -d "$dir/bin" "$dir/share/applications" "$dir/share/doc/mixercraft"
    for b in "${bins[@]}"; do
        install -m 0755 "target/release/$b" "$dir/bin/$b"
        # Line tables stay in the build; packages carry symbols only.
        strip --strip-debug "$dir/bin/$b"
    done
    install -m 0644 assets/linux/mixercraft.desktop "$dir/share/applications/mixercraft.desktop"
    (cd assets/linux && find hicolor -name '*.png') | while read -r icon; do
        install -D -m 0644 "assets/linux/$icon" "$dir/share/icons/$icon"
    done
    install -m 0644 LICENSE-MIT LICENSE-APACHE README.md CHANGELOG.md "$dir/share/doc/mixercraft/"
}

# --- .deb -----------------------------------------------------------------
deb_root="$work/deb"
stage "$deb_root/usr"
cat > "$deb_root/usr/share/doc/mixercraft/copyright" <<EOF
Format: https://www.debian.org/doc/packaging-manuals/copyright-format/1.0/
Upstream-Name: MixerCraft
Source: https://github.com/JustinSchweiger/MixerCraft

Files: *
Copyright: MixerCraft contributors
License: MIT or Apache-2.0
 See /usr/share/doc/mixercraft/LICENSE-MIT and LICENSE-APACHE.
EOF

# Shared-library dependencies of the binaries, as Debian package names.
shlib_dir="$work/shlibdeps"
mkdir -p "$shlib_dir/debian"
printf 'Source: mixercraft\n\nPackage: mixercraft\nArchitecture: any\n' > "$shlib_dir/debian/control"
depends="$(cd "$shlib_dir" && dpkg-shlibdeps -O "${bins[@]/#/$deb_root/usr/bin/}" 2>/dev/null | sed -n 's/^shlibs:Depends=//p')"
[ -n "$depends" ] || { echo "dpkg-shlibdeps found no dependencies" >&2; exit 1; }

installed_kb="$(du -sk "$deb_root/usr" | cut -f1)"
install -d "$deb_root/DEBIAN"
cat > "$deb_root/DEBIAN/control" <<EOF
Package: mixercraft
Version: $version
Architecture: $deb_arch
Maintainer: MixerCraft contributors <https://github.com/JustinSchweiger/MixerCraft/issues>
Installed-Size: $installed_kb
Depends: $depends
Recommends: pipewire-alsa | pulseaudio
Section: sound
Priority: optional
Homepage: https://github.com/JustinSchweiger/MixerCraft
Description: Audio mixer and router (Voicemeeter alternative)
 MixerCraft mixes and routes audio between input and output devices with
 independent clocks: gain, mute, solo, gate, compressor, EQ and noise
 suppression per input, several output buses and live meters.
 .
 The package contains the desktop app (mixercraft-gui), the background
 engine (mixercraftd) and the command-line client (mixercraft).
EOF
deb="$out/mixercraft_${version}_${deb_arch}.deb"
dpkg-deb --root-owner-group -Zxz --build "$deb_root" "$deb" >/dev/null

# --- portable .tar.gz -----------------------------------------------------
name="mixercraft-$version-linux-$arch"
tar_root="$work/$name"
stage "$tar_root"
install -m 0755 "$root/scripts/linux-install.sh" "$tar_root/install.sh"
tarball="$out/$name.tar.gz"
tar -C "$work" --owner=0 --group=0 -czf "$tarball" "$name"

# --- checksums --------------------------------------------------------------
for f in "$deb" "$tarball"; do
    (cd "$out" && sha256sum "$(basename "$f")" > "$(basename "$f").sha256")
done
rm -rf "$work"

echo "Package:   $deb"
echo "Archive:   $tarball"
echo "Depends:   $depends"
