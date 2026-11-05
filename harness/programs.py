#!/usr/bin/env python3
"""programs.py: two programs of one workspace built by one daemon.

The stress workspace gets a second binary. Each step edits files and then asks the daemon for
one program or both by `blaze build -p NAME`. Every program asked for is run and held to what a
fresh build of the same source does (rebuild.py).
"""
import os, shutil, sys, time

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
from rebuild import WRONG, Project, copy, edit, scratch

TOOL_TOML = """[package]
name = "stool"
version = "0.1.0"
edition = "2021"

[dependencies]
sbase = { path = "../base" }
smid = { path = "../mid" }
"""
TOOL_MAIN = """use sbase::shapes::{total_area, Shape};
use smid::Tile;

fn weight(n: u32) -> u32 {
    n * 3
}

fn main() {
    let shapes = vec![Shape::Circle(3), Shape::Dot];
    println!("tool total {}", total_area(&shapes));
    let tile = Tile { side: 4, label: "tool".to_string() };
    println!("tool tile {}", total_area(&[tile]));
    println!("tool weight {}", weight(7));
}
"""
MID, TOOL, APP = "mid/src/lib.rs", "tool/src/main.rs", "app/src/main.rs"
AREA = "sbase::square!(self.side)"

# (what the step is, [(file, old, new)], programs asked for in order, whether it is not Rust)
STEPS = [
    ("nothing edited, the second program asked for first time", [], ["stool"], False),
    ("nothing edited, both again", [], ["sapp", "stool", "stool"], False),
    ("code both use", [(MID, AREA, AREA + " + 1")], ["sapp", "stool"], False),
    ("code both use, the other asked first", [(MID, AREA + " + 1", AREA + " + 2")], ["stool", "sapp"], False),
    ("only the second program's code", [(TOOL, "n * 3", "n * 5")], ["stool", "sapp"], False),
    ("only the first program's code", [(APP, "items.iter().sum()", "items.iter().sum::<u32>() + 1")], ["stool", "sapp"], False),
    ("two edits before the second is asked", [(MID, AREA + " + 2", AREA + " + 3")], ["sapp"], False),
    ("the second catches up", [(TOOL, "n * 5", "n * 7")], ["stool", "sapp"], False),
    ("broken code both use", [(MID, AREA + " + 3", AREA + ' + "x"')], ["sapp", "stool", "sapp"], True),
    ("mended", [(MID, AREA + ' + "x"', AREA + " + 4")], ["stool", "sapp"], False),
    # A parenthesized macro call with no `;` after it is an expression, so the outer call's
    # expansion, two calls with nothing between, does not parse (#154).
    ("a macro of several statements in the parentheses of another's call, which does not parse",
     [(MID, "        " + AREA + " + 4\n",
       "        macro_rules! twice {\n            ($($body:tt)*) => { $($body)* $($body)* };\n        }\n"
       "        let mut n = 0;\n        twice!(twice!(n += 1;));\n        " + AREA + " + 4 + n\n")],
     ["sapp", "stool"], True),
    ("the calls braced, which both accept", [(MID, "twice!(twice!(n += 1;));", "twice! { twice! { n += 1; } }")], ["stool", "sapp"], False),
    ("a function added to the second", [(TOOL, "fn main() {", "fn extra() -> u32 {\n    41\n}\n\nfn main() {\n    println!(\"extra {}\", extra());")], ["stool", "sapp"], False),
]


def main():
    work = scratch("blaze-programs-")
    ws = copy(os.path.join(HERE, "stress"), os.path.join(work, "ws"))
    edit(ws, "tool/Cargo.toml", None, TOOL_TOML)
    edit(ws, TOOL, None, TOOL_MAIN)
    edit(ws, "Cargo.toml", '"app"]', '"app", "tool"]')
    project = Project(ws, work)
    wrong = asked = 0
    try:
        for what, edits, order, broken in STEPS:
            for path, old, new in edits:
                edit(ws, path, old, new)
            print(what, flush=True)
            for name in order:
                began = time.time()
                built = project.build(name, "-p", name)
                took = (time.time() - began) * 1000
                verdict, said = project.held(built, name, "-p", name, broken=broken)
                asked += 1
                wrong += verdict in WRONG
                print(f"    {name:6} {verdict:16} {took:6.0f} ms  {said.splitlines()[0][:90] if said else ''}", flush=True)
                if verdict == "DAEMON DIED":
                    raise SystemExit(f"the daemon died: its log is in {project.home}")
    finally:
        project.stop()
    print(f"{asked - wrong} of {asked} right")
    if not wrong:
        shutil.rmtree(work, ignore_errors=True)
    sys.exit(1 if wrong else 0)


if __name__ == "__main__":
    main()
