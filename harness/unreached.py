#!/usr/bin/env python3
"""unreached.py: an edit that calls what the program did not call when it was linked is
spliced all the same, or the program is built in full. Linux and macOS.

usage: unreached.py    (BLAZE_BIN names the blaze built; blaze is found next to it)

A patch calls the code of the executable by name, also code the executable's own did not
reach when it was linked: here `<u16 as Debug>::fmt` and the formatting it calls, which a
generic function of a library first calls for `u16` in an edit. blaze's link takes out what
nothing reaches (`-dead_strip` on macOS, `--gc-sections` on Linux), and a splice that does
not find what the patch calls declines: the daemon then lays the program out in full.

The program is built with `blaze build`, then edited. Every edit after the first one
(which writes the whole object again) has to be spliced or laid out in full, as the build's
`blaze-work` line says (`--message-format=json`, storelib.links), and the program has to print
what the source says.

Python 3, stdlib only.
"""
import glob
import os
import shutil
import subprocess
import sys
import tempfile

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
from storelib import linked  # noqa: E402

os.environ.setdefault("RUSTUP_TOOLCHAIN", open(os.path.join(HERE, "toolchain")).read().strip())
FILES = {
    "Cargo.toml": '[workspace]\nmembers = ["app", "show"]\nresolver = "2"\n',
    "show/Cargo.toml": '[package]\nname = "ushow"\nversion = "0.1.0"\nedition = "2021"\n',
    "show/src/lib.rs": """pub fn show<T: Default + std::fmt::Debug>() -> String {
    format!("{:?}", T::default())
}
""",
    "app/Cargo.toml": '[package]\nname = "uapp"\nversion = "0.1.0"\nedition = "2021"\n\n'
                      '[dependencies]\nushow = { path = "../show" }\n',
    "app/src/main.rs": """fn main() {
    println!("{} {}", "a", ushow::show::<u8>());
}
""",
}
# (what the step is, [(old, new)] in app/src/main.rs, what the program prints after it)
STEPS = [
    ("the program's own text", [('"a"', '"b"')], "b 0"),
    ("an instance whose callee nothing called", [('"{} {}"', '"{} {} {}"'), ("<u8>()", "<u8>(), ushow::show::<u16>()")], "b 0 0"),
    ("and one more", [('"{} {} {}"', '"{} {} {} {}"'), ("<u16>()", "<u16>(), ushow::show::<i128>()")], "b 0 0 0"),
]


def main():
    bin_dir = os.path.dirname(os.path.abspath(os.environ["BLAZE_BIN"]))
    if not glob.glob(os.path.join(bin_dir, "blaze*")):
        raise SystemExit(f"no blaze beside {os.environ['BLAZE_BIN']}")
    # The daemon's sockets are under its home, and a socket's path has to be short: macOS's
    # temporary directory is too long a start.
    root = tempfile.mkdtemp(prefix="blaze-unreached-", dir=None if os.name == "nt" else "/tmp")
    ws, home = os.path.join(root, "ws"), os.path.join(root, "h")
    for path, text in FILES.items():
        os.makedirs(os.path.dirname(os.path.join(ws, path)), exist_ok=True)
        open(os.path.join(ws, path), "w").write(text)
    env = dict(os.environ, PATH=f"{bin_dir}{os.pathsep}{os.environ['PATH']}", BLAZE_HOME=home)
    for name in ("CARGO_TARGET_DIR", "CARGO_INCREMENTAL", "RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS"):
        env.pop(name, None)
    program, source = os.path.join(ws, "target", "debug", "uapp"), os.path.join(ws, "app", "src", "main.rs")
    wrong = 0
    try:
        linked(ws, env, "first build", "uapp")
        for n, (what, edits, expected) in enumerate(STEPS):
            text = open(source).read()
            for old, new in edits:
                assert old in text, (what, old)
                text = text.replace(old, new, 1)
            open(source, "w").write(text)
            made = [str(it) for it in linked(ws, env, what, "uapp")]
            printed = subprocess.run([program], capture_output=True, text=True, timeout=60).stdout.strip()
            how = made[-1] if made else "no link"
            verdict = "ok"
            if printed != expected:
                verdict = f"PRINTED {printed!r}, NOT {expected!r}"
            elif n > 0 and not how.startswith(("spliced", "full")):
                verdict = "NOT SPLICED"
            wrong += verdict != "ok"
            print(f"{what:42} {verdict:12} {how[:140]}", flush=True)
    finally:
        subprocess.run(["blaze", "stop"], cwd=ws, env=env, capture_output=True, timeout=120)
    print(f"{len(STEPS) - wrong} of {len(STEPS)} right; under {root}")
    if wrong:
        sys.exit(1)
    shutil.rmtree(root, ignore_errors=True)


if __name__ == "__main__":
    main()
