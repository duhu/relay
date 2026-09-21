#!/usr/bin/env python3
"""Generate Relay's app icon and menu bar template icons.

Pure standard library: PNGs are written with zlib + struct, shapes are drawn
from signed distance fields so the edges are antialiased without any image
library. Run from anywhere:

    python3 scripts/gen-icons.py

It writes src-tauri/icons/source.png (1024x1024 app icon source), hands it to
`pnpm tauri icon` to produce the platform icon set, and writes the three
44x44 black-on-transparent menu bar template icons.
"""

from __future__ import annotations

import math
import shutil
import struct
import subprocess
import sys
import zlib
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
ICONS = ROOT / "src-tauri" / "icons"


# --- PNG output -------------------------------------------------------------


def write_png(path: Path, width: int, height: int, rgba: bytearray) -> None:
    """Write RGBA8 pixels as a non-interlaced PNG."""

    raw = bytearray()
    stride = width * 4
    for y in range(height):
        raw.append(0)  # filter type 0 (None)
        raw += rgba[y * stride : (y + 1) * stride]

    def chunk(kind: bytes, data: bytes) -> bytes:
        return (
            struct.pack(">I", len(data))
            + kind
            + data
            + struct.pack(">I", zlib.crc32(kind + data) & 0xFFFFFFFF)
        )

    png = b"\x89PNG\r\n\x1a\n"
    png += chunk(b"IHDR", struct.pack(">IIBBBBB", width, height, 8, 6, 0, 0, 0))
    png += chunk(b"IDAT", zlib.compress(bytes(raw), 9))
    png += chunk(b"IEND", b"")
    path.write_bytes(png)


# --- signed distance fields (units: pixels, negative = inside) ---------------


def sd_rounded_rect(px, py, cx, cy, half_w, half_h, radius):
    dx = abs(px - cx) - (half_w - radius)
    dy = abs(py - cy) - (half_h - radius)
    outside = math.hypot(max(dx, 0.0), max(dy, 0.0))
    return outside + min(max(dx, dy), 0.0) - radius


def sd_circle(px, py, cx, cy, radius):
    return math.hypot(px - cx, py - cy) - radius


def sd_ring(px, py, cx, cy, radius, half_width):
    return abs(sd_circle(px, py, cx, cy, radius)) - half_width


def sd_capsule(px, py, ax, ay, bx, by, radius):
    pax, pay = px - ax, py - ay
    bax, bay = bx - ax, by - ay
    denom = bax * bax + bay * bay
    t = 0.0 if denom == 0 else max(0.0, min(1.0, (pax * bax + pay * bay) / denom))
    return math.hypot(pax - bax * t, pay - bay * t) - radius


def coverage(distance: float) -> float:
    """Antialiased inside-ness of a pixel centre at `distance` from an edge."""

    return max(0.0, min(1.0, 0.5 - distance))


# --- compositing ------------------------------------------------------------


def blend(dst: bytearray, offset: int, rgb, alpha: float) -> None:
    if alpha <= 0:
        return
    da = dst[offset + 3] / 255.0
    out_a = alpha + da * (1.0 - alpha)
    if out_a <= 0:
        return
    for i in range(3):
        src = rgb[i] * alpha
        old = dst[offset + i] * da * (1.0 - alpha)
        dst[offset + i] = int(round((src + old) / out_a))
    dst[offset + 3] = int(round(out_a * 255))


def draw(size: int, layers) -> bytearray:
    """Rasterise `layers` = [(rgb, sdf(px, py) -> distance), ...]."""

    buf = bytearray(size * size * 4)
    for y in range(size):
        py = y + 0.5
        row = y * size * 4
        for x in range(size):
            px = x + 0.5
            for rgb, sdf in layers:
                blend(buf, row + x * 4, rgb, coverage(sdf(px, py)))
    return buf


def arrow(x0, y0, x1, y1, thickness, head):
    """Capsules forming a straight arrow from (x0, y0) to (x1, y1)."""

    angle = math.atan2(y1 - y0, x1 - x0)
    r = thickness / 2.0
    wings = []
    for sign in (-1, 1):
        a = angle + sign * math.radians(140)
        wings.append((x1 + head * math.cos(a), y1 + head * math.sin(a)))
    parts = [(x0, y0, x1, y1)]
    parts += [(x1, y1, wx, wy) for wx, wy in wings]
    return [
        (lambda p, q, seg=seg: sd_capsule(p, q, seg[0], seg[1], seg[2], seg[3], r))
        for seg in parts
    ]


# --- icons ------------------------------------------------------------------


def make_source(size: int = 1024) -> None:
    """Rounded blue square with two arrows handing off in opposite directions."""

    bg = (43, 108, 240)
    fg = (255, 255, 255)
    layers = [
        (bg, lambda p, q: sd_rounded_rect(p, q, size / 2, size / 2, size / 2, size / 2, size * 0.22)),
    ]
    thickness = size * 0.075
    head = size * 0.14
    for sdf in arrow(size * 0.26, size * 0.38, size * 0.74, size * 0.38, thickness, head):
        layers.append((fg, sdf))
    for sdf in arrow(size * 0.74, size * 0.62, size * 0.26, size * 0.62, thickness, head):
        layers.append((fg, sdf))
    write_png(ICONS / "source.png", size, size, draw(size, layers))


def make_tray_icons(size: int = 44) -> None:
    """Black + alpha template images; macOS recolours them for the menu bar."""

    black = (0, 0, 0)
    c = size / 2.0

    idle = [(black, lambda p, q: sd_ring(p, q, c, c, size * 0.32, size * 0.055))]
    switching = [(black, lambda p, q: sd_circle(p, q, c, c, size * 0.36))]
    attention = [
        (black, lambda p, q: sd_ring(p, q, c, c, size * 0.25, size * 0.055)),
        (black, lambda p, q: sd_circle(p, q, size * 0.82, size * 0.18, size * 0.12)),
    ]
    for name, layers in (
        ("tray-idle", idle),
        ("tray-switching", switching),
        ("tray-attention", attention),
    ):
        write_png(ICONS / f"{name}.png", size, size, draw(size, layers))


def run_tauri_icon() -> None:
    source = ICONS / "source.png"
    try:
        subprocess.run(
            ["pnpm", "tauri", "icon", str(source.relative_to(ROOT))],
            cwd=ROOT,
            check=True,
        )
    except (OSError, subprocess.CalledProcessError) as err:
        print(f"warning: `pnpm tauri icon` failed ({err}); run it manually", file=sys.stderr)
        return
    prune_foreign_platforms()


def prune_foreign_platforms() -> None:
    """Relay is macOS only; drop the iOS/Android/Windows icons tauri emits."""

    for path in list(ICONS.glob("Square*Logo.png")) + [ICONS / "StoreLogo.png"]:
        path.unlink(missing_ok=True)
    for name in ("ios", "android"):
        directory = ICONS / name
        if directory.is_dir():
            shutil.rmtree(directory)


def main() -> None:
    ICONS.mkdir(parents=True, exist_ok=True)
    make_source()
    run_tauri_icon()
    make_tray_icons()
    print(f"icons written to {ICONS}")


if __name__ == "__main__":
    main()
