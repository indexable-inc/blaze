#!/usr/bin/env python3
"""storetracked.py: a v2 daemon that starts on a store holding the checkout's code builds the
checkout right, and every edit after it too (#105, #216). Linux and macOS.

    uv run --frozen python harness/storetracked.py '{"blaze": "target/release/blaze"}'

`blaze` is the v2 command line. The input is one JSON object, inline, a file's path, or `-`.

A daemon builds the workspace into an empty store and stops. A second daemon in the same home,
with the VFS's journal and the database's image removed so all it has is the store, builds the
same checkout; a third, in a home of its own whose store is a copy of the first's, builds a copy
of the checkout at another path. Each then takes every edit of STEPS in turn: a generic
function's body, whose instance is the checkout's and was in the store; lines and closures above
a closure, which leave or take its place among its body's closures; a closure's body; a static's
value; the program's own text; an `#[inline]` function whose code is in its callers' crates; the
closure an `impl Trait` is, whose drop glue in the store names none of it; and all of it taken
back. After each, the build has to succeed and the program print what the source says: no code
the store kept for the edit's old source may be used for its new one.
"""
import os, shutil, sys, time
from dataclasses import dataclass

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
from typed_input import fail, parse
from storelib import Daemon, Failed, blaze_of, forget, scratch, store_of

SCRIPT = "storetracked.py"
PROGRAM = "tapp"
LIB, APP = "lib/src/lib.rs", "app/src/main.rs"
FILES = {
    "Cargo.toml": '[workspace]\nmembers = ["app", "lib"]\nresolver = "2"\n',
    "lib/Cargo.toml": '[package]\nname = "tlib"\nversion = "0.1.0"\nedition = "2021"\n',
    LIB: """pub static BASE: u64 = 10;

pub fn scale<T: Into<u64>>(x: T) -> u64 {
    x.into() * 3
}

pub fn each<T: Copy + Into<u64>>(v: &[T]) -> u64 {
    v.iter().map(|&x| x.into() + 1).sum()
}

pub fn total(v: &[u64]) -> u64 {
    walk(v, &|x| x * 2)
}

fn walk<F: Fn(u64) -> u64>(v: &[u64], f: &F) -> u64 {
    match v {
        [] => 0,
        [x, rest @ ..] => f(*x) + walk(rest, f),
    }
}

pub struct Span {
    pub start: u64,
}

impl Span {
    // rustc copies it into each crate that calls it, and a generic function's instance of
    // another crate calls that copy.
    #[inline]
    pub fn start(&self) -> u64 {
        self.start
    }
}

pub fn starts<T: Copy + Into<u64>>(v: &[T]) -> u64 {
    v.iter().map(|&x| Span { start: x.into() }.start()).sum()
}

// Its caller drops what it gives, whose drop glue's name says `impl Fn` and not the closure.
pub fn label(n: u64) -> impl Fn() -> usize {
    let s = "x".repeat(n as usize);
    move || s.len()
}
""",
    "app/Cargo.toml": '[package]\nname = "tapp"\nversion = "0.1.0"\nedition = "2021"\n\n'
                      '[dependencies]\ntlib = { path = "../lib" }\n',
    APP: """fn main() {
    println!("{} {} {} {} {} {} {}", "a", tlib::scale(2u8), tlib::each(&[1u32, 2]), tlib::BASE,
             tlib::total(&[1, 2]), tlib::starts(&[3u16, 4]), tlib::label(3)());
}
""",
}
FIRST = "a 6 5 10 6 7 3"
# (what the step is, [(file, old, new)], what the program prints after it)
STEPS = [
    ("a generic function's body", [(LIB, "x.into() * 3", "x.into() * 4")], "a 8 5 10 6 7 3"),
    # A closure is named by its place among the closures of its body, which a line above it
    # leaves: the instances generic over it, from the store, are as they were.
    ("a line above a closure", [(LIB, "    v.iter()", "    let _ = 0u64;\n    v.iter()")], "a 8 5 10 6 7 3"),
    # The same of a recursive function's instance, which calls itself.
    ("a line above a closure a recursive function takes", [(LIB, "    walk(v", "    let _ = 1u64;\n    walk(v")],
     "a 8 5 10 6 7 3"),
    # A closure added above one takes its place: the one that was there and the instances
    # generic over it are other code, and the ones from the store are called no more.
    ("a closure above a closure", [(LIB, "    let _ = 0u64;\n", "    let _ = (|| 0u64)();\n")], "a 8 5 10 6 7 3"),
    ("a closure's body", [(LIB, "x.into() + 1", "x.into() + 2")], "a 8 7 10 6 7 3"),
    ("a static's value", [(LIB, "BASE: u64 = 10", "BASE: u64 = 12")], "a 8 7 12 6 7 3"),
    ("the program's own text", [(APP, '"a"', '"b"')], "b 8 7 12 6 7 3"),
    # An `#[inline]` function, whose code is in each crate that calls it: its callers from the
    # store have to call the new one (#216).
    ("a function rustc copies into its callers", [(LIB, "        self.start\n", "        self.start + 1\n")],
     "b 8 7 12 6 9 3"),
    # The closure an `impl Trait` is captures more, and the drop glue that drops it from the
    # store is not the code it was, though no name it mentions changed.
    ("what an impl Trait is", [(LIB, "    move || s.len()\n", "    let t = s.clone();\n    move || s.len() + t.len()\n")],
     "b 8 7 12 6 9 6"),
    ("all of it taken back", [(LIB, "x.into() * 4", "x.into() * 3"), (LIB, "    let _ = (|| 0u64)();\n", ""),
                              (LIB, "    let _ = 1u64;\n", ""), (LIB, "x.into() + 2", "x.into() + 1"),
                              (LIB, "BASE: u64 = 12", "BASE: u64 = 10"), (APP, '"b"', '"a"'),
                              (LIB, "        self.start + 1\n", "        self.start\n"),
                              (LIB, "    let t = s.clone();\n    move || s.len() + t.len()\n", "    move || s.len()\n")],
     FIRST),
]


@dataclass(frozen=True)
class Input:
    blaze: str


def write(ws):
    for path, text in FILES.items():
        os.makedirs(os.path.dirname(os.path.join(ws, path)), exist_ok=True)
        open(os.path.join(ws, path), "w").write(text)


def edit(ws, what, edits):
    for file, old, new in edits:
        path = os.path.join(ws, file)
        text = open(path).read()
        if old not in text:
            raise Failed(f"{what}: {file} has no {old!r}")
        open(path, "w").write(text.replace(old, new, 1))


def printed(daemon, ws, what):
    """What the program prints once the daemon has built `ws`; a line of what went wrong when
    the build failed."""
    built = daemon.build(ws, program=PROGRAM)
    if built.status != 0 or not built.executable:
        return f"BUILD ENDED {built.status}: {built.said[-1500:]}"
    return (built.program() or "").strip()


def steps(daemon, ws, label):
    """The checkout as it is, then every step: how many printed other than the source says."""
    wrong = 0
    got = printed(daemon, ws, f"{label}: the start")
    verdict = "ok" if got == FIRST else f"PRINTED {got!r}, NOT {FIRST!r}"
    wrong += verdict != "ok"
    print(f"{label + ': the start':56} {verdict}", flush=True)
    for what, edits, expected in STEPS:
        edit(ws, what, edits)
        got = printed(daemon, ws, f"{label}: {what}")
        verdict = "ok" if got == expected else f"PRINTED {got!r}, NOT {expected!r}"
        wrong += verdict != "ok"
        print(f"{label + ': ' + what:56} {verdict}", flush=True)
    return wrong


def started(blaze, home, name):
    daemon = Daemon(blaze, home, name=name, wait=False)
    if not daemon.wait():
        raise Failed(f"{name}: the daemon did not start on the store:\n{daemon.log()[-2000:]}\nlog: {daemon.log_path}")
    return daemon


def main(args):
    if sys.platform not in ("linux", "darwin"):
        fail(SCRIPT, f"Linux and macOS only, not {sys.platform}")
    blaze = blaze_of(args.blaze)
    start = time.time()
    root = scratch("blaze-tracked-")
    ws, copy = os.path.join(root, "ws"), os.path.join(root, "copy")
    here, there = os.path.join(root, "h"), os.path.join(root, "h2")
    write(ws)
    wrong = 0
    try:
        daemon = started(blaze, here, "fill")
        got = printed(daemon, ws, "filling the store")
        daemon.stop()
        if got != FIRST:
            raise Failed(f"filling the store: the program printed {got!r}, not {FIRST!r}")
        shutil.copytree(ws, copy, ignore=shutil.ignore_patterns("target"))
        shutil.copytree(store_of(here), store_of(there))
        forget(here)
        daemon = started(blaze, here, "restart")
        wrong += steps(daemon, ws, "restart")
        daemon.stop()
        daemon = started(blaze, there, "another")
        wrong += steps(daemon, copy, "another checkout")
        daemon.stop()
    except Failed as failed:
        sys.exit(f"{SCRIPT}: {failed}\n  kept: {root}")
    print(f"{wrong} wrong in {time.time() - start:.0f}s; under {root}")
    if wrong:
        sys.exit(f"{SCRIPT}: {wrong} wrong")
    shutil.rmtree(root, ignore_errors=True)


if __name__ == "__main__":
    if len(sys.argv) != 2:
        sys.exit(f"usage: {SCRIPT} '<json>'\n\n{__doc__}")
    main(parse(Input, sys.argv[1], SCRIPT))
