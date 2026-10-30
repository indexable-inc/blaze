#!/usr/bin/env python3
# pyright: basic
"""warm_store.py: whether a checkout at another path, with a home of its own, builds from the
store another checkout filled without compiling anything again.

The workspace in harness/keys (a build script, a dependency, generated code) is built in one
checkout with an empty store and a fresh BLAZE_HOME, then in a copy of it at another path, with
another fresh BLAZE_HOME and an empty target directory but the store the first build filled, as
a second worktree, a teammate or the gate's "warm store" run has it. The second build has to
compile no function and keep every build-script and proc-macro step (compile and run) from the
store, and both programs have to print the same.

usage: warm_store.py '{}'
       warm_store.py '{"keep": true}'

The input is one JSON object (inline, the path of a file, or `-` for standard input), an Input
below; a field it does not have is refused. `keep` leaves the checkouts, homes and store in
place to look at. Run it in the repository's Python workspace: `uv run --frozen python
harness/warm_store.py '{}'` from the root.
"""

import os
import shutil
import subprocess
import sys
import tempfile
from dataclasses import dataclass

from typed_input import fail, parse

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
from blaze_cli import Side, base_env, store_use, wait_warm, worker_logs

SCRIPT = "warm_store.py"


@dataclass(frozen=True)
class Input:
    keep: bool = False


def build(ws, home, store):
    """`blaze build` of the program in `ws` with its own `home` and the shared `store`: what the
    workers logged, and what the program prints."""
    env = dict(base_env(home), BLAZE_CACHE=store, BLAZE_STATS="1")
    side = Side(ws, env, True)
    try:
        _, ran = side.cargo("build", "-p", "kapp")
        if ran.returncode != 0:
            fail(SCRIPT, f"`blaze build` in {ws} failed:\n{ran.stderr[-3000:]}")
        _, states = wait_warm(side, ws)
        if states != ["ready"]:
            fail(SCRIPT, f"the daemon of {ws} did not get ready: {states}")
        logged = worker_logs(home)
    finally:
        side.cargo("stop")
    program = os.path.join(ws, "target", "debug", "kapp")
    printed = subprocess.run([program], cwd=ws, capture_output=True, stdin=subprocess.DEVNULL)
    return logged, (printed.returncode, printed.stdout)


def main():
    match sys.argv[1:]:
        case [text]:
            it = parse(Input, text, SCRIPT)
        case _:
            fail(SCRIPT, f"usage: {SCRIPT} JSON (see the top of the file)")
    work = os.path.realpath(tempfile.mkdtemp(prefix="blaze-warm-store-"))
    store = os.path.join(work, "store")
    # The second is deeper, and its name is not the first's: no key may name where either is.
    first = os.path.join(work, "a", "ws")
    second = os.path.join(work, "elsewhere", "b", "checkout")
    for ws in (first, second):
        shutil.copytree(os.path.join(HERE, "keys"), ws, ignore=shutil.ignore_patterns("target"))
    _, printed_first = build(first, os.path.join(work, "home-a"), store)
    logged, printed_second = build(second, os.path.join(work, "home-b"), store)
    use = store_use(logged)
    problems = []
    if printed_first != printed_second:
        problems.append(f"the programs differ: {printed_first!r} and {printed_second!r}")
    if use["functions_compiled"]:
        problems.append(f"{use['functions_compiled']} functions compiled again, {use['entries_missed']} entries not found")
    if not use["steps"]:
        problems.append("no build-script or proc-macro step was logged")
    elif use["steps_kept"] != use["steps"]:
        problems.append(f"{use['steps'] - use['steps_kept']} of {use['steps']} build-script and proc-macro steps made again")
    print(f"second checkout: {use}")
    for problem in problems:
        print(f"  {problem}")
    if it.keep:
        print(f"kept: {work}")
    else:
        shutil.rmtree(work, ignore_errors=True)
    sys.exit(1 if problems else 0)


if __name__ == "__main__":
    main()
