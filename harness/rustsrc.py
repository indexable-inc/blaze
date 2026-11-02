#!/usr/bin/env python3
"""rustsrc.py: a build without the standard library's source is turned down, saying so.

blaze compiles `core`, `alloc` and `std` from its own `library/` (docs/toolchain.md), which
`BLAZE_LIBRARY` names to its processes; no toolchain's `rust-src` is read. Without it every
call into them is unresolved, and each function making one would be refused for a reason that
says nothing of the library. The project is built by `blaze build` in an empty home with
`BLAZE_LIBRARY` naming a library with an empty `core`: blaze has to fail before building
anything and say what the library it names lacks (crates/blaze-cargo/src/toolchain/library.rs).
"""
import os, sys

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
from rebuild import Project, scratch
from storelib import Failed

TOML = """[package]
name = "nosrc"
version = "0.1.0"
edition = "2021"
"""
SAID = ("BLAZE_LIBRARY names", "which is not a standard library blaze can read")


def main():
    work = scratch("blaze-rustsrc-")
    project_dir, library = os.path.join(work, "nosrc"), os.path.join(work, "library")
    os.makedirs(os.path.join(project_dir, "src"))
    os.makedirs(os.path.join(library, "core"))
    open(os.path.join(project_dir, "Cargo.toml"), "w").write(TOML)
    open(os.path.join(project_dir, "src", "main.rs"), "w").write('fn main() {\n    println!("{}", 7);\n}\n')
    project = Project(project_dir, work, env={"BLAZE_LIBRARY": library})
    # The daemon may turn the library down as it starts, or the build as it loads.
    try:
        built = project.build("nosrc")
        status, said, executable = built.status, built.said, built.executable
    except Failed as error:
        status, said, executable = "no daemon", str(error), None
    finally:
        project.stop()
    missing = [it for it in SAID if it not in said]
    if status == 0 or missing:
        print(f"blaze exited {status} without saying {missing}:\n{said[-2000:]}")
        sys.exit(1)
    if executable or os.path.exists(os.path.join(project_dir, "target", "debug", "nosrc")):
        print("blaze built a program with a library it turned down")
        sys.exit(1)
    print("turned down: " + next(line for line in said.splitlines() if SAID[0] in line).strip())


if __name__ == "__main__":
    main()
