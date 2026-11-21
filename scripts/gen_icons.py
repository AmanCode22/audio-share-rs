#!/usr/bin/env python3
"""Generate app icons for the desktop companion (Tauri) from scratch.

Produces a rounded-square gradient tile with a speaker + sound-wave glyph:
  apps/desktop/app-icon.png           (512, source icon)
  apps/desktop/icons/32x32.png
  apps/desktop/icons/128x128.png
  apps/desktop/icons/128x128@2x.png   (256)
  apps/desktop/icons/icon.png         (512)
Also copies the 512 icon to apps/desktop/ui/assets/icon.png for the UI header.
"""
import math
import os
import struct
import zlib

OUT = os.path.join(os.path.dirname(__file__), "..", "apps", "desktop")


def write_png(path: str, size: int, data: bytes):
    """data: RGBA rows, size*size*4 bytes."""
    raw = b"".join(
        b"\x00" + data[y * size * 4 : (y + 1) * size * 4] for y in range(size)
    )

    def chunk(tag: bytes, payload: bytes) -> bytes:
        c = struct.pack(">I", len(payload)) + tag + payload
        return c + struct.pack(">I", zlib.crc32(tag + payload) & 0xFFFFFFFF)

    ihdr = struct.pack(">IIBBBBB", size, size, 8, 6, 0, 0, 0)
    png = (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", ihdr)
        + chunk(b"IDAT", zlib.compress(raw, 9))
        + chunk(b"IEND", b"")
    )
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "wb") as f:
        f.write(png)
    print("wrote", path)


def render(size: int) -> bytes:
    """Rounded-square gradient tile + speaker/wave glyph."""
    px = bytearray(size * size * 4)
    r = size * 0.22  # corner radius
    cx, cy = size * 0.5, size * 0.5

    # gradient stops (top-left -> bottom-right): indigo -> azure
    top = (54, 62, 149)
    bottom = (35, 130, 228)

    def inside_rounded(x, y):
        # distance from rect with rounded corners
        minx, miny, maxx, maxy = 0, 0, size - 1, size - 1
        qx = max(minx + r - x, 0, x - (maxx - r))
        qy = max(miny + r - y, 0, y - (maxy - r))
        return qx * qx + qy * qy <= r * r

    def lerp(a, b, t):
        return a + (b - a) * t

    # glyph geometry in unit coords (0..1)
    sp_x0, sp_x1 = 0.26, 0.44          # speaker body x-range
    sp_yc = 0.50                        # speaker center y
    sp_half_h = 0.10                    # half height of box
    cone_x = 0.44                       # cone apex x

    for y in range(size):
        t = y / size
        gr = (lerp(top[0], bottom[0], t), lerp(top[1], bottom[1], t), lerp(top[2], bottom[2], t))
        for x in range(size):
            idx = (y * size + x) * 4
            if not inside_rounded(x, y):
                px[idx + 3] = 0
                continue
            # glyph anti-aliasing coverage
            u, v = x / size, y / size
            alpha = 0.0

            # speaker box (rounded rect)
            if sp_x0 <= u <= sp_x1 and abs(v - sp_yc) <= sp_half_h:
                alpha = max(alpha, 1.0)
            # cone (triangle)
            if sp_x1 <= u <= cone_x:
                half = 0.19 * (1 - (u - sp_x1) / (cone_x - sp_x1))
                if abs(v - sp_yc) <= half:
                    alpha = max(alpha, 1.0)
            # sound arcs (three quarter-arcs)
            for k, rr in enumerate((0.16, 0.26, 0.36)):
                d = math.hypot(u - sp_x1, v - sp_yc)
                if abs(d - rr) <= 0.022 and u >= sp_x1 - 0.02 and abs(v - sp_yc) <= rr:
                    alpha = max(alpha, 1.0 - abs(d - rr) / 0.022)
            # dome of speaker
            if u <= cone_x and math.hypot(u - sp_x1, v - sp_yc) <= 0.0 and False:
                pass

            if alpha > 0:
                col = (245, 248, 255)
                a = alpha
            else:
                col, a = gr, 1.0
            px[idx] = int(col[0])
            px[idx + 1] = int(col[1])
            px[idx + 2] = int(col[2])
            px[idx + 3] = int(a * 255)
    return bytes(px)


def main():
    sizes = {
        "app-icon.png": 512,
        "icons/icon.png": 512,
        "icons/128x128@2x.png": 256,
        "icons/128x128.png": 128,
        "icons/32x32.png": 32,
    }
    cache = {}
    for rel, s in sizes.items():
        if s not in cache:
            cache[s] = render(s)
        write_png(os.path.join(OUT, rel), s, cache[s])
    # copy for the webview header
    write_png(os.path.join(OUT, "ui", "assets", "icon.png"), 512, cache[512])


if __name__ == "__main__":
    main()
