#!/usr/bin/env python3
"""spaces.py: a workspace under a directory whose name has a space in it, built end to end by
`blaze build` (issue #14).

usage: spaces.py    (BLAZE_BIN names the blaze built; blaze is found next to
                     it)

The directory is also named with `$$`, `#` and a quote, which rustc's dep-info files write as
they are, and the daemon's home is in it too. The workspace has a library and a program that
calls it, and a module in a file whose name has a space. It is built until the daemon serves
the build, then each crate is edited in turn. After every build the program has to print
what the source says, and the daemon's objects have to name the functions of both crates by
rustc's symbols: a crate the daemon does not find among what rustc built gets names of
blaze's own (`blaze.<id>.<path>`), which nothing rustc built links to.

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
FILES = {
    "Cargo.toml": '[workspace]\nmembers = ["app", "mid"]\nresolver = "2"\n',
    "mid/Cargo.toml": '[package]\nname = "smid"\nversion = "0.1.0"\nedition = "2021"\n',
    "mid/src/lib.rs": """pub fn area(side: u32) -> u32 {
    side * side
}

pub struct Shelf {
    pub items: Vec<u32>,
}

impl Shelf {
    pub fn total(&self) -> u32 {
        self.items.iter().sum()
    }
}
""",
    "app/Cargo.toml": '[package]\nname = "sapp"\nversion = "0.1.0"\nedition = "2021"\n\n'
                      '[dependencies]\nsmid = { path = "../mid" }\n',
    "app/src/main.rs": """#[path = "with space.rs"]
mod spaced;

fn answer() -> u32 {
    1
}

fn main() {
    let shelf = smid::Shelf { items: vec![1, 2, 3] };
    println!("{} {} {} {}", answer(), smid::area(3), shelf.total(), spaced::word());
}
""",
    "app/src/with space.rs": 'pub fn word() -> &\'static str {\n    "one"\n}\n',
}
# (what the step is, [(file, old, new)], what the program prints after it)
STEPS = [
    ("nothing edited", [], "1 9 6 one"),
    ("the program's crate", [("app/src/main.rs", "    1\n}", "    2\n}")], "2 9 6 one"),
    ("the library", [("mid/src/lib.rs", "side * side", "side * side + 1")], "2 10 6 one"),
    ("a method of the library", [("mid/src/lib.rs", ".sum()", ".sum::<u32>() * 2")], "2 10 12 one"),
    ("the file with a space in its name", [("app/src/with space.rs", '"one"', '"two"')], "2 10 12 two"),
]
OWN_NAME = re.compile(r"^_?blaze\.[0-9a-f]+\.")


def build(ws, env, step):
    ran = subprocess.run(["blaze", "build"], cwd=ws, env=env, capture_output=True, text=True,
                         timeout=TIMEOUT)
    said = ran.stdout + ran.stderr
    if ran.returncode != 0:
        raise SystemExit(f"{step}: the build failed:\n{said[-3000:]}")
    return said


def own_names(ws):
    """The functions the daemon's objects define under names of blaze's own."""
    found = []
    for obj in glob.glob(os.path.join(ws, "target", "blaze", "*", "programs", "*", "*.o")):
        listed = subprocess.run(["nm", "--defined-only", obj], capture_output=True, text=True).stdout
        names = [line.split()[-1] for line in listed.splitlines() if line.split()]
        found += [name for name in names if OWN_NAME.match(name) and re.search(r"s(app|mid)", name)]
    return sorted(set(found))


def main():
    bin_dir = os.path.dirname(os.path.abspath(os.environ["BLAZE_BIN"]))
    if not os.path.exists(os.path.join(bin_dir, "blaze")):
        raise SystemExit(f"no blaze beside {os.environ['BLAZE_BIN']}")
    root = tempfile.mkdtemp(prefix="blaze sp $$ #'q' ", dir="/tmp")
    ws, home = os.path.join(root, "work space"), os.path.join(root, "home")
    for path, text in FILES.items():
        os.makedirs(os.path.dirname(os.path.join(ws, path)), exist_ok=True)
        open(os.path.join(ws, path), "w").write(text)
    env = dict(os.environ, PATH=f"{bin_dir}{os.pathsep}{os.environ['PATH']}", BLAZE_HOME=home)
    for name in ("CARGO_TARGET_DIR", "CARGO_INCREMENTAL", "RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS"):
        env.pop(name, None)
    program = os.path.join(ws, "target", "debug", "sapp")
    wrong = 0
    try:
        build(ws, env, "first build")
        for _ in range(240):
            if "[blaze]" in build(ws, env, "wait for the daemon"):
                break
            time.sleep(0.5)
        else:
            raise SystemExit(f"no build said `[blaze]`; the daemon's log is in {home}")
        for what, edits, expected in STEPS:
            for path, old, new in edits:
                text = open(os.path.join(ws, path)).read()
                assert old in text, (what, old)
                open(os.path.join(ws, path), "w").write(text.replace(old, new, 1))
            said = build(ws, env, what)
            printed = subprocess.run([program], capture_output=True, text=True, timeout=60).stdout.strip()
            own = own_names(ws)
            served = "[blaze]" in said
            verdict = "ok"
            if printed != expected:
                verdict = f"PRINTED {printed!r}, NOT {expected!r}"
            elif not served:
                verdict = "NOT SERVED BY THE DAEMON"
            elif own:
                verdict = f"NAMED BY BLAZE: {', '.join(own[:3])}"
            wrong += verdict != "ok"
            print(f"{what:40} {verdict}", flush=True)
    finally:
        subprocess.run(["blaze", "stop"], cwd=ws, env=env, capture_output=True, timeout=120)
    print(f"{len(STEPS) - wrong} of {len(STEPS)} right; under {root}")
    if wrong:
        sys.exit(1)
    shutil.rmtree(root, ignore_errors=True)


if __name__ == "__main__":
    main()
