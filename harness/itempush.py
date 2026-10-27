#!/usr/bin/env python3
"""itempush.py: an edit that changes items asks again about what refers to what changed, and
what the program does is still what cargo's build of it does (#344).

usage: itempush.py    (RA_JIT_BIN names the ra-jit built; cargo-ra-jit is found next to it)

A toy workspace of a library and a program is built and loaded, and then edited step by step
in the ways that move what a name is bound to: imports reordered, a glob import shadowed and
the shadow taken away, a re-export bound to another function, a function moved to another
module, a function made private so that the glob no longer brings it, an inherent method that
takes a call from a trait's, a trait's default method called directly and through `dyn`, and
a file written again with nothing changed, a module added that imports a macro a module that
stays binds as well, and a `#[macro_use]` module added whose `macro_rules!` a call in a file
that is not edited now names instead of std's, and then not, and the import of a macro taken
away from a module that is edited, an inherent constant that a path to the trait's now names,
moved to a new file and taken away. After each step
the program the daemon built prints what cargo's prints, or both builds fail.

The steps run twice, each time in a workspace of its own: with RA_JIT_CHECK_ALL=1, where a
patch asks about everything and says `check-all: MISSED` for what the items left out, and
without it, where what is asked is what the items say.

Python 3, stdlib only.
"""
import glob, os, re, subprocess, sys, tempfile, time

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import cargo_ra_jit
from cargo_ra_jit import Side, wait_warm

LIB = """mod a;
mod b;
pub mod shapes;

use a::*;
pub use b::scale;

pub fn pick() -> u32 {
    value() + tag!() + e::at() + shapes::Sq::SIDES
}

pub trait Sides {
    const SIDES: u32 = 4;
}

pub trait Area {
    fn area(&self) -> u32;
    fn twice(&self) -> u32 {
        self.area() * 2
    }
}

mod e;
pub use e::{here, named};
pub use e::same;
"""
# Functions moved to another file and back: one that tells where it is, which calls others moved
# with it, and one that names itself by its path, as does one it calls.
MOVED = """
pub fn here() -> String {
    let at = std::panic::Location::caller();
    format!("{}:{}:{} {}", at.file(), at.line(), inner(), spot())
}

fn inner() -> u32 {
    5
}

fn spot() -> String {
    format!("{}:{}", file!(), line!())
}

pub fn named() -> String {
    format!("{} {}", std::any::type_name_of_val(&named), kind())
}

fn kind() -> &'static str {
    std::any::type_name_of_val(&kind)
}
"""
# Enums whose derived `eq`s are written alike, moved to another file and back with what calls them:
# only the impl each is of tells them apart.
TWINS = """
#[derive(PartialEq)]
pub enum Dir {
    Up,
    Down,
}

#[derive(PartialEq)]
pub enum Mode {
    Fast,
    Slow,
}

pub fn same() -> u32 {
    u32::from(Dir::Up == Dir::Down) + 2 * u32::from(Mode::Slow == Mode::Slow)
}
"""
# A trait's impl, which a call through `dyn` takes too, moved to another file and back.
AREA = "impl crate::Area for crate::shapes::Sq {\n    fn area(&self) -> u32 {\n        self.0 * self.0\n    }\n}\n"
FILES = {
    "Cargo.toml": '[workspace]\nmembers = ["app", "lib"]\nresolver = "2"\n',
    "lib/Cargo.toml": '[package]\nname = "lib"\nversion = "0.1.0"\nedition = "2021"\n',
    "lib/src/lib.rs": LIB,
    "lib/src/a.rs": "pub fn value() -> u32 {\n    1\n}\n\npub fn other() -> u32 {\n    10\n}\n\nmacro_rules! tag {\n    () => {\n        20\n    };\n}\npub(crate) use tag;\n",
    "lib/src/e.rs": "pub fn at() -> u32 {\n    line!()\n}\n" + MOVED + TWINS,
    "lib/src/b.rs": "pub fn scale(x: u32) -> u32 {\n    x * 3\n}\n\npub fn value() -> u32 {\n    2\n}\n",
    "lib/src/shapes.rs": "pub struct Sq(pub u32);\n\nimpl crate::Sides for Sq {}\n\n" + AREA,
    "app/Cargo.toml": '[package]\nname = "app"\nversion = "0.1.0"\nedition = "2021"\n\n[dependencies]\nlib = { path = "../lib" }\n',
    "app/src/main.rs": 'use lib::shapes::Sq;\nuse lib::{pick, scale, Area};\n\nfn main() {\n    let dynamic: &dyn Area = &Sq(3);\n    println!("{} {} {} {} {}", pick(), scale(2), Sq(3).area(), Sq(3).twice(), dynamic.twice());\n    println!("{} {} {}", lib::here(), lib::named(), lib::same());\n}\n',
}
LIB_RS, A_RS, B_RS, SHAPES_RS = "lib/src/lib.rs", "lib/src/a.rs", "lib/src/b.rs", "lib/src/shapes.rs"
C_RS, D_RS, E_RS, F_RS = "lib/src/c.rs", "lib/src/d.rs", "lib/src/e.rs", "lib/src/f.rs"
TO_SHAPES = (LIB_RS, "pub use e::{here, named};", "pub use shapes::{here, named};")
TO_E = (LIB_RS, "pub use shapes::{here, named};", "pub use e::{here, named};")
# A file of the crate's directory that no module is written in, which the crate doesn't build.
STRAY = "lib/src/stray.rs"
# A line of `here` written over two, as a formatter does.
AT_SPLIT = "    let at =\n        std::panic::Location::caller();\n"
# An edit whose file the daemon sees empty first, as a write that truncates it shows it.
EMPTY_FIRST = "empty first"
SIDES = "\nimpl Sq {\n    pub const SIDES: u32 = 40;\n}\n"
INHERENT = "\nimpl Sq {\n    pub fn area(&self) -> u32 {\n        7\n    }\n}\n"
# (step, edits); an edit is (file, old, new), with `None` for old to write the file whole.
STEPS = [
    ("imports reordered", [(LIB_RS, "use a::*;\npub use b::scale;\n", "pub use b::scale;\nuse a::*;\n")]),
    ("glob shadowed", [(LIB_RS, "use a::*;\n", "use a::*;\nuse b::value;\n")]),
    ("shadow taken away", [(LIB_RS, "use a::*;\nuse b::value;\n", "use a::*;\n")]),
    ("re-export bound to another", [
        (A_RS, "pub fn other", "pub fn scale(x: u32) -> u32 {\n    x + 100\n}\n\npub fn other"),
        (LIB_RS, "pub use b::scale;\n", "pub use a::scale;\n"),
    ]),
    ("moved to another module", [
        (A_RS, "pub fn value() -> u32 {\n    1\n}\n", "pub use crate::shapes::value;\n"),
        (SHAPES_RS, "pub struct Sq", "pub fn value() -> u32 {\n    1\n}\n\npub struct Sq"),
    ]),
    ("made private", [(SHAPES_RS, "pub fn value", "fn value")]),
    ("made public again", [(SHAPES_RS, "fn value() -> u32 {\n    1\n}\n\npub struct", "pub fn value() -> u32 {\n    1\n}\n\npub struct")]),
    ("inherent method", [(SHAPES_RS, None, lambda text: text + INHERENT)]),
    ("inherent method taken away", [(SHAPES_RS, INHERENT, "")]),
    ("trait default", [(LIB_RS, "self.area() * 2", "self.area() * 4")]),
    ("written again", [(LIB_RS, None, lambda text: text)]),
    ("module added importing a macro", [
        (C_RS, None, lambda _: "use crate::a::tag;\n\npub fn more() -> u32 {\n    tag!() + 40\n}\n"),
        (LIB_RS, "mod b;\n", "mod b;\nmod c;\n"),
    ]),
    ("`#[macro_use]` module added", [
        (D_RS, None, lambda _: "macro_rules! line {\n    () => {\n        500\n    };\n}\n"),
        (LIB_RS, "pub fn pick", "#[macro_use]\nmod d;\n\npub fn pick"),
    ]),
    ("`#[macro_use]` taken off", [(LIB_RS, "#[macro_use]\nmod d;", "mod d;")]),
    ("a macro's import taken away", [(C_RS, "use crate::a::tag;\n\n", ""), (C_RS, "tag!()", "crate::a::tag!()")]),
    ("inherent const shadows the trait's", [(SHAPES_RS, None, lambda text: text + SIDES)]),
    ("inherent impl moved to a new file", [
        (SHAPES_RS, SIDES, ""),
        (F_RS, None, lambda _: SIDES.replace("impl Sq", "impl crate::shapes::Sq").lstrip()),
        (LIB_RS, "mod e;\n", "mod e;\nmod f;\n"),
    ]),
    ("inherent impl taken away", [(F_RS, None, lambda _: "")]),
    ("a file no module is written in", [(STRAY, None, lambda _: "pub fn pick() -> u32 {\n    9\n}\n")]),
    ("it written again", [(STRAY, "9", "90")]),
    ("functions moved to another file", [(E_RS, MOVED, ""), (SHAPES_RS, None, lambda t: t + MOVED), TO_SHAPES]),
    ("an edit after a move", [(B_RS, "x * 3", "x * 5")]),
    ("moved back", [(SHAPES_RS, MOVED, ""), (E_RS, None, lambda t: t + MOVED), TO_E]),
    ("moved with a callee changed", [
        (E_RS, MOVED, ""),
        (SHAPES_RS, None, lambda t: t + MOVED.replace("    5\n", "    6\n")),
        TO_SHAPES,
    ]),
    ("a trait's impl moved to another file", [(SHAPES_RS, AREA, ""), (E_RS, None, lambda t: t + AREA)]),
    ("it moved back", [(E_RS, AREA, ""), (SHAPES_RS, None, lambda t: t + AREA)]),
    ("twins moved to another file", [
        (E_RS, TWINS, ""),
        (SHAPES_RS, None, lambda t: t + TWINS),
        (LIB_RS, "pub use e::same;", "pub use shapes::same;"),
    ]),
    ("twins moved back", [
        (SHAPES_RS, TWINS, ""),
        (E_RS, None, lambda t: t + TWINS),
        (LIB_RS, "pub use shapes::same;", "pub use e::same;"),
    ]),
    ("a constant shadowed, its file seen empty first", [(SHAPES_RS, None, lambda text: text + SIDES, EMPTY_FIRST)]),
    ("it taken away, its file seen empty first", [(SHAPES_RS, SIDES, "", EMPTY_FIRST)]),
    ("a function written over more lines, above others", [(E_RS, "u32 {\n    line!()", "u32 {\n\n    line!()")]),
    ("a line above functions that tell where they are", [(SHAPES_RS, "Sq(pub u32);\n", "Sq(pub u32);\n\n")]),
    ("one written over more lines within", [(SHAPES_RS, "    let at = std::panic::Location::caller();\n", AT_SPLIT)]),
    ("all written as they were", [
        (E_RS, "u32 {\n\n    line!()", "u32 {\n    line!()"),
        (SHAPES_RS, "Sq(pub u32);\n\n", "Sq(pub u32);\n"),
        (SHAPES_RS, AT_SPLIT, "    let at = std::panic::Location::caller();\n"),
    ]),
]
# Steps whose moved functions keep their code from before the edit, which items find.
RELOCATES = {"functions moved to another file", "twins moved to another file"}


def logs(home):
    return "".join(open(log, errors="replace").read() for log in sorted(glob.glob(os.path.join(home, "*", "*", "log"))))


def wait_items(home, timeout=30):
    """Waits until the daemon has the items of its last build: it makes them once it has been
    idle a while, and again after an edit it couldn't push by them."""
    began = time.time()
    while time.time() - began < timeout:
        log = logs(home)
        made = log.rfind("\nitems-us: ")
        lost = max(log.rfind("\nitems: whole crates: no items"), log.rfind("\nitems: not made"))
        if made >= 0 and made > lost:
            return
        time.sleep(0.1)


def ran(exe):
    return subprocess.run([exe], capture_output=True, text=True, timeout=60).stdout.strip()


def steps(check_all):
    bin_dir = os.path.dirname(os.path.abspath(os.environ["RA_JIT_BIN"]))
    root = os.path.realpath(tempfile.mkdtemp(prefix="ra-jit-itempush-", dir="/tmp"))
    ws, home = os.path.join(root, "ws"), os.path.join(root, "home")
    for path, text in FILES.items():
        os.makedirs(os.path.dirname(os.path.join(ws, path)), exist_ok=True)
        open(os.path.join(ws, path), "w").write(text)
    env = cargo_ra_jit.base_env(home)
    env["PATH"] = bin_dir + os.pathsep + env["PATH"]
    for name in ("RUSTC_WRAPPER", "CARGO_INCREMENTAL", "RA_JIT_SPARE"):
        env.pop(name, None)
    env["RA_JIT_STATS"] = "1"
    if check_all:
        env["RA_JIT_CHECK_ALL"] = "1"
    side, reference = Side(ws, env, True), Side(ws, env, False)
    ours_exe, theirs_exe = (os.path.join(ws, it, "debug", "app") for it in ("target", "target-ref"))
    mode = "check-all" if check_all else "items"
    wrong, pushed = [], 0
    try:
        _, built = side.cargo("build")
        if built.returncode != 0:
            raise SystemExit(f"the cold build failed:\n{(built.stdout + built.stderr)[-3000:]}")
        took, states = wait_warm(side, ws)
        if states != ["ready"]:
            raise SystemExit(f"the daemon did not get ready: {states}")
        for step, edits in STEPS:
            wait_items(home)
            seen = len(logs(home))
            # Every file is written as one change, one right after another: a daemon that
            # noticed the first alone would take a move as an item gone and, later, one added.
            texts, empty_first = {}, []
            for path, old, new, *how in edits:
                path = os.path.join(ws, path)
                text = texts[path] if path in texts else open(path).read() if os.path.exists(path) else ""
                if old is None:
                    text = new(text)
                elif old not in text:
                    raise SystemExit(f"{step}: {old!r} is not in {path}")
                else:
                    text = text.replace(old, new, 1)
                texts[path] = text
                if EMPTY_FIRST in how:
                    empty_first.append(path)
            for path in empty_first:
                open(path, "w").close()
            if empty_first:
                time.sleep(0.5)
            for path, text in texts.items():
                open(path, "w").write(text)
            _, ours = side.cargo("build")
            _, theirs = reference.cargo("build")
            said = ours.stdout + ours.stderr
            log = logs(home)[seen:]
            right = (ours.returncode == 0) == (theirs.returncode == 0)
            outputs = ""
            if ours.returncode == 0 and theirs.returncode == 0:
                right &= side.how_built(ours.stderr) == "daemon"
                mine, cargo = ran(ours_exe), ran(theirs_exe)
                right &= mine == cargo
                outputs = f"; printed {mine!r}, cargo's {cargo!r}"
            missed = re.findall(r"^check-all: MISSED .*$", log, re.M)
            right &= not missed
            how = re.findall(r"^items: (whole crates: .*|\d+ written, \d+ changed, \d+ names, .*?) in \d+ us$", log, re.M)
            pushed += any(not it.startswith("whole") for it in how)
            relocated = re.findall(r"^relocated: (\d+) instances, (\d+) moved now by their code.*$", log, re.M)
            if step in RELOCATES and not check_all:
                right &= any(int(kept) > 0 for _, kept in relocated)
            detail = f"cargo exit {theirs.returncode}, ours {ours.returncode}{outputs}; items {how}"
            if relocated:
                detail += f"; relocated {relocated}"
            if missed:
                detail += f"; {missed[:3]}"
            print(f"{mode:9} {step:27} {'ok' if right else 'WRONG':6} {detail}")
            if not right:
                items = "\n".join(re.findall(r"^(?:items-\w+|relocat\w+|patch-made): .*$", log, re.M))
                wrong.append(f"{mode} {step}: {detail}\n{items}\n{said[-1500:]}")
    finally:
        side.cargo("stop")
    # A run where no edit was pushed by its items tests nothing of them.
    if not pushed:
        wrong.append(f"{mode}: no step was pushed by its items")
    return wrong


def main():
    wrong = steps(True) + steps(False)
    if wrong:
        raise SystemExit("\n".join(wrong))


if __name__ == "__main__":
    main()
