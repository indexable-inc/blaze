#!/usr/bin/env python3
"""loadedits.py: a file written while a worker loads the workspace is built as written.

usage: loadedits.py [--rounds N] [--keep]   (BLAZE_BIN names the blaze built; blaze
                                             is found next to it)

A worker reads the workspace's files when it loads, and from then on builds a file again
when its timestamp is no longer the one it took. A write after the read and before the
timestamp was taken left the worker with the old text and the new timestamp, and every
build after that answered with the program as it was before the write (#53: fd's tests ran
against the program from before the edit).

The daemon runs with `BLAZE_LOAD_PAUSE_MS`, which holds each worker between the two and has
it say "load: paused". Each round builds with a feature no round before used, which has the
daemon start a worker for it, writes `app/src/main.rs` while that worker is held, waits
until it is ready, and builds again: the daemon has to serve that build, and the program
has to print what was written. That build can be rustc's link, so the round then edits
`lib/src/lib.rs`, which the daemon patches into its own build of the program: that has to
print what `app/src/main.rs` says too.

Python 3, stdlib only.
"""
import argparse
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
PAUSE_MS = 2000
MAIN = 'fn main() {{\n    println!("{} {{}}", llib::times(2));\n}}\n'
LIB = "pub fn times(it: u32) -> u32 {{\n    it * {}\n}}\n"


def files(rounds):
    features = "".join(f"f{n} = []\n" for n in range(rounds))
    return {
        "Cargo.toml": '[workspace]\nmembers = ["app", "lib"]\nresolver = "2"\n',
        "lib/Cargo.toml": '[package]\nname = "llib"\nversion = "0.1.0"\nedition = "2021"\n',
        "lib/src/lib.rs": LIB.format(2),
        "app/Cargo.toml": '[package]\nname = "lapp"\nversion = "0.1.0"\nedition = "2021"\n\n'
                          f'[features]\n{features}\n[dependencies]\nllib = {{ path = "../lib" }}\n',
        "app/src/main.rs": MAIN.format("start"),
    }


def run(cmd, ws, env, step):
    ran = subprocess.run(cmd, cwd=ws, env=env, capture_output=True, text=True, timeout=TIMEOUT)
    said = ran.stdout + ran.stderr
    if ran.returncode != 0:
        raise SystemExit(f"{step}: {' '.join(cmd)} failed:\n{said[-3000:]}")
    return said


def states(ws, env):
    out = run(["blaze", "status"], ws, env, "status")
    return re.findall(rf"^    {re.escape(ws)}: (.*)$", out, re.M)


def paused(home):
    """How many workers said they were held after their load. A worker that was a spare
    keeps the spare's log."""
    logs = glob.glob(os.path.join(home, "*", "*", "log"))
    return sum(open(log, errors="replace").read().count("load: paused\n") for log in logs)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--rounds", type=int, default=2)
    parser.add_argument("--keep", action="store_true", help="keep the workspace and the daemon's logs")
    args = parser.parse_args()
    bin_dir = os.path.dirname(os.path.abspath(os.environ["BLAZE_BIN"]))
    if not glob.glob(os.path.join(bin_dir, "blaze*")):
        raise SystemExit(f"no blaze beside {os.environ['BLAZE_BIN']}")
    # The daemon's sockets are under its home, and a socket's path has to be short: macOS's
    # temporary directory is too long a start. Status names the workspace by its real path.
    root = os.path.realpath(tempfile.mkdtemp(prefix="blaze-loadedits-", dir=None if os.name == "nt" else "/tmp"))
    ws, home = os.path.join(root, "ws"), os.path.join(root, "h")
    for path, text in files(args.rounds).items():
        os.makedirs(os.path.dirname(os.path.join(ws, path)), exist_ok=True)
        open(os.path.join(ws, path), "w").write(text)
    env = dict(os.environ, PATH=f"{bin_dir}{os.pathsep}{os.environ['PATH']}", BLAZE_HOME=home,
               BLAZE_LOAD_PAUSE_MS=str(PAUSE_MS))
    for name in ("CARGO_TARGET_DIR", "CARGO_INCREMENTAL", "RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS"):
        env.pop(name, None)
    program, source = os.path.join(ws, "target", "debug", "lapp"), os.path.join(ws, "app", "src", "main.rs")
    library = os.path.join(ws, "lib", "src", "lib.rs")
    wrong = 0
    try:
        for n in range(args.rounds):
            build = ["blaze", "build", "--features", f"f{n}"]
            held = paused(home)
            run(build, ws, env, f"round {n}: the first build")
            began = time.monotonic()
            while paused(home) == held:
                if time.monotonic() - began > 300:
                    raise SystemExit(f"round {n}: no worker said `load: paused`; the logs are in {home}")
                time.sleep(0.02)
            want = f"round {n}"
            open(source, "w").write(MAIN.format(want))
            while not states(ws, env) or any(s.startswith("loading") for s in states(ws, env)):
                if time.monotonic() - began > 300:
                    raise SystemExit(f"round {n}: no worker is ready; the logs are in {home}")
                time.sleep(0.05)
            verdict = "ok"
            for step, factor in (("the write", 2), ("an edit of the library", 3 + n)):
                if factor != 2:
                    open(library, "w").write(LIB.format(factor))
                said = run(build, ws, env, f"round {n}: the build after {step}")
                printed = subprocess.run([program], capture_output=True, text=True, timeout=60).stdout.strip()
                expected = f"{want} {2 * factor}"
                if printed != expected:
                    verdict = f"after {step}: PRINTED {printed!r}, NOT {expected!r}"
                elif "[blaze]" not in said:
                    verdict = f"after {step}: NOT SERVED BY THE DAEMON"
                if verdict != "ok":
                    break
            open(library, "w").write(LIB.format(2))
            wrong += verdict != "ok"
            print(f"round {n}: {verdict}", flush=True)
    finally:
        subprocess.run(["blaze", "stop"], cwd=ws, env=env, capture_output=True, timeout=120)
    print(f"{args.rounds - wrong} of {args.rounds} right; under {root}")
    if wrong:
        sys.exit(1)
    if not args.keep:
        shutil.rmtree(root, ignore_errors=True)


if __name__ == "__main__":
    main()
