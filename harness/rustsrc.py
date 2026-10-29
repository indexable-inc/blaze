#!/usr/bin/env python3
"""rustsrc.py: a build without the standard library's source is turned down, saying so.

blaze compiles `core`, `alloc` and `std` from its own `library/` (docs/toolchain.md), which
`BLAZE_LIBRARY` names to its processes; no toolchain's `rust-src` is read. Without it every
call into them is unresolved, and each function making one would be refused for a reason that
says nothing of the library. The project is loaded with `BLAZE_LIBRARY` naming a library with
an empty `core`: blaze has to fail before building anything and say how it gets the source.
"""
import os, subprocess, sys, tempfile

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
from difftest import BLAZE

TOML = """[package]
name = "nosrc"
version = "0.1.0"
edition = "2021"
"""
SAID = ("has no source of the standard library", "`blaze toolchain` downloads blaze's")


def main():
    with tempfile.TemporaryDirectory(prefix="blaze-rustsrc-") as work:
        project, library = os.path.join(work, "nosrc"), os.path.join(work, "library")
        os.makedirs(os.path.join(project, "src"))
        os.makedirs(os.path.join(library, "core"))
        open(os.path.join(project, "Cargo.toml"), "w").write(TOML)
        open(os.path.join(project, "src", "main.rs"), "w").write('fn main() {\n    println!("{}", 7);\n}\n')
        env = {**os.environ, "BLAZE_LIBRARY": library}
        run = subprocess.run([BLAZE, project, "--crate", "nosrc", "--out", os.path.join(work, "nosrc.o")],
                             capture_output=True, text=True, env=env, timeout=300)
        missing = [it for it in SAID if it not in run.stderr]
        if run.returncode == 0 or missing:
            print(f"blaze exited {run.returncode} without saying {missing}:\n{run.stderr[-2000:]}")
            sys.exit(1)
        if os.path.exists(os.path.join(work, "nosrc.o")):
            print("blaze wrote an object for a library it turned down")
            sys.exit(1)
        print("turned down: " + next(line for line in run.stderr.splitlines() if SAID[0] in line).strip())


if __name__ == "__main__":
    main()
