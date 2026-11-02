#!/usr/bin/env python3
# pyright: basic
"""includes.py: a daemon follows edits to a file that only `include!` names.

The file is not a `.rs` file, so nothing loads it until the macro is expanded. Each step edits
it or the file that includes it, asks the daemon for the program by `blaze build` and holds it
to what a fresh build of the same source does (rebuild.py).

usage: includes.py '{}'

Its steps are fixed, so its input is the empty JSON object (inline, the path of a file, or `-`
for standard input): any field is refused. Run it in the repository's Python workspace: `uv run
--frozen python harness/includes.py '{}'` from the root. The scripts that import `follow` run
steps of their own.
"""

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
}
"""
DATA, MAIN = "src/table.rs.data", "src/main.rs"
LABEL, BYTES = "src/label.txt", "src/bytes.dat"
# (what the step is, [(file, old, new)], whether it does not build). An `old` of None makes the
# file, a `new` of None removes it.
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
]


def follow(files, steps):
    """Edits a crate called `sinc` a step at a time under a daemon, against a fresh build of it."""
    work = scratch("blaze-includes-")
    ws = os.path.join(work, "ws")
    os.makedirs(os.path.join(ws, "src"))
    for path, text in (("Cargo.toml", TOML), *files):
        Path(ws, path).write_text(text)
    project = Project(ws, work)
    wrong = 0
    try:
        for what, edits, broken in steps:
            for path, old, new in edits:
                path = os.path.join(ws, path)
                if old is None:
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
            wrong += verdict in WRONG
            print(f"{what:40} {verdict:16} {took:6.0f} ms  {said.splitlines()[0][:90] if said else ''}", flush=True)
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
    follow([(DATA, DATA_TEXT), (MAIN, MAIN_TEXT), (LABEL, "old label"), (BYTES, "old bytes")], STEPS)
