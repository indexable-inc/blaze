#!/usr/bin/env python3
"""Writes crates/blaze-lints/src/registry/clippy.rs: every lint the pinned nightly's clippy
declares (name, group, description, the version it came in), and the lints it renamed and
removed, read from clippy's source at the pinned rust-lang/rust commit.

    python3 harness/clippy_registry.py [--clippy <rust checkout>/src/tools/clippy] [--check]

The source is `clippy_lints/src/**/*.rs` (`declare_clippy_lint!`) and
`clippy_lints/src/deprecated_lints.rs`. `--check` fails when the file is not what it would write.
"""

import argparse
import pathlib
import re
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
OUT = ROOT / "crates/blaze-lints/src/registry/clippy.rs"
DEFAULT_CLIPPY = pathlib.Path("/Volumes/Projects/tmp/blaze-subcmds/rust/src/tools/clippy")
COMMIT = "12433c307b5a497388ffe2f4e49dbf1ab8c71fa0"

DECLARE = re.compile(
    r"declare_clippy_lint!\s*\{(?P<body>.*?)\n\}",
    re.S,
)
HEAD = re.compile(
    r'#\[clippy::version\s*=\s*"(?P<version>[^"]*)"\]\s*pub\s+(?P<name>[A-Z0-9_]+)\s*,\s*'
    r'(?:(?://[^\n]*\n)\s*)*(?P<group>[a-z]+)\s*,\s*'
    r'(?P<desc>r#"(?:.*?)"#|"(?:[^"\\]|\\.)*")',
    re.S,
)
PAIR = re.compile(r'\(\s*"clippy::([a-z0-9_]+)"\s*,\s*"((?:[^"\\]|\\.)*)"\s*\)')

GROUPS = {
    "correctness": "Correctness",
    "suspicious": "Suspicious",
    "style": "Style",
    "complexity": "Complexity",
    "perf": "Perf",
    "pedantic": "Pedantic",
    "restriction": "Restriction",
    "nursery": "Nursery",
    "cargo": "Cargo",
}


def lints(clippy: pathlib.Path):
    found = []
    for path in sorted((clippy / "clippy_lints/src").rglob("*.rs")):
        text = path.read_text()
        for block in DECLARE.finditer(text):
            head = HEAD.search(block.group("body"))
            if not head:
                continue
            desc = head.group("desc")
            if desc.startswith('r#"'):
                desc = desc[3:-2].replace("\\", "\\\\").replace('"', '\\"')
            else:
                desc = desc[1:-1]
            found.append((head.group("name").lower(), head.group("group"), desc, head.group("version")))
    found.sort()
    return found


def removed(clippy: pathlib.Path):
    text = (clippy / "clippy_lints/src/deprecated_lints.rs").read_text()
    deprecated = text.split("DEPRECATED(DEPRECATED_VERSION)")[1].split("]}")[0]
    renamed = text.split("RENAMED(RENAMED_VERSION)")[1].split("]}")[0]
    return PAIR.findall(deprecated), PAIR.findall(renamed)


def render(found, deprecated, renamed) -> str:
    out = [
        f"//! Every lint of the pinned nightly's clippy, from its source at rust-lang/rust {COMMIT}",
        "//! (`src/tools/clippy/clippy_lints/src`, MIT or Apache-2.0). Written by",
        "//! `harness/clippy_registry.py`; do not edit by hand.",
        "",
        "use super::{Group, Info};",
        "",
        "/// By name, as `#[allow(clippy::<name>)]` writes it.",
        "pub(super) static LINTS: &[Info] = &[",
    ]
    for name, group, desc, version in found:
        out.append(
            f'    Info {{ name: "{name}", group: Group::{GROUPS[group]}, desc: "{desc}", version: "{version}" }},'
        )
    out += [
        "];",
        "",
        "/// The lints clippy removed, and why: naming one is a warning of rustc's, and does nothing.",
        "pub(super) static REMOVED: &[(&str, &str)] = &[",
    ]
    for name, why in deprecated:
        out.append(f'    ("{name}", "{why}"),')
    out += [
        "];",
        "",
        "/// The lints clippy renamed, to their names now (some to rustc's own lints).",
        "pub(super) static RENAMED: &[(&str, &str)] = &[",
    ]
    for old, new in renamed:
        out.append(f'    ("{old}", "{new}"),')
    out += ["];", ""]
    return "\n".join(out)


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--clippy", type=pathlib.Path, default=DEFAULT_CLIPPY)
    parser.add_argument("--check", action="store_true")
    args = parser.parse_args()
    found = lints(args.clippy)
    deprecated, renamed = removed(args.clippy)
    text = render(found, deprecated, renamed)
    if args.check:
        if OUT.read_text() != text:
            print(f"{OUT} is not what {args.clippy} declares", file=sys.stderr)
            return 1
        return 0
    OUT.parent.mkdir(parents=True, exist_ok=True)
    OUT.write_text(text)
    print(f"wrote {len(found)} lints, {len(deprecated)} removed, {len(renamed)} renamed to {OUT}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
