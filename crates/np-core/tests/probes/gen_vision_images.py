#!/usr/bin/env python3
"""Generate the R7c vision-probe test images. Stdlib only — this machine has no Pillow.

Two shapes at two sizes, all flat colour so nothing depends on a font:

    red_square_<W>x<H>.png       a solid red square, centred, on white
    stripes_<W>x<H>.png          blue/white diagonal stripes on white

The shapes are chosen to be visually unmistakable to a vision encoder that has never seen the file: "a red
square" and "blue and white diagonal stripes" are things a caption can say and a model can only have learned
from pixels, not from a filename.

Usage (writes into probes/vision/, which is where the probe note points):

    python3 crates/np-core/tests/probes/gen_vision_images.py
"""

import os
import struct
import zlib

SIZES = [(1024, 768), (512, 384)]

RED = (220, 30, 30)
BLUE = (30, 60, 200)
WHITE = (255, 255, 255)


def pixel(shape: str, x: int, y: int, w: int, h: int):
    side = min(w, h) * 6 // 10
    x0 = (w - side) // 2
    y0 = (h - side) // 2
    if shape == "red_square":
        return RED if x0 <= x < x0 + side and y0 <= y < y0 + side else WHITE
    if shape == "stripes":
        step = max(w // 12, 1)
        return BLUE if ((x + y) // step) % 2 == 0 else WHITE
    raise ValueError(f"unknown shape: {shape}")


def write_png(path: str, w: int, h: int, shape: str) -> None:
    raw = bytearray()
    for y in range(h):
        raw.append(0)  # filter type 0 (None) — one filter byte per scanline, then RGB triples
        for x in range(w):
            raw += bytes(pixel(shape, x, y, w, h))

    def chunk(tag: bytes, data: bytes) -> bytes:
        return (struct.pack(">I", len(data)) + tag + data
                + struct.pack(">I", zlib.crc32(tag + data) & 0xFFFFFFFF))

    header = struct.pack(">IIBBBBB", w, h, 8, 2, 0, 0, 0)  # 8-bit truecolour RGB, no interlace
    with open(path, "wb") as f:
        f.write(b"\x89PNG\r\n\x1a\n")
        f.write(chunk(b"IHDR", header))
        f.write(chunk(b"IDAT", zlib.compress(bytes(raw), 9)))
        f.write(chunk(b"IEND", b""))


def main() -> None:
    out = os.path.join(os.path.dirname(os.path.abspath(__file__)), "vision")
    os.makedirs(out, exist_ok=True)
    for shape in ("red_square", "stripes"):
        for (w, h) in SIZES:
            path = os.path.join(out, f"{shape}_{w}x{h}.png")
            write_png(path, w, h, shape)
            print(f"wrote {path} ({os.path.getsize(path)} bytes)")


if __name__ == "__main__":
    main()
