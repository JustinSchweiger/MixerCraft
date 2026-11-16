#!/usr/bin/env python3
"""Regenerates the platform icon files from the master PNG.

Source: crates/mixercraft-gui/ui/icon.png (256x256 RGBA; also the window and
tray icon). Outputs, all committed:

* assets/windows/mixercraft.ico  16/20/24/32/40/48/64 px as 32-bit BMP
  entries, 256 px as PNG (the layout Explorer and rc.exe expect)
* assets/macos/mixercraft.icns   16/32/64/128/256 px PNG entries
* assets/linux/hicolor/<n>x<n>/apps/mixercraft.png  16..256 px

Needs Pillow (`pip install pillow`). Run from anywhere:
    python assets/generate-icons.py
"""

import io
import struct
from pathlib import Path

from PIL import Image

ROOT = Path(__file__).resolve().parent.parent
SOURCE = ROOT / "crates" / "mixercraft-gui" / "ui" / "icon.png"
ASSETS = ROOT / "assets"

ICO_SIZES = [16, 20, 24, 32, 40, 48, 64, 256]
ICNS_TYPES = [(16, b"icp4"), (32, b"icp5"), (64, b"icp6"), (128, b"ic07"), (256, b"ic08")]
LINUX_SIZES = [16, 24, 32, 48, 64, 128, 256]


def scaled(master: Image.Image, size: int) -> Image.Image:
    if master.size == (size, size):
        return master.copy()
    return master.resize((size, size), Image.Resampling.LANCZOS)


def png_bytes(img: Image.Image) -> bytes:
    buf = io.BytesIO()
    img.save(buf, format="PNG", optimize=True)
    return buf.getvalue()


def dib_bytes(img: Image.Image) -> bytes:
    """32-bit BGRA DIB with an all-zero AND mask (alpha does the masking)."""
    w, h = img.size
    header = struct.pack("<IiiHHIIiiII", 40, w, h * 2, 1, 32, 0, 0, 0, 0, 0, 0)
    rgba = img.tobytes()
    rows = []
    for y in range(h - 1, -1, -1):  # bottom-up
        row = rgba[y * w * 4 : (y + 1) * w * 4]
        bgra = bytearray(row)
        bgra[0::4], bgra[2::4] = row[2::4], row[0::4]
        rows.append(bytes(bgra))
    mask_stride = ((w + 31) // 32) * 4
    return header + b"".join(rows) + bytes(mask_stride * h)


def write_ico(master: Image.Image, path: Path) -> None:
    images = []
    for size in ICO_SIZES:
        img = scaled(master, size)
        images.append((size, png_bytes(img) if size >= 256 else dib_bytes(img)))
    out = bytearray(struct.pack("<HHH", 0, 1, len(images)))
    offset = 6 + 16 * len(images)
    for size, data in images:
        dim = 0 if size >= 256 else size
        out += struct.pack("<BBBBHHII", dim, dim, 0, 0, 1, 32, len(data), offset)
        offset += len(data)
    for _, data in images:
        out += data
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(bytes(out))


def write_icns(master: Image.Image, path: Path) -> None:
    body = b""
    for size, kind in ICNS_TYPES:
        data = png_bytes(scaled(master, size))
        body += kind + struct.pack(">I", 8 + len(data)) + data
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(b"icns" + struct.pack(">I", 8 + len(body)) + body)


def write_linux(master: Image.Image, base: Path) -> None:
    for size in LINUX_SIZES:
        path = base / f"{size}x{size}" / "apps" / "mixercraft.png"
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(png_bytes(scaled(master, size)))


def main() -> None:
    master = Image.open(SOURCE).convert("RGBA")
    if master.size != (256, 256):
        raise SystemExit(f"{SOURCE} must be 256x256, is {master.size}")
    write_ico(master, ASSETS / "windows" / "mixercraft.ico")
    write_icns(master, ASSETS / "macos" / "mixercraft.icns")
    write_linux(master, ASSETS / "linux" / "hicolor")
    print("icons written under", ASSETS)


if __name__ == "__main__":
    main()
