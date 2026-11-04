"""Port Ghostty's Nerd Font constraint table to Rust.

Ghostty generates src/font/nerd_font_tables.zig from the Nerd Fonts patcher (src/font/nerd_font_codegen.py): a 3-level
lookup from a codepoint to a `Glyph.RenderOptions.Constraint`. This reads that generated file and writes the same data as
codepoint ranges for crates/harness/harness-native/src/text/nerd.rs.

    uv run python fonts/nerd-symbols/gen_constraints.py <nerd_font_tables.zig> <ghostty commit> > src/text/nerd.rs
"""

import re
import sys
from pathlib import Path

FIELDS = {
    "size": "size",
    "height": "height",
    "max_constraint_width": "max_w",
    "align_horizontal": "align_x",
    "align_vertical": "align_y",
    "pad_top": "pad_top",
    "pad_left": "pad_left",
    "pad_right": "pad_right",
    "pad_bottom": "pad_bottom",
    "relative_width": "rel_w",
    "relative_height": "rel_h",
    "relative_x": "rel_x",
    "relative_y": "rel_y",
    "max_xy_ratio": "max_xy",
}
ENUMS = {
    "size": {"none": "Size::None", "fit": "Size::Fit", "cover": "Size::Cover", "fit_cover1": "Size::FitCover1", "stretch": "Size::Stretch"},
    "height": {"cell": "Height::Cell", "icon": "Height::Icon"},
    "align_x": {"none": "Align::None", "start": "Align::Start", "end": "Align::End", "center": "Align::Center", "center1": "Align::Center1"},
}
ENUMS["align_y"] = ENUMS["align_x"]


def array(src: str, name: str) -> list[int]:
    m = re.search(rf"pub const {name}: \[\d+\]u16 = \.\{{([^}}]*)\}};", src)
    if m is None:
        raise SystemExit(f"no {name} in the table")
    return [int(v) for v in m.group(1).split(",") if v.strip()]


def literals(src: str) -> list[dict[str, str] | None]:
    body = src[src.index("pub const stage3") :]
    body = body[body.index("= .{") + 4 :]
    out: list[dict[str, str] | None] = []
    for item in re.finditer(r"null|\.\{([^}]*)\}", body):
        if item.group(0) == "null":
            out.append(None)
            continue
        fields: dict[str, str] = {}
        for k, v in re.findall(r"\.(\w+)\s*=\s*([^,\n]+),", item.group(1)):
            fields[FIELDS[k]] = v.strip()
        out.append(fields)
        if body[item.end() :].lstrip().startswith("}"):
            break
    return out


def rust(c: dict[str, str]) -> str:
    vals = {"size": "Size::None", "height": "Height::Cell", "max_w": "2", "align_x": "Align::None", "align_y": "Align::None"}
    vals |= {k: "0.0" for k in ("pad_top", "pad_left", "pad_right", "pad_bottom", "rel_x", "rel_y")}
    vals |= {"rel_w": "1.0", "rel_h": "1.0", "max_xy": "0.0"}
    for k, v in c.items():
        if k in ENUMS:
            vals[k] = ENUMS[k][v.lstrip(".")]
        elif k == "max_w":
            vals[k] = v
        else:
            vals[k] = repr(float(v))
    order = ["size", "height", "max_w", "align_x", "align_y", "pad_top", "pad_left", "pad_right", "pad_bottom", "rel_w", "rel_h", "rel_x", "rel_y", "max_xy"]
    return "C { " + ", ".join(f"{k}: {vals[k]}" for k in order) + " }"


def main() -> None:
    src = Path(sys.argv[1]).read_text()
    commit = sys.argv[2]
    s1, s2, s3 = array(src, "stage1"), array(src, "stage2"), literals(src)
    ranges: list[list[int]] = []
    for cp in range(0x110000):
        idx = s2[s1[cp >> 8] + (cp & 0xFF)]
        if idx == 0:
            continue
        if ranges and ranges[-1][1] == cp - 1 and ranges[-1][2] == idx:
            ranges[-1][1] = cp
        else:
            ranges.append([cp, cp, idx])
    print("//! Ghostty's Nerd Font glyph constraints (src/font/nerd_font_tables.zig, generated there from the Nerd Fonts")
    print(f"//! patcher), ported as data from ghostty-org/ghostty@{commit} by fonts/nerd-symbols/gen_constraints.py. MIT, Ghostty.")
    print("//! Do not edit by hand.")
    print("use super::nerd_rules::{Align, Constraint as C, Height, Size};")
    print()
    print(f"pub(super) const CONSTRAINTS: [C; {len(s3) - 1}] = [")
    for c in s3[1:]:
        assert c is not None
        print(f"    {rust(c)},")
    print("];")
    print()
    print("/// First and last codepoint of each range and its constraint, index into [`CONSTRAINTS`] plus one.")
    print(f"pub(super) const RANGES: [(u32, u32, u16); {len(ranges)}] = [")
    for a, b, i in ranges:
        print(f"    (0x{a:X}, 0x{b:X}, {i}),")
    print("];")


if __name__ == "__main__":
    main()
