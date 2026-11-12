"""Draws the app icons (PNG) with the standard library only. Run: python3 tools/icons.py"""
import math, struct, sys, zlib
from pathlib import Path

BG, RING, DOT = (12, 12, 14), (245, 245, 247), (52, 211, 153)

def png(path, size, safe):
    """safe: fraction of the canvas the glyph may use (maskable icons need a safe zone)."""
    c = size / 2
    ring_r, ring_w, dot_r = size * 0.24 * safe / 0.8, size * 0.07 * safe / 0.8, size * 0.085 * safe / 0.8
    corner = 0 if safe < 0.8 else size * 0.22
    rows = []
    for y in range(size):
        row = bytearray([0])
        for x in range(size):
            px, py = x + 0.5, y + 0.5
            # rounded-square background (full bleed for maskable)
            dx, dy = max(abs(px - c) - (c - corner), 0), max(abs(py - c) - (c - corner), 0)
            inside = corner == 0 or math.hypot(dx, dy) <= corner
            a = 255 if inside else 0
            col = BG
            d = math.hypot(px - c, py - c)
            ring = 1 - min(1, max(0, abs(d - ring_r) - ring_w / 2))
            dot = 1 - min(1, max(0, d - dot_r))
            if dot > 0:
                col = tuple(round(BG[i] * (1 - dot) + DOT[i] * dot) for i in range(3))
            elif ring > 0:
                col = tuple(round(BG[i] * (1 - ring) + RING[i] * ring) for i in range(3))
            row += bytes(col) + bytes([a])
        rows.append(bytes(row))
    raw = zlib.compress(b"".join(rows), 9)
    chunk = lambda t, d: struct.pack(">I", len(d)) + t + d + struct.pack(">I", zlib.crc32(t + d) & 0xFFFFFFFF)
    Path(path).write_bytes(b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", size, size, 8, 6, 0, 0, 0)) + chunk(b"IDAT", raw) + chunk(b"IEND", b""))

out = Path(sys.argv[1] if len(sys.argv) > 1 else "src/pwa")
png(out / "icon-192.png", 192, 0.8)
png(out / "icon-512.png", 512, 0.8)
png(out / "icon-maskable.png", 512, 0.6)
png(out / "apple-touch-icon.png", 180, 0.6)  # iOS rounds the corners itself, so no transparent edge
