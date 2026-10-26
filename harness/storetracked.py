#!/usr/bin/env python3
"""storetracked.py: a daemon that starts on a store holding the checkout's code takes that
code as it is and patches the first edit to it, with nothing of rustc's (#105). Linux and
macOS: on Windows `cargo ra-jit` hands builds to cargo.

usage: storetracked.py    (RA_JIT_BIN names the ra-jit built; cargo-ra-jit and ra-jit-rustc
                           are found next to it)

A patch is cut from what changed since the build it patches, and it knew that by marks
computed in its own process: a mark of code the store had came from the process that wrote
it, so the daemon built everything of the checkout the store gave it again before the first
edit could be patched ("compiling what the store had of the checkout"), all of nushell's
73,586 functions for a start with nothing changed. Each function of the checkout now goes
into the store with a mark of its code and of what it names by stable names, the same in any
process, checkout or build of ra-jit, so the base takes what it found as it is and an edit
recompiles only what it touched.

A daemon builds the workspace into an empty store and stops. A second one on the same
checkout, and a third on a copy of it elsewhere with a home of its own, start on that store:
each has to take the checkout's functions from it, compile none of them again, and splice
every edit after: a generic function's body, whose instance is the checkout's and was found
in the store, a closure's, a static's value, the program's own text, an `#[inline]` function
whose callers from the store call rustc's copy of it in the program's crate, the closure an
`impl Trait` is, whose drop glue from the store names none of it, and all of it taken back.
The program has to print what the source says, and no call of rustc's copy of a function of
the checkout may be left out of the store (#216).

An edit of one body takes the other code from the store of that file as it was, by the items
each entry names reading as they did then, without compiling it to tell: the second daemon has
to take some so on those edits, and the third, which compiles everything to check
(`RA_JIT_CHECK_ALL`), must find none of what it would have taken changed.

Python 3, stdlib only.
"""
import glob
import os
import re
import shutil
import subprocess
import sys
import tempfile
import time

HERE = os.path.dirname(os.path.abspath(__file__))
os.environ.setdefault("RUSTUP_TOOLCHAIN", open(os.path.join(HERE, "toolchain")).read().strip())
TIMEOUT = 900
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
    # leaves: the instances generic over it are the base's, from the store, as they were.
    ("a line above a closure", [(LIB, "    v.iter()", "    let _ = 0u64;\n    v.iter()")], "a 8 5 10 6 7 3"),
    # The same of a recursive function's instance, which calls itself.
    ("a line above a closure a recursive function takes", [(LIB, "    walk(v", "    let _ = 1u64;\n    walk(v")],
     "a 8 5 10 6 7 3"),
    # A closure added above one takes its place, and is named apart from it by what is
    # written of it: the one that was there and the instances generic over it get new names,
    # and the ones from the store are called no more.
    ("a closure above a closure", [(LIB, "    let _ = 0u64;\n", "    let _ = (|| 0u64)();\n")], "a 8 5 10 6 7 3"),
    ("a closure's body", [(LIB, "x.into() + 1", "x.into() + 2")], "a 8 7 10 6 7 3"),
    ("a static's value", [(LIB, "BASE: u64 = 10", "BASE: u64 = 12")], "a 8 7 12 6 7 3"),
    ("the program's own text", [(APP, '"a"', '"b"')], "b 8 7 12 6 7 3"),
    # Its callers from the store call rustc's copy of it in the program's crate (#216).
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
REBUILT = "compiling what the store had of the checkout"
# The steps whose patch compiles only code the base has, by the names it had, and the one
# that renames some of it, whose patch compiles that under the new names.
KEEPING = {"a generic function's body", "a line above a closure", "a line above a closure a recursive function takes"}
RENAMING = {"a closure above a closure"}
# The steps that edit one body of a file whose other code the store had: the patch takes that
# code as it was by its items, which read as they did, and compiles none of it to tell.
UNCHANGED = {"a generic function's body", "a closure's body"}
# What the daemon says under RA_JIT_CHECK_ALL of code it would have taken as it was by its
# items and that changed.
MISSED = "its items unchanged"
# What a call of rustc's copy of a function of the checkout was left out of the store as (#216).
UNKEPT = "rustc's copy of a function of the checkout"


def build(ws, env, step):
    ran = subprocess.run(["cargo", "ra-jit", "build"], cwd=ws, env=env, capture_output=True, text=True,
                         timeout=TIMEOUT)
    said = ran.stdout + ran.stderr
    if ran.returncode != 0:
        raise SystemExit(f"{step}: the build failed:\n{said[-3000:]}")
    return said


class Logs:
    """What the workers of a daemon about to start log. A worker that was a spare keeps the
    spare's log, and a worker of the same build as one before it writes the same file anew,
    so what the daemons before logged is moved aside."""

    def __init__(self, home):
        self.home = home
        for path in self.paths():
            os.replace(path, f"{path}.before")

    def paths(self):
        return sorted(glob.glob(os.path.join(self.home, "*", "*", "log")))

    def text(self):
        said = ""
        for path in self.paths():
            with open(path, errors="replace") as log:
                said += log.read()
        return said

    def links(self):
        return re.findall(r"^link: \S+ \((.*)\)$", self.text(), re.M)

    def anew(self):
        """How many instances each patch compiled that the base had no code for."""
        return [int(n) for n in re.findall(r"^patch: (\d+) instances the base has no code for$", self.text(), re.M)]

    def unchanged(self):
        """How much code each patch took as it was, its items reading as they did."""
        return [int(r) + int(i) for r, i in
                re.findall(r"unchanged by their items: (\d+) roots, (\d+) instances", self.text())]


def program(ws):
    return subprocess.run([os.path.join(ws, "target", "debug", "tapp")], capture_output=True, text=True,
                          timeout=60).stdout.strip()


def serve(ws, env, label):
    """Builds until the daemon serves the build, then waits until it has nothing left to do
    unasked."""
    build(ws, env, f"{label}: the first build")
    for _ in range(240):
        if "[ra-jit]" in build(ws, env, f"{label}: the wait for the daemon"):
            break
        time.sleep(0.5)
    else:
        raise SystemExit(f"{label}: no build said `[ra-jit]`; the daemon's log is in {env['RA_JIT_HOME']}")
    for _ in range(40):
        status = subprocess.run(["cargo", "ra-jit", "status"], cwd=ws, env=env, capture_output=True,
                                text=True, timeout=120).stdout
        if "loading" not in status and "building" not in status:
            break
        time.sleep(0.25)
    # What the daemon would do unasked after its build, it starts at once.
    time.sleep(2)


def restart(ws, env, label, steps):
    """A daemon that starts on the store: the result is how many of its checks failed."""
    logs = Logs(env["RA_JIT_HOME"])
    serve(ws, env, label)
    found = [(int(f), int(a)) for f, a in
             re.findall(r"^store: (\d+) functions found, (\d+) compiled and added$", logs.text(), re.M)]
    wrong = 0
    took = sum(f for f, _ in found)
    roots = sum(int(n) for n in re.findall(r"^base from the store: (\d+) roots", logs.text(), re.M))
    # A start whose roots from the store call all it needs has nothing to look up.
    verdict = "ok" if roots and program(ws) == FIRST else f"FOUND {took}, {roots} ROOTS, PRINTED {program(ws)!r}"
    wrong += verdict != "ok"
    print(f"{label + ': the start':48} {verdict:12} store {found}, {roots} roots", flush=True)
    for what, edits, expected in steps:
        for file, old, new in edits:
            path = os.path.join(ws, file)
            text = open(path).read()
            assert old in text, (what, old)
            open(path, "w").write(text.replace(old, new, 1))
        seen, seen_anew, seen_unchanged = len(logs.links()), len(logs.anew()), len(logs.unchanged())
        said = build(ws, env, what)
        printed = program(ws)
        made = logs.links()[seen:]
        anew = logs.anew()[seen_anew:]
        unchanged = logs.unchanged()[seen_unchanged:]
        how = made[-1] if made else "no link"
        verdict = "ok"
        if printed != expected:
            verdict = f"PRINTED {printed!r}, NOT {expected!r}"
        elif "[ra-jit]" not in said:
            verdict = "NOT SERVED BY THE DAEMON"
        elif not how.startswith("spliced"):
            verdict = "NOT SPLICED"
        elif what in KEEPING and (not anew or anew[-1]):
            verdict = "NEW CODE"
        elif what in RENAMING and not (anew and anew[-1]):
            verdict = "NOTHING RENAMED"
        elif what in UNCHANGED and "RA_JIT_CHECK_ALL" not in env and not (unchanged and unchanged[-1]):
            verdict = "NONE BY ITEMS"
        wrong += verdict != "ok"
        print(f"{label + ': ' + what:48} {verdict:12} {how[:100]}; {anew[-1] if anew else '?'} anew, "
              f"{unchanged[-1] if unchanged else '?'} by items", flush=True)
    if REBUILT in logs.text():
        print(f"{label}: NOT the store's code taken as it is: the daemon said `{REBUILT}`", flush=True)
        wrong += 1
    if UNKEPT in logs.text():
        print(f"{label}: NOT every call kept: the daemon said `{UNKEPT}`", flush=True)
        wrong += 1
    if MISSED in logs.text():
        print(f"{label}: NOT the code taken by its items the code it was: the daemon said `{MISSED}`", flush=True)
        wrong += 1
    return wrong


def main():
    bin_dir = os.path.dirname(os.path.abspath(os.environ["RA_JIT_BIN"]))
    if not glob.glob(os.path.join(bin_dir, "cargo-ra-jit*")):
        raise SystemExit(f"no cargo-ra-jit beside {os.environ['RA_JIT_BIN']}")
    # The daemon's sockets are under its home, and a socket's path has to be short: macOS's
    # temporary directory is too long a start.
    root = os.path.realpath(tempfile.mkdtemp(prefix="ra-jit-tracked-", dir="/tmp"))
    ws, copy, store = os.path.join(root, "ws"), os.path.join(root, "copy"), os.path.join(root, "store")
    for path, text in FILES.items():
        os.makedirs(os.path.dirname(os.path.join(ws, path)), exist_ok=True)
        open(os.path.join(ws, path), "w").write(text)

    def env_of(home):
        env = dict(os.environ, PATH=f"{bin_dir}{os.pathsep}{os.environ['PATH']}", RA_JIT_HOME=home,
                   RA_JIT_CACHE=store, RA_JIT_RUSTC_BASE="0", RA_JIT_STATS="1")
        for name in ("CARGO_TARGET_DIR", "CARGO_INCREMENTAL", "RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS"):
            env.pop(name, None)
        return env

    here, there = env_of(os.path.join(root, "h")), env_of(os.path.join(root, "h2"))
    wrong = 0
    try:
        serve(ws, here, "filling the store")
        subprocess.run(["cargo", "ra-jit", "stop"], cwd=ws, env=here, capture_output=True, timeout=120)
        shutil.copytree(ws, copy, ignore=shutil.ignore_patterns("target"))
        wrong += restart(ws, here, "restart", STEPS)
        subprocess.run(["cargo", "ra-jit", "stop"], cwd=ws, env=here, capture_output=True, timeout=120)
        wrong += restart(copy, dict(there, RA_JIT_CHECK_ALL="1"), "another checkout", STEPS)
    finally:
        for at, env in ((ws, here), (copy, there)):
            if os.path.isdir(at):
                subprocess.run(["cargo", "ra-jit", "stop"], cwd=at, env=env, capture_output=True, timeout=120)
    print(f"{wrong} wrong; under {root}")
    if wrong:
        sys.exit(1)
    shutil.rmtree(root, ignore_errors=True)


if __name__ == "__main__":
    main()
