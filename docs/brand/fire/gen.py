# /// script
# requires-python = ">=3.10"
# dependencies = ["pillow>=10"]
# ///
"""gen.py: the README's animated header, "blaze" in pixel letters, on fire.

    uv run docs/brand/fire/gen.py                 # writes header-{dark,light}.webp next to this file, and
                                                  # header-{dark,light}-still.webp, its first frame
    uv run docs/brand/fire/gen.py --preview DIR   # instead, DIR/preview-{dark,light}.png: four
                                                  # frames on GitHub's backgrounds, to look at

Everything is drawn on a coarse grid of cells and scaled up by whole pixels, so it stays crisp.
The letters are drawn by hand below. The fire is a cellular automaton: each frame a cell takes
the heat of a cell below it (shifted left or right by up to one cell) minus a little, and the
cells just above the letters' top edges are the heat source. The random choices come from a hash
of (cell, frame mod FRAMES), so after the fire has burned in, frame FRAMES is frame 0 again and
the animation loops without a seam. Heat is quantized to five colours.

It writes lossless animated WebP: for a few flat colours and on-or-off transparency it is exact
and the smallest of the formats a README shows inline that every browser plays. The same 32
frames (564x336, measured 2026-10-25) came to 12 KiB as lossless WebP, 25 KiB as lossy AVIF and
37 KiB as lossless AVIF (avifenc), 50 KiB as APNG, 96 KiB as lossy WebP at quality 75 (lossy
WebP blurs the cell edges) and 228 KiB as GIF. The README picks the dark or light file by the
reader's theme; they differ in the letters' and the fire's colours.
"""
import hashlib
import math
import os
import sys

from PIL import Image

HERE = os.path.dirname(os.path.abspath(__file__))

GLYPHS = {
    "b": """
        XX.....
        XX.....
        XX.....
        XX.....
        XXXXXX.
        XXXXXXX
        XX...XX
        XX...XX
        XX...XX
        XX...XX
        XXXXXXX
        XXXXXX.
    """,
    "l": """
        XX
        XX
        XX
        XX
        XX
        XX
        XX
        XX
        XX
        XX
        XX
        XX
    """,
    "a": """
        .......
        .......
        .......
        .......
        .XXXXX.
        XXXXXXX
        .....XX
        .XXXXXX
        XXXXXXX
        XX...XX
        XXXXXXX
        .XXXXXX
    """,
    "z": """
        .......
        .......
        .......
        .......
        XXXXXXX
        XXXXXXX
        ....XXX
        ...XXX.
        ..XXX..
        .XXX...
        XXXXXXX
        XXXXXXX
    """,
    "e": """
        .......
        .......
        .......
        .......
        .XXXXX.
        XXXXXXX
        XX...XX
        XXXXXXX
        XXXXXXX
        XX.....
        XXXXXXX
        .XXXXX.
    """,
}

WORD = "blaze"
GAP = 2           # cells between letters
ABOVE = 13        # cells of room for the flames above the tallest letter
SIDE = 2          # cells of room on each side
BELOW = 1         # cells under the letters (the shadow's row)
SCALE = 16        # pixels per cell in the file, even: the README shows it at half, for 2x screens
FRAMES = 32
FPS = 16
HEAT = 12         # the hottest a cell gets; a cell under 1 is empty
COOL_MIN, COOL_MAX = 0.4, 2.3  # how much heat a cell loses per cell it rises

# The colour of each heat, coolest (1) to hottest (HEAT), from five colours: original, Minecraft-like.
LEVELS = [0, 1, 1, 2, 2, 2, 3, 3, 3, 4, 4, 4]

THEMES = {
    # The letters, their shadow (a cell down and to the right, as pixel-font game text has) and
    # the five fire colours. On white the hottest colour is a step deeper, or the flames' cores
    # would vanish into the page.
    "dark": ("#f0f6fc", "#6e2a14", ["#a8240f", "#e0461a", "#ff8f1f", "#ffd43b", "#fff6c2"]),
    "light": ("#1f2328", "#e8c3a0", ["#9a1f0c", "#d63c12", "#f57a10", "#ffae1a", "#ffd84a"]),
}
BACKGROUND = {"dark": "#0d1117", "light": "#ffffff"}


def letters():
    """The word as a set of (x, y) cells, and its width and height."""
    cells, x = set(), 0
    for i, ch in enumerate(WORD):
        rows = [r.strip() for r in GLYPHS[ch].strip().splitlines()]
        for y, row in enumerate(rows):
            for dx, c in enumerate(row):
                if c == "X":
                    cells.add((x + dx, y))
        x += len(rows[0]) + (GAP if i < len(WORD) - 1 else 0)
    height = max(y for _, y in cells) + 1
    return cells, x, height


def rand(*key):
    """A deterministic byte for a key, so the same frame mod FRAMES makes the same choices."""
    return hashlib.blake2b(repr(key).encode(), digest_size=1).digest()[0]


def source_heat(x, t):
    """How hot the base of the flame over column x burns at frame t: a few standing waves whose
    periods divide FRAMES, so some columns feed tall tongues and the gaps between them breathe."""
    phase = 2 * math.pi * t / FRAMES
    w = (0.55 * math.sin(2 * math.pi * x / 6.0 + 0.7) * math.cos(phase)
         + 0.35 * math.sin(2 * math.pi * x / 3.7 + 1.9) * math.cos(2 * phase + 0.5)
         + 0.25 * math.sin(2 * math.pi * x / 9.0 + 0.3) * math.sin(3 * phase))
    return max(0, min(HEAT, round(HEAT * (0.72 + 0.4 * w))))


def noise(name, x, k, sx, sk):
    """Smooth value noise over (x, k), k wrapping every FRAMES, in [0, 1]."""
    period = FRAMES // sk
    fx, fk = x / sx, k / sk
    x0, k0 = math.floor(fx), math.floor(fk)
    tx, tk = fx - x0, fk - k0
    tx, tk = tx * tx * (3 - 2 * tx), tk * tk * (3 - 2 * tk)

    def at(i, j):
        return rand(name, i, j % period) / 255

    top = at(x0, k0) * (1 - tx) + at(x0 + 1, k0) * tx
    bottom = at(x0, k0 + 1) * (1 - tx) + at(x0 + 1, k0 + 1) * tx
    return top * (1 - tk) + bottom * tk


def simulate(width, height, solid, sources):
    """FRAMES frames of heat (lists of rows), looping seamlessly.

    A parcel of heat moves up a cell a frame, so y + t stays the same along its path: noise keyed
    by (x, y + t) follows the flame up instead of flickering in place, which is what makes tongues
    rather than sparks."""
    heat = [[0.0] * width for _ in range(height)]

    def step(t):
        nonlocal heat
        new = [[0.0] * width for _ in range(height)]
        for y in range(height - 1):
            for x in range(width):
                if (x, y) in solid:
                    continue
                if (x, y) in sources:
                    new[y][x] = source_heat(x, t)
                    continue
                k = (y + t) % FRAMES
                sway = noise("sway", x, k, 5, 4)
                dx = -1 if sway < 0.16 else 1 if sway > 0.84 else 0
                sx = min(max(x - dx, 0), width - 1)
                cool = COOL_MIN + (COOL_MAX - COOL_MIN) * noise("cool", x, k, 2.2, 4) ** 1.5
                cool += 2.5 * max(0, 4 - y)  # near the top edge, so no flame is cut off flat
                new[y][x] = max(heat[y + 1][sx] - cool, 0.0)
        heat = new

    burn_in = FRAMES * (height // FRAMES + 2)
    for t in range(burn_in):
        step(t % FRAMES)
    frames = []
    for t in range(FRAMES):
        step(t)
        frames.append([row[:] for row in heat])
    step(0)
    assert heat == frames[0], "the fire did not settle into a loop"
    return frames


def render(theme):
    """The frames, the rows above what the flames ever reach cropped."""
    word, word_w, word_h = letters()
    width, height = word_w + 2 * SIDE + 1, ABOVE + word_h + BELOW
    ox, oy = SIDE, ABOVE
    solid = {(x + ox, y + oy) for x, y in word}
    shadow = {(x + 1, y + 1) for x, y in solid} - solid
    tops = {}
    for x, y in solid:
        tops[x] = min(y, tops.get(x, y))
    sources = {(x, y - 1) for x, y in tops.items()}
    frames = simulate(width, height, solid, sources)
    ink, shade, fire = THEMES[theme]
    palette = [hex_rgba(fire[level]) for level in LEVELS]
    images = []
    for heat in frames:
        img = Image.new("RGBA", (width, height), (0, 0, 0, 0))
        px = img.load()
        for y in range(height):
            for x in range(width):
                h = int(heat[y][x])
                if h > 0:
                    px[x, y] = palette[min(h, HEAT) - 1]
        for x, y in shadow:
            px[x, y] = hex_rgba(shade)
        for x, y in solid:
            px[x, y] = hex_rgba(ink)
        images.append(img)
    top = min(img.getbbox()[1] for img in images)
    box = (0, top, width, height)
    return [img.crop(box).resize(((box[2] - box[0]) * SCALE, (box[3] - box[1]) * SCALE), Image.NEAREST)
            for img in images]


def hex_rgba(c):
    return tuple(int(c[i:i + 2], 16) for i in (1, 3, 5)) + (255,)


def main():
    args = sys.argv[1:]
    preview = args[args.index("--preview") + 1] if "--preview" in args else None
    for theme in THEMES:
        images = render(theme)
        w, h = images[0].size
        if preview:
            os.makedirs(preview, exist_ok=True)
            picks = images[:: FRAMES // 4]
            sheet = Image.new("RGBA", (w, (h + SCALE) * len(picks)), BACKGROUND[theme])
            for i, img in enumerate(picks):
                sheet.alpha_composite(img, (0, (h + SCALE) * i))
            sheet.convert("RGB").save(os.path.join(preview, f"preview-{theme}.png"))
            continue
        out = os.path.join(HERE, f"header-{theme}.webp")
        images[0].save(out, save_all=True, append_images=images[1:], duration=1000 // FPS, loop=0,
                       background=(0, 0, 0, 0), lossless=True, quality=80, method=4)
        print(f"{os.path.relpath(out)}: {w}x{h}, shown at {w // 2}x{h // 2}, {FRAMES} frames, "
              f"{os.path.getsize(out) / 1024:.1f} KiB")
        # The site's sidebar shows this until the pointer is on the logo.
        still = os.path.join(HERE, f"header-{theme}-still.webp")
        images[0].save(still, lossless=True, quality=80, method=4)
        print(f"{os.path.relpath(still)}: the first frame, {os.path.getsize(still) / 1024:.1f} KiB")


if __name__ == "__main__":
    main()
