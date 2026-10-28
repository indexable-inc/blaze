# /// script
# requires-python = ">=3.10"
# dependencies = ["pillow>=10"]
# ///
"""icon.py: the square icon, the pixel "b" of the header (gen.py) on fire, for favicons and avatars.

    uv run docs/brand/fire/icon.py   # writes docs/brand/blaze-icon-{dark,light}.svg, PNGs of
                                     # each at PNG_SIZES and favicon.ico, and site/static/brand/'s copies

The letter, its shadow and the five fire colours are gen.py's; the flame is drawn by hand below, since the
header's simulation makes shapeless blobs at 24 cells.
"""
import os

from PIL import Image, ImageDraw
from gen import GLYPHS, THEMES
# Flame cells: (x, y): heat 1 (deep red) .. 5 (white-hot), in a 24-cell square, b's stem at x=8.
F = """
y2: 9:1
y3: 8:1 9:2
y4: 8:2 9:2 10:1
y5: 7:1 8:3 9:2 10:1
y6: 7:2 8:3 9:3 10:2
y7: 7:2 8:4 9:3 10:2 13:1
y8: 6:1 7:3 8:4 9:4 10:2 12:1 13:2
y9: 6:1 7:3 8:5 9:4 10:3 11:1 12:2 13:3 14:1
y10: 7:2 10:3 11:2 12:3 13:3 14:2
y11: 7:1 10:4 11:3 12:4 13:4 14:2 15:1
y12: 10:4 11:4 12:5 13:4 14:3 15:1
y13: 10:5 11:5 12:5 13:5 14:4 15:2
y14: 14:4 15:2
"""
DX, DY = 2, 0
flame = {}
for line in F.strip().splitlines():
    y, *cells = line.split()
    for c in cells:
        x, h = map(int, c.split(":"))
        flame[(x + DX, int(y[1:-1]) + DY)] = h
rows = [r.strip() for r in GLYPHS["b"].strip().splitlines()]
solid = {(8 + DX + x, 10 + DY + y) for y, r in enumerate(rows) for x, c in enumerate(r) if c == "X"}
shadow = {(x + 1, y + 1) for x, y in solid} - solid - set(flame)
N, pad = 24, 1
PNG_SIZES = [32, 180, 512, 1024]
CELL = 80  # pixels per cell before scaling down to each PNG size
ROOT = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "..", "..")
OUTS = [os.path.join(ROOT, "docs", "brand"), os.path.join(ROOT, "site", "static", "brand")]
for theme, bg in (("dark", "#0d1117"), ("light", "#ffffff")):
    ink, shade, fire = THEMES[theme]
    cells = [(x, y, shade) for x, y in shadow] + [(x, y, fire[h - 1]) for (x, y), h in flame.items()] + [(x, y, ink) for x, y in solid]
    s = N + 2 * pad
    body = "".join(f'<rect x="{x+pad}" y="{y+pad}" width="1" height="1" fill="{c}"/>' for x, y, c in cells)
    svg = (
        f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {s} {s}" shape-rendering="crispEdges" role="img" aria-label="blaze">'
        f'<title>blaze</title><rect width="{s}" height="{s}" rx="{s*0.22:.1f}" fill="{bg}"/>{body}</svg>')
    for out in OUTS:
        with open(os.path.join(out, f"blaze-icon-{theme}.svg"), "w") as f:
            f.write(svg)
    # The same cells as pixels, for where SVG is not taken: square cells at CELL, the corners
    # cut from a mask, then scaled down to each size.
    big = Image.new("RGBA", (s * CELL, s * CELL), (0, 0, 0, 0))
    draw = ImageDraw.Draw(big)
    draw.rectangle((0, 0, s * CELL, s * CELL), fill=bg)
    for x, y, c in cells:
        draw.rectangle(((x + pad) * CELL, (y + pad) * CELL, (x + pad + 1) * CELL - 1, (y + pad + 1) * CELL - 1), fill=c)
    mask = Image.new("L", big.size, 0)
    ImageDraw.Draw(mask).rounded_rectangle((0, 0, s * CELL - 1, s * CELL - 1), radius=round(s * 0.22 * CELL), fill=255)
    big.putalpha(mask)
    for size in PNG_SIZES:
        img = big.resize((size, size), Image.LANCZOS)
        for out in OUTS:
            img.save(os.path.join(out, f"blaze-icon-{theme}-{size}.png"), optimize=True)
    if theme == "dark":
        for out in OUTS:
            big.resize((256, 256), Image.LANCZOS).save(os.path.join(out, "favicon.ico"), sizes=[(16, 16), (32, 32), (48, 48)])
