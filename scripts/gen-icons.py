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


def sd_triangle(px, py, a, b, c):
    """Signed distance to the triangle a-b-c (any winding)."""

    def seg(p, q):  # squared distance from (px, py) to segment p-q, plus the side sign
        ex, ey = q[0] - p[0], q[1] - p[1]
        wx, wy = px - p[0], py - p[1]
        t = max(0.0, min(1.0, (wx * ex + wy * ey) / (ex * ex + ey * ey)))
        dx, dy = wx - ex * t, wy - ey * t
        return dx * dx + dy * dy, ex * wy - ey * wx

    d0, s0 = seg(a, b)
    d1, s1 = seg(b, c)
    d2, s2 = seg(c, a)
    d = math.sqrt(min(d0, d1, d2))
    inside = (s0 >= 0 and s1 >= 0 and s2 >= 0) or (s0 <= 0 and s1 <= 0 and s2 <= 0)
    return -d if inside else d


def quad_bezier_stroke(p0, c, p1, radius, segments=48):
    """A stroked quadratic Bézier with round caps, as one SDF.

    The curve is approximated by `segments` capsules; their union (min) is a
    single shape, so the joints are not blended twice at the antialiased edge.
    """

    pts = []
    for i in range(segments + 1):
        t = i / segments
        u = 1.0 - t
        pts.append(
            (
                u * u * p0[0] + 2 * u * t * c[0] + t * t * p1[0],
                u * u * p0[1] + 2 * u * t * c[1] + t * t * p1[1],
            )
        )
    segs = [(pts[i][0], pts[i][1], pts[i + 1][0], pts[i + 1][1]) for i in range(segments)]

    def sdf(px, py):
        return min(sd_capsule(px, py, ax, ay, bx, by, radius) for ax, ay, bx, by in segs)

    return sdf


def arrow_head(tip, angle, length, front=0.55, half_width=0.475):
    """Filled triangle anchored at `tip`, pointing along `angle` (radians).

    Local shape, before rotation: (front·L, 0), (-0.45L, ±half_width·L),
    translated so the local origin sits at `tip` (the canvas mock-ups'
    translate + rotate).
    """

    ca, sa = math.cos(angle), math.sin(angle)

    def at(lx, ly):
        return (tip[0] + lx * ca - ly * sa, tip[1] + lx * sa + ly * ca)

    a = at(front * length, 0)
    b = at(-0.45 * length, -half_width * length)
    c = at(-0.45 * length, half_width * length)
    return lambda p, q: sd_triangle(p, q, a, b, c)


def sd_outline(sdf, half_width):
    """The stroke of a shape's edge."""

    return lambda p, q: abs(sdf(p, q)) - half_width


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


def draw(size: int, layers, tile: int = 16) -> bytearray:
    """Rasterise `layers` in order onto a transparent canvas.

    A layer is `(colour, sdf)`, `(colour, sdf, alpha)` or `("erase", sdf)`.
    `colour` is an `(r, g, b)` tuple or a function `(px, py) -> (r, g, b)`;
    `alpha` is a constant opacity; an erase layer scales the alpha it covers
    down (pixels are straight RGBA, so the colour is left alone).

    Layers are drawn one after another, tile by tile: every SDF here is
    1-Lipschitz, so a tile whose centre is farther from the shape than the
    tile's half-diagonal cannot be touched and is skipped.
    """

    buf = bytearray(size * size * 4)
    for layer in layers:
        colour, sdf = layer[0], layer[1]
        alpha = layer[2] if len(layer) > 2 else 1.0
        erase = isinstance(colour, str)
        if erase and colour != "erase":
            raise ValueError(f"unknown layer kind {colour!r}")
        varying = callable(colour)
        for ty in range(0, size, tile):
            th = min(tile, size - ty)
            for tx in range(0, size, tile):
                tw = min(tile, size - tx)
                reach = math.hypot(tw, th) / 2.0 + 0.5
                if sdf(tx + tw / 2.0, ty + th / 2.0) > reach:
                    continue
                for y in range(ty, ty + th):
                    py = y + 0.5
                    row = y * size * 4
                    for x in range(tx, tx + tw):
                        px = x + 0.5
                        cov = coverage(sdf(px, py))
                        if cov <= 0:
                            continue
                        offset = row + x * 4
                        if erase:
                            dst = buf[offset + 3]
                            buf[offset + 3] = int(round(dst * (1.0 - cov)))
                        else:
                            rgb = colour(px, py) if varying else colour
                            blend(buf, offset, rgb, cov * alpha)
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


def rr(x, y, w, h, r):
    """Rounded rect by its top-left corner, like canvas `roundRect`."""

    return lambda p, q: sd_rounded_rect(p, q, x + w / 2, y + h / 2, w / 2, h / 2, r)


def capsule(a, b, radius):
    return lambda p, q: sd_capsule(p, q, a[0], a[1], b[0], b[1], radius)


def make_source(size: int = 1024) -> None:
    """Two monitors on a violet gradient, a white arc jumping to the right one."""

    S = size
    hw = S * 0.07 / 2
    white = (255, 255, 255)
    start, stop = (90, 92, 255), (177, 76, 243)  # #5a5cff -> #b14cf3, top-left to bottom-right

    def gradient(px, py):
        t = max(0.0, min(1.0, (px + py) / (2 * S)))
        return tuple(int(round(a + (b - a) * t)) for a, b in zip(start, stop))

    layers = [(gradient, rr(0, 0, S, S, S * 0.225))]
    for x in (S * 0.21, S * 0.59):
        screen = rr(x, S * 0.50, S * 0.20, S * 0.15, S * 0.025)
        layers.append((white, sd_outline(screen, hw), 0.55))
        layers.append((white, capsule((x + S * 0.06, S * 0.72), (x + S * 0.14, S * 0.72), hw), 0.55))
    # the target screen is lit; the outer half of its stroke stays translucent
    layers.append((white, rr(S * 0.59, S * 0.50, S * 0.20, S * 0.15, S * 0.025)))
    layers.append((white, quad_bezier_stroke((S * 0.31, S * 0.42), (S * 0.50, S * 0.10), (S * 0.69, S * 0.40), hw)))
    layers.append((white, arrow_head((S * 0.70, S * 0.42), 0.40 * math.pi, S * 0.16)))
    write_png(ICONS / "source.png", S, S, draw(S, layers))


def make_tray_icons(size: int = 44) -> None:
    """Black + alpha template images; macOS recolours them for the menu bar.

    Geometry is in 44 px units (22 pt @2x): one monitor, an arc above it while
    switching, an exclamation mark cut out of it when attention is needed.
    """

    black = (0, 0, 0)

    def solid_monitor():
        return [
            (black, rr(10, 12, 24, 16, 2.5)),
            (black, capsule((17.2, 33.6), (26.8, 33.6), 1.75)),
        ]

    idle = solid_monitor()
    switching = [
        (black, sd_outline(rr(10, 19, 24, 13, 2.5), 1.5)),
        (black, capsule((17.2, 36.8), (26.8, 36.8), 1.5)),
        (black, quad_bezier_stroke((9, 12), (22, 0), (35, 12), 1.6)),
        (black, arrow_head((35, 12), 0.42 * math.pi, 9, front=0.6, half_width=0.5)),
    ]
    attention = solid_monitor() + [
        ("erase", rr(20.5, 15, 3, 6.5, 1.5)),
        ("erase", lambda p, q: sd_circle(p, q, 22, 24.5, 1.7)),
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
