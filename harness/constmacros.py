#!/usr/bin/env python3
"""constmacros.py: a derive written as a function of the package, run by blaze's const evaluator.

    constmacros.py ['{"keep": true}']

The input, when given, is JSON: `keep` (bool, default false) keeps the copy of the fixture.

harness/constmacros is a package whose `[package.metadata.blaze] const-macros` file defines the
derives `Named`, given the struct as text, and `Fields`, given it as a flat tree read through
`const_ast`'s views (docs/future/const-macros.md). In a copy of it, with a daemon of its own:
  cold      `blaze run` from nothing: the derives expand and the program prints `Point 3 x,y`
  warm      `blaze run` again
  edit      the macro's output is edited: the program prints what the new one generates
  off       the root's `experimental = ["const-macros"]` taken out: the build fails, naming it
Prints each step's time and exits non-zero when one is wrong.
"""
import json, os, shutil, subprocess, sys, tempfile, time

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
from difftest import BLAZE


def main():
    options = json.loads(sys.argv[1]) if len(sys.argv) > 1 else {}
    unknown = set(options) - {"keep"}
    if unknown or not isinstance(options.get("keep", False), bool):
        sys.exit(f"constmacros.py: the input is {{\"keep\": bool}}, not {sys.argv[1]}")
    ws = tempfile.mkdtemp(prefix="blaze-constmacros-")
    shutil.copytree(os.path.join(HERE, "constmacros"), ws, dirs_exist_ok=True)
    home = os.path.join(ws, ".blaze-home")
    env = {**os.environ, "BLAZE_HOME": home, "BLAZE_CACHE": os.path.join(home, "store")}

    def blaze(*args):
        # The engine takes `blaze build` as `cargo build`; `stop` as itself.
        argv = [BLAZE, *args] if args[0] == "stop" else [BLAZE, "cargo", *args]
        start = time.monotonic()
        ran = subprocess.run(argv, cwd=ws, env=env, capture_output=True, text=True)
        return ran, time.monotonic() - start

    def edit(path, old, new):
        path = os.path.join(ws, path)
        with open(path) as f:
            text = f.read()
        assert old in text, f"{old!r} is not in {path}"
        with open(path, "w") as f:
            f.write(text.replace(old, new))

    wrong = []

    def step(name, args, check):
        ran, took = blaze(*args)
        why = check(ran)
        print(f"{name:6} {took:6.2f}s  {'ok' if why is None else 'WRONG: ' + why}")
        if why is not None:
            wrong.append(name)
            print("   ", (ran.stdout + ran.stderr)[-3000:].replace("\n", "\n    "))

    def prints(expected):
        def check(ran):
            if ran.returncode:
                return f"exit {ran.returncode}"
            if ran.stdout.strip() != expected:
                return f"printed {ran.stdout.strip()!r}, not {expected!r}"
            return None

        return check

    try:
        step("cold", ["run", "-q"], prints("Point 3 x,y"))
        step("warm", ["run", "-q"], prints("Point 3 x,y"))
        edit("src/macros.rs", 'NAME: &\'static str = \\"{name}\\"', 'NAME: &\'static str = \\"the {name}\\"')
        step("edit", ["run", "-q"], prints("the Point 3 x,y"))
        edit("Cargo.toml", 'experimental = ["const-macros"]', "experimental = []")

        def refused(ran):
            if ran.returncode == 0:
                return "built"
            if 'experimental = ["const-macros"]' not in ran.stderr:
                return "the error does not name the experiment"
            return None

        step("off", ["build", "-q"], refused)
    finally:
        blaze("stop")
        if not options.get("keep"):
            shutil.rmtree(ws, ignore_errors=True)
        else:
            print("kept", ws)
    print(f"{4 - len(wrong)} of 4 right")
    sys.exit(1 if wrong else 0)


if __name__ == "__main__":
    main()
