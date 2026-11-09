#!/usr/bin/env python3
"""pfmdiff.py: two float frames compared pixel by pixel, failing past a bound.

    pfmdiff.py '<json>' | FILE | -

What a shader port is held to (docs/future/gpu/shaders.md, "Verifying the port"): an offscreen
shot of one scene before and after, each as the PFM `HARNESS_NATIVE_SHOT_PFM` writes (the frame
unclipped: extended sRGB-encoded, premultiplied float RGB, little endian, bottom row first),
compared channel by channel. Fast math lets two shader compilers differ in the last bits, so the
bound is on the largest absolute difference, not on equality.

The input is one JSON object:

  before     the PFM of the frame before the change (required)
  after      the PFM of the frame after it (required)
  tolerance  the largest absolute difference a channel may have (default 1/1024, of white = 1.0)
  show       how many of the pixels past it to name, worst first (default 10)

It prints the largest difference and where (x, y from the top left, as the PNG has them, and the
channel), how many pixels are past the bound, and each of the first `show` of them with both
values; it exits 1 if any pixel is past the bound, 2 if the input is wrong or the frames can't be
compared (a size apart, not a PFM). Python 3, stdlib only.
"""
import json, struct, sys, typing
from dataclasses import dataclass, fields
from typing import NoReturn, cast


@dataclass(frozen=True)
class Input:
    before: str
    after: str
    tolerance: float = 1.0 / 1024.0
    show: int = 10


def refuse(what: str) -> NoReturn:
    """An input or a frame that can't be compared: said, and exit 2, apart from a pixel past the bound's 1."""
    sys.stderr.write(f"pfmdiff.py: {what}\n")
    sys.exit(2)


def typed(text: str) -> Input:
    """The Input from the JSON `text` (inline, a path, or `-`); exits naming the field that is wrong."""
    if text == "-":
        text = sys.stdin.read()
    elif text and not text.lstrip().startswith("{"):
        text = open(text).read()
    try:
        value: object = json.loads(text or "{}")
    except json.JSONDecodeError as e:
        refuse(f"the input is not JSON: {e}")
    if not isinstance(value, dict):
        refuse("the input is a JSON object")
    known = {f.name for f in fields(Input)}
    value = cast("dict[str, object]", value)
    if unknown := sorted(set(value) - known):
        refuse(f"no field {', '.join(unknown)} (fields: {', '.join(sorted(known))})")
    if missing := sorted({"before", "after"} - set(value)):
        refuse(f"field {', '.join(missing)} is required: the PFM path")
    hints = typing.get_type_hints(Input)
    for name, given in value.items():
        want = hints[name]
        # A whole number is a float tolerance too.
        ok = type(given) is want or (want is float and type(given) is int)
        if not ok:
            refuse(f"field {name} is a {want.__name__}, not {given!r}")
    if value.get("tolerance", 0.0) < 0:
        refuse(f"field tolerance is at least 0, not {value['tolerance']!r}")
    if value.get("show", 0) < 0:
        refuse(f"field show is at least 0, not {value['show']!r}")
    return Input(**{k: float(v) if hints[k] is float else v for k, v in value.items()})


def read_pfm(path):
    """(width, height, the channels top row first): a colour PFM, as shot.rs's `write_pfm` writes it."""
    try:
        data = open(path, "rb").read()
    except OSError as e:
        refuse(f"{path}: {e}")
    parts = data.split(b"\n", 3)
    if len(parts) < 4 or parts[0] != b"PF":
        refuse(f"{path}: not a colour PFM (no `PF` header)")
    try:
        w, h = (int(n) for n in parts[1].split())
        scale = float(parts[2])
    except ValueError:
        refuse(f"{path}: a PFM header of `{parts[1]!r} {parts[2]!r}`")
    order = "<" if scale < 0 else ">"
    body = parts[3]
    if len(body) != w * h * 12:
        refuse(f"{path}: {len(body)} bytes of pixels, where {w}x{h} RGB floats are {w * h * 12}")
    rows = [struct.unpack_from(f"{order}{w * 3}f", body, row * w * 12) for row in range(h)]
    rows.reverse()  # bottom row first in the file
    return w, h, rows


def main():
    if len(sys.argv) > 2:
        refuse(__doc__)
    given = typed(sys.argv[1] if len(sys.argv) == 2 else "")
    wa, ha, a = read_pfm(given.before)
    wb, hb, b = read_pfm(given.after)
    if (wa, ha) != (wb, hb):
        refuse(f"{given.before} is {wa}x{ha} and {given.after} is {wb}x{hb}")
    worst = (0.0, 0, 0, 0)
    past = []  # (difference, x, y, channel)
    for y in range(ha):
        ra, rb = a[y], b[y]
        for i in range(wa * 3):
            d = abs(ra[i] - rb[i])
            if d != d:  # NaN in one frame and not the other, or in both
                d = float("inf")
            if d > worst[0]:
                worst = (d, i // 3, y, i % 3)
            if d > given.tolerance:
                past.append((d, i // 3, y, i % 3))
    channel = "RGB"
    d, x, y, c = worst
    print(f"pfmdiff: {wa}x{ha}, largest difference {d:.6g} at ({x}, {y}) {channel[c]}; bound {given.tolerance:.6g}")
    pixels = {(x, y) for _, x, y, _ in past}
    print(f"pfmdiff: {len(pixels)} pixels past the bound")
    past.sort(reverse=True)
    for d, x, y, c in past[: given.show]:
        before, after = a[y][x * 3 + c], b[y][x * 3 + c]
        print(f"  ({x}, {y}) {channel[c]}: {before:.6g} before, {after:.6g} after, {d:.6g} apart")
    sys.exit(1 if past else 0)


if __name__ == "__main__":
    main()
