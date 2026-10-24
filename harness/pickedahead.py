#!/usr/bin/env python3
"""pickedahead.py: a first build that picks its target (`--example`) is loaded by the daemon
while cargo builds it, as one that builds the whole package is.

usage: pickedahead.py    (RA_JIT_BIN names the ra-jit built; cargo-ra-jit is found next to it)

Rustc builds every crate of a build that picks its targets, and the daemon was told of one only
once cargo was done: bevy's `--example breakout` then waited 25 s more for the workspace to
load. Here a package has a library and an example, and a build script that keeps cargo busy.
With a spare up, `cargo ra-jit build --example demo` has to have a worker load the workspace
beside cargo's build, the next build has to be the daemon's, and so an edit of the example,
whose program has to print what cargo's does.

Python 3, stdlib only.
"""
import glob, os, re, shutil, subprocess, sys, tempfile, time

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import cargo_ra_jit
from cargo_ra_jit import Side, wait_warm
from spare import FIRST_MAIN, FIRST_TOML, settled

# How long the build script keeps cargo busy.
WAIT_SECS = 5
FILES = {
    "Cargo.toml": '[package]\nname = "app"\nversion = "0.1.0"\nedition = "2021"\nbuild = "build.rs"\n\n[workspace]\n',
    "build.rs": f'fn main() {{\n    println!("cargo:rerun-if-changed=build.rs");\n'
                f'    std::thread::sleep(std::time::Duration::from_secs({WAIT_SECS}));\n}}\n',
    "src/lib.rs": 'pub fn value() -> u32 {\n    (1..10u32).sum()\n}\n',
    "examples/demo.rs": 'fn main() {\n    println!("{}", app::value() + 5);\n}\n',
}


def tails(home, lines=40):
    """The last lines of each of the daemon's logs, for a failure on a machine whose files are
    gone once it ends."""
    logs = sorted(glob.glob(os.path.join(home, "*.log")) + glob.glob(os.path.join(home, "*", "*", "log")))
    return "".join(f"\n--- {log}\n" + "".join(open(log, errors="replace").readlines()[-lines:]) for log in logs)


def told_ahead(home):
    """Whether a worker was told of the build before cargo was done with it."""
    logs = glob.glob(os.path.join(home, "*", "*", "log"))
    return any(re.search(r"^spare: loading beside cargo's build$", open(log, errors="replace").read(), re.M)
               for log in logs)


def build(side, step, home):
    _, ran = side.cargo("build", "--example", "demo")
    if ran.returncode != 0:
        raise SystemExit(f"{step}: the build failed:\n{(ran.stdout + ran.stderr)[-3000:]}{tails(home)}")
    return ran.stdout + ran.stderr


def main():
    bin_dir = os.path.dirname(os.path.abspath(os.environ["RA_JIT_BIN"]))
    if not os.path.exists(os.path.join(bin_dir, "cargo-ra-jit")):
        raise SystemExit(f"no cargo-ra-jit beside {os.environ['RA_JIT_BIN']}")
    # Short, for the daemon's sockets under its home.
    root = os.path.realpath(tempfile.mkdtemp(prefix="ra-jit-pa-", dir="/tmp"))
    ws, first_ws, home = (os.path.join(root, n) for n in ("ws", "first", "home"))
    for path, text in FILES.items():
        os.makedirs(os.path.dirname(os.path.join(ws, path)), exist_ok=True)
        open(os.path.join(ws, path), "w").write(text)
    os.makedirs(os.path.join(first_ws, "src"))
    open(os.path.join(first_ws, "Cargo.toml"), "w").write(FIRST_TOML)
    open(os.path.join(first_ws, "src/main.rs"), "w").write(FIRST_MAIN)
    env = cargo_ra_jit.base_env(home)
    env["PATH"] = bin_dir + os.pathsep + env["PATH"]
    for name in ("RUSTC_WRAPPER", "CARGO_INCREMENTAL", "RA_JIT_SPARE"):
        env.pop(name, None)
    first, side = Side(first_ws, env, True), Side(ws, env, True)
    reference = Side(ws, env, False)
    program = os.path.join(ws, "target", "debug", "examples", "demo")
    wrong = []
    try:
        # A daemon with a spare already up is told of the next build ahead of cargo.
        _, ran = first.cargo("build")
        if ran.returncode != 0:
            raise SystemExit(f"first: the build failed:\n{(ran.stdout + ran.stderr)[-3000:]}{tails(home)}")
        wait_warm(first, first_ws)
        settled(home)
        build(side, "cold", home)
        if not told_ahead(home):
            wrong.append("no worker loaded the workspace while cargo built the example")
        _, states = wait_warm(side, ws)
        if states != ["ready"]:
            raise SystemExit(f"the daemon did not get ready: {states}{tails(home)}")
        for _ in range(240):
            if "[ra-jit]" in build(side, "wait for the daemon", home):
                break
            time.sleep(0.5)
        else:
            raise SystemExit(f"no build said `[ra-jit]`{tails(home)}")
        example = os.path.join(ws, "examples/demo.rs")
        text = open(example).read()
        open(example, "w").write(text.replace("+ 5", "+ 50"))
        said = build(side, "an edit", home)
        ran = subprocess.run([program], capture_output=True, text=True, timeout=60)
        _, theirs = reference.cargo("run", "-q", "--example", "demo")
        if "[ra-jit]" not in said:
            wrong.append("the edit was not served by the daemon")
        if ran.stdout != theirs.stdout:
            wrong.append(f"the program printed {ran.stdout.strip()!r}, cargo's {theirs.stdout.strip()!r}")
        print(f"{'--example loaded ahead':30} {'ok' if not wrong else 'WRONG':8} printed {ran.stdout.strip()!r}")
    finally:
        first.cargo("stop")
    for why in wrong:
        print(f"  {why}")
    if wrong:
        print(f"under {root}{tails(home)}")
        sys.exit(1)
    shutil.rmtree(root, ignore_errors=True)


if __name__ == "__main__":
    main()
