#!/usr/bin/env python3
# pyright: basic
"""includes.py: a daemon follows edits to a file that only `include!` names.

The file is not a `.rs` file, so nothing loads it until the macro is expanded. Three more files
that `include!`, `include_str!` and `include_bytes!` read sit outside the package, in no
directory the build loads, so an edit of them moves nothing the daemon's view holds (#1638:
`include!` of such a file). Each step edits one of them or the file that includes it, asks the
daemon for the program by `blaze build` and holds it to what a fresh build of the same source
does (rebuild.py). A step may also say whether the program's unit is to be kept from the build
before (`"fresh"` in its `compiler-artifact` message): a build that edits nothing reuses the
unit's record, filed by what its crate read outside every source root, and one that edits such
a file compiles it again.

usage: includes.py '{}'

Its steps are fixed, so its input is the empty JSON object (inline, the path of a file, or `-`
for standard input): any field is refused. Run it in the repository's Python workspace: `uv run
--frozen python harness/includes.py '{}'` from the root. The scripts that import `follow` run
steps of their own.
"""

import json
import os
import shutil
import sys
import time
from dataclasses import dataclass
from pathlib import Path

from typed_input import fail, parse

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
from rebuild import WRONG, Project, scratch

TOML = """[package]
name = "sinc"
version = "0.1.0"
edition = "2021"
"""
DATA_TEXT = """pub const TABLE: [u32; 3] = [3, 5, 8];
pub fn pick(i: usize) -> u32 {
    TABLE[i % 3] * 2
}
macro_rules! make {
    ($name:ident, $v:expr) => {
        pub fn $name() -> u32 {
            $v + pick(1)
        }
    };
}
"""
MAIN_TEXT = """mod data {
    include!("table.rs.data");
    make!(made, 100);
}

fn main() {
    println!("{} {} {:?}", data::pick(2), data::made(), data::TABLE);
    println!("{} {:?}", include_str!("label.txt"), include_bytes!("bytes.dat"));
    println!("{} {:?}", include_str!("../../far/label.txt"), include_bytes!("../../far/bytes.dat"));
    println!("{}", far::code());
}

mod far {
    include!("../../far/code.rs");
}
"""
DATA, MAIN = "src/table.rs.data", "src/main.rs"
LABEL, BYTES = "src/label.txt", "src/bytes.dat"
# Outside the package's directory (`ws`), beside it: read only through the macros.
FAR_LABEL, FAR_BYTES, FAR_CODE = "../far/label.txt", "../far/bytes.dat", "../far/code.rs"
FAR_CODE_TEXT = "pub fn code() -> u32 {\n    40\n}\n"
# (what the step is, [(file, old, new)], whether it does not build[, whether the program's unit
# is kept from the build before]). An `old` of None makes the file, a `new` of None removes it.
STEPS = [
    ("nothing edited", [], False),
    ("a body in the included file", [(DATA, "* 2", "* 7")], False),
    ("a constant in the included file", [(DATA, "[3, 5, 8]", "[4, 6, 9]")], False),
    ("a macro in the included file", [(DATA, "$v + pick(1)", "$v * 2 + pick(0)")], False),
    ("the included file broken", [(DATA, "* 7", '* "x"')], True),
    ("mended", [(DATA, '* "x"', "* 11")], False),
    (
        "a function added to it and used",
        [
            (DATA, "macro_rules!", "pub fn more() -> u32 {\n    77\n}\nmacro_rules!"),
            (MAIN, "fn main() {", 'fn main() {\n    println!("{}", data::more());'),
        ],
        False,
    ),
    ("the including file alone", [(MAIN, "make!(made, 100)", "make!(made, 200)")], False),
    ("the included file again", [(DATA, "77", "78")], False),
    ("remove include_str input", [(LABEL, "old label", None)], True),
    ("the input is still missing", [], True),
    ("remove the reference", [(MAIN, 'include_str!("label.txt")', '"fallback"')], False),
    ("restore the reference", [(MAIN, '"fallback"', 'include_str!("label.txt")')], True),
    ("restore include_str input", [(LABEL, None, "new label")], False),
    ("remove include_bytes input", [(BYTES, "old bytes", None)], True),
    ("restore include_bytes input", [(BYTES, None, "new bytes")], False),
    ("include_str input outside the package", [(FAR_LABEL, "far label", "far label edited")], False),
    ("include_bytes input outside the package", [(FAR_BYTES, "far bytes", "far bytes edited")], False),
    ("remove the outside include_bytes input", [(FAR_BYTES, "far bytes edited", None)], True),
    ("restore the outside include_bytes input", [(FAR_BYTES, None, "far bytes back")], False),
    ("nothing edited: the unit is kept", [], False, True),
    ("include! input outside every root", [(FAR_CODE, "40", "41")], False, False),
    ("nothing edited after it", [], False, True),
    ("the outside include! input broken", [(FAR_CODE, "41", '"x"')], True),
    ("mended", [(FAR_CODE, '"x"', "42")], False),
    ("a function added to it", [(FAR_CODE, "\n}\n", "\n}\npub fn more() -> u32 {\n    1\n}\n")], False),
    ("used from the including file", [(MAIN, "far::code());", "far::code() + far::more());")], False),
    ("remove the outside include! input", [(FAR_CODE, None, None)], True),
    (
        "restore it",
        [(FAR_CODE, None, "pub fn code() -> u32 {\n    43\n}\npub fn more() -> u32 {\n    2\n}\n")],
        False,
    ),
    ("nothing edited at the end", [], False, True),
]


def kept(said, program):
    """Whether the build that said `said` kept `program`'s unit from the build before: the
    `fresh` of its `compiler-artifact` message, None with no such message."""
    found = None
    for line in said.splitlines():
        if not line.startswith("{"):
            continue
        try:
            message = json.loads(line)
        except json.JSONDecodeError:
            continue
        if message.get("reason") == "compiler-artifact" and message.get("target", {}).get("name") == program:
            found = message.get("fresh")
    return found


def follow(files, steps):
    """Edits a crate called `sinc` a step at a time under a daemon, against a fresh build of it."""
    work = scratch("blaze-includes-")
    ws = os.path.join(work, "ws")
    os.makedirs(os.path.join(ws, "src"))
    for path, text in (("Cargo.toml", TOML), *files):
        Path(ws, path).parent.mkdir(parents=True, exist_ok=True)
        Path(ws, path).write_text(text)
    # The directories outside the checkout that its files are in (`FAR_LABEL`, `FAR_BYTES`).
    dirs = sorted({os.path.dirname(os.path.normpath(os.path.join(ws, path))) for path, _ in files})
    project = Project(ws, work, outside=[it for it in dirs if it != ws and not it.startswith(ws + os.sep)])
    wrong = 0
    try:
        for what, edits, broken, *fresh in steps:
            for path, old, new in edits:
                path = os.path.join(ws, path)
                if old is None and new is None:
                    # Removed, whatever it holds.
                    os.unlink(path)
                elif old is None:
                    assert not os.path.exists(path), (what, path)
                    Path(path).write_text(new)
                elif new is None:
                    assert Path(path).read_text() == old, (what, path)
                    os.unlink(path)
                else:
                    text = Path(path).read_text()
                    assert old in text, (what, old)
                    Path(path).write_text(text.replace(old, new, 1))
            began = time.time()
            built = project.build("sinc")
            took = (time.time() - began) * 1000
            verdict, said = project.held(built, "sinc", broken=broken)
            if verdict == "refused" and not broken:
                # Both builds refused a step written to build: a blaze that builds nothing
                # would pass every step.
                verdict = "BOTH REFUSED"
            if fresh and verdict == "same" and kept(built.said, "sinc") is not fresh[0]:
                verdict = "RECOMPILED" if fresh[0] else "NOT REBUILT"
                said = f"unit kept: {kept(built.said, 'sinc')}, expected {fresh[0]}"
            wrong += verdict in WRONG or verdict in ("BOTH REFUSED", "RECOMPILED", "NOT REBUILT")
            print(
                f"{what:40} {verdict:16} {took:6.0f} ms  {said.splitlines()[0][:90] if said else ''}",
                flush=True,
            )
            if verdict == "DAEMON DIED":
                raise SystemExit(f"the daemon died: its log is in {project.home}")
    finally:
        project.stop()
    print(f"{len(steps) - wrong} of {len(steps)} right")
    if not wrong:
        shutil.rmtree(work, ignore_errors=True)
    sys.exit(1 if wrong else 0)


SCRIPT = "includes.py"


@dataclass(frozen=True)
class Input:
    """No field: the steps are STEPS."""


if __name__ == "__main__":
    match sys.argv[1:]:
        case [text]:
            parse(Input, text, SCRIPT)
        case _:
            fail(SCRIPT, f"usage: {SCRIPT} '{{}}' (see the top of the file)")
    follow(
        [
            (DATA, DATA_TEXT),
            (MAIN, MAIN_TEXT),
            (LABEL, "old label"),
            (BYTES, "old bytes"),
            (FAR_LABEL, "far label"),
            (FAR_BYTES, "far bytes"),
            (FAR_CODE, FAR_CODE_TEXT),
        ],
        STEPS,
    )
