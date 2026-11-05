#!/usr/bin/env python3
# pyright: basic
"""outdirwarn.py: a warning in a file a build script writes is shown with its source line.

The crate's build script writes `$OUT_DIR/gen.rs` with an unused variable in it, and the crate
takes it in with `include!(concat!(env!("OUT_DIR"), "/gen.rs"))`. No mount of the build holds
`OUT_DIR`, so the text the warning is shown from is the one the check read. The build must
succeed and say the warning as rustc does, with the line of `gen.rs` it is on and where that
is; a daemon that dies writing it fails the run.

usage: outdirwarn.py '{}'

Its fixture is fixed, so its input is the empty JSON object (inline, the path of a file, or `-`
for standard input): any field is refused. Run it in the repository's Python workspace: `uv run
--frozen python harness/outdirwarn.py '{}'` from the root.
"""

import json
import os
import shutil
import sys
from dataclasses import dataclass
from pathlib import Path

from typed_input import fail, parse

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
from rebuild import Project, scratch

TOML = """[package]
name = "owarn"
version = "0.1.0"
edition = "2021"
"""
BUILD = """use std::{env, fs, path::Path};

fn main() {
    let out = env::var("OUT_DIR").unwrap();
    fs::write(Path::new(&out).join("gen.rs"), "pub fn made() -> u32 {\\n    let unused = 1;\\n    7\\n}\\n").unwrap();
}
"""
MAIN = """include!(concat!(env!("OUT_DIR"), "/gen.rs"));

fn main() {
    println!("{}", made());
}
"""
SAID = "unused variable: `unused`"
LINE = "let unused = 1;"


def rendered(said, level=None):
    """The rendered text of each compiler message in a `--message-format=json` build's output,
    of `level` (`"error"`, ...) or of any."""
    out = []
    for line in said.splitlines():
        try:
            message = json.loads(line)
        except ValueError:
            continue
        if isinstance(message, dict) and message.get("reason") == "compiler-message":
            inner = message.get("message", {})
            if level is None or inner.get("level") == level:
                out.append(inner.get("rendered") or "")
    return out


def failed(said):
    """What a failed build said: its errors, each in full, then the end of its output."""
    errors = "".join(rendered(said, "error"))
    return f"{errors}\n{said[-2000:]}" if errors else said[-2000:]


def run():
    work = scratch("blaze-outdirwarn-")
    ws = os.path.join(work, "ws")
    os.makedirs(os.path.join(ws, "src"))
    for path, text in (("Cargo.toml", TOML), ("build.rs", BUILD), ("src/main.rs", MAIN)):
        Path(ws, path).write_text(text)
    project = Project(ws, work)
    wrong = []
    try:
        built = project.build("owarn")
        if project.daemon is None or not project.daemon.alive():
            raise SystemExit(f"the daemon died: its log is in {project.home}")
        if built.status != 0:
            wrong.append(f"the build failed ({built.status}):\n{failed(built.said)}")
        warnings = [it for it in rendered(built.said) if SAID in it]
        if not warnings:
            wrong.append(f"no warning `{SAID}`:\n{failed(built.said)}")
        for it in warnings:
            if LINE not in it or "/out/gen.rs:2:9" not in it:
                wrong.append(f"the warning is not shown at its line of `gen.rs`:\n{it}")
    finally:
        project.stop()
    for it in wrong:
        print(it, flush=True)
    print("right" if not wrong else f"{len(wrong)} wrong")
    if not wrong:
        shutil.rmtree(work, ignore_errors=True)
    sys.exit(1 if wrong else 0)


SCRIPT = "outdirwarn.py"


@dataclass(frozen=True)
class Input:
    """No field: the fixture is fixed."""


if __name__ == "__main__":
    match sys.argv[1:]:
        case [text]:
            parse(Input, text, SCRIPT)
            run()
        case _:
            fail(SCRIPT, f"usage: {SCRIPT} '{{}}' (see the top of the file)")
