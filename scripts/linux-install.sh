#!/usr/bin/env sh
# Installs MixerCraft from this folder for the current user (no root needed).
#
#   ./install.sh                  install to ~/.local
#   ./install.sh --prefix DIR     install to DIR (bin/, share/)
#   ./install.sh --uninstall      remove the installed files again
#
# Your configuration (~/.config/MixerCraft) is never touched. Before
# uninstalling, turn off "start with the session" (`mixercraft autostart
# disable`) if you enabled it.
set -eu

here="$(cd "$(dirname "$0")" && pwd)"
prefix="${HOME:?HOME is not set}/.local"
uninstall=0
while [ $# -gt 0 ]; do
    case "$1" in
        --prefix) prefix="$2"; shift 2 ;;
        --uninstall) uninstall=1; shift ;;
        -h|--help) sed -n '2,11p' "$0"; exit 0 ;;
        *) echo "unknown option: $1" >&2; exit 2 ;;
    esac
done

bins="mixercraft-gui mixercraftd mixercraft"
icons="$(cd "$here/share/icons" && find hicolor -name 'mixercraft.png')"

if [ "$uninstall" -eq 1 ]; then
    for b in $bins; do rm -f "$prefix/bin/$b"; done
    rm -f "$prefix/share/applications/mixercraft.desktop"
    for i in $icons; do rm -f "$prefix/share/icons/$i"; done
    rm -rf "$prefix/share/doc/mixercraft"
    echo "MixerCraft removed from $prefix."
    exit 0
fi

# A running engine keeps working: replacing a file does not touch the
# program already in memory. Restart the engine to use the new version.
mkdir -p "$prefix/bin" "$prefix/share/applications" "$prefix/share/doc/mixercraft"
for b in $bins; do
    cp "$here/bin/$b" "$prefix/bin/$b.new"
    chmod 0755 "$prefix/bin/$b.new"
    mv -f "$prefix/bin/$b.new" "$prefix/bin/$b"
done
for i in $icons; do
    mkdir -p "$(dirname "$prefix/share/icons/$i")"
    cp "$here/share/icons/$i" "$prefix/share/icons/$i"
done
cp "$here/share/doc/mixercraft/"* "$prefix/share/doc/mixercraft/"
# Desktop sessions often do not have ~/.local/bin on PATH: use the full path.
sed "s|^Exec=mixercraft-gui|Exec=\"$prefix/bin/mixercraft-gui\"|" \
    "$here/share/applications/mixercraft.desktop" > "$prefix/share/applications/mixercraft.desktop"
# No icon or desktop caches are written: desktops scan these folders, and a
# cache in the user's folders would go stale when other programs add files.

echo "MixerCraft installed to $prefix."
case ":${PATH:-}:" in
    *":$prefix/bin:"*) ;;
    *) echo "Note: $prefix/bin is not on your PATH; the command-line client is $prefix/bin/mixercraft." ;;
esac
