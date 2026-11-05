#!/usr/bin/env python3
"""testselectbench.py: what `BLAZE_TESTS=affected` saves on real projects. Measures, and fails
when a build was not the daemon's, which blaze is never to do (#84): every `blaze test` runs
with `--message-format=json`, and one that prints no `blaze-work` line was not served.

usage: testselectbench.py [--json FILE] [--keep DIR] [--args ARG,ARG...] [project ...]
       (BLAZE_BIN names the blaze built; the projects are bench.py's, ripgrep, helix and
       nushell by default, at their pinned revisions and on harness/toolchain, tested with
       bench.py's `test` arguments)

Each project is copied to a scratch directory, opted into sandboxed tests, and tested with
`blaze test`, each step a row: every test, the first time (which starts the daemon, and is
served by it as every build is); every test again (`BLAZE_TESTS=all`); the affected ones twice, the first recording what passed; the affected
ones after bench.py's `leaf` edit, after a second edit of the same function, and every test
after it. A row has the wall time, whether the daemon built it, how many tests ran and how
many were skipped, and, from what the daemon wrote beside each test program, how many tests
are run every time and the leading reasons. `--keep` writes what each step printed in DIR, and
under DIR/reach the files the daemon wrote beside each test program after the run with no edit.
"""
import collections
import glob
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile

import bench
from blaze_cli import BLAZE, TOOLCHAIN, fetch, timed
from storelib import worked

TIMEOUT = 7200
KEEP = None
RAN = re.compile(r"^test (.+?) \.\.\. ", re.M)
# v1's line for people; v2 prints no count of the tests it skipped in any machine format.
SKIPPED = re.compile(r"Skipped (\d+) tests?,")


STD = ("std::", "<std::", "core::", "<core::", "alloc::", "<alloc::")


def reach(dirs):
    """How many tests the daemon's `tests-reach` files list, how many of them are run every
    time, and why: by the call each names, and by the first function of the standard library
    on the calls that take it there, with what it reaches. And how many reach the world outside
    their code but are known, to run sandboxed where there is a sandbox and every time where not."""
    listed, sandboxed, always, by = 0, 0, collections.Counter(), collections.Counter()
    paths = {path for dir in dirs for path in glob.glob(os.path.join(dir, "**", "tests-reach"), recursive=True)}
    for path in sorted(paths):
        for line in open(path).read().splitlines():
            parts = line.split("\t")
            listed += 1
            sandboxed += parts[0].startswith("~")
            if parts[0] in ("-", "*"):
                why = parts[2] if parts[0] == "-" and len(parts) > 2 else parts[-1]
                always[re.sub(r"::<.*>", "::<…>", why)] += 1
                path = parts[3].split(" -> ") if parts[0] == "-" and len(parts) > 3 else []
                first = next((it for it in path if it.startswith(STD)), "-")
                by[(first, path[-1] if path else "-")] += 1
    return listed, sandboxed, always, by


def row(ws, env, args, mode, what):
    took, ran = timed(["blaze", "test", "--message-format=json", *args], ws, dict(env, BLAZE_TESTS=mode), TIMEOUT)
    said = ran.stdout + ran.stderr
    if KEEP:
        os.makedirs(KEEP, exist_ok=True)
        name = re.sub(r"\W+", "-", f"{os.path.basename(ws)} {what}")
        open(os.path.join(KEEP, f"{name}.log"), "w").write(said)
    skipped = sum(int(n) for n in SKIPPED.findall(said))
    listed, sandboxed, always, by = reach([os.path.join(ws, "target"), env["BLAZE_HOME"]])
    # Served: the daemon's `blaze-work` line is there. There is no cargo to fall back to, so
    # there is no reason a build went elsewhere to read.
    return dict(step=what, mode=mode, exit=ran.returncode, seconds=round(took, 1), served=bool(worked(ran.stdout)),
                ran=len(RAN.findall(ran.stdout)), skipped=skipped, listed=listed, always=sum(always.values()), sandboxed=sandboxed,
                why=always.most_common(8), by=[(f"{first} … {leaf}", n) for (first, leaf), n in by.most_common(12)])


def edit(ws, changes):
    for file, old, new in changes:
        path = os.path.join(ws, file)
        text = open(path).read()
        assert old in text, (file, old)
        open(path, "w").write(text.replace(old, new, 1))


def show(name, it):
    why = "; ".join(f"{n} {w}" for w, n in it["why"][:3])
    print(f"{name:9} {it['step']:24} {it['seconds']:7.1f}s exit {it['exit']} {'daemon' if it['served'] else 'NONE  ':6} "
          f"ran {it['ran']:5} skipped {it['skipped']:5} always {it['always']:5}/{it['listed']:<5} "
          f"sandboxed {it.get('sandboxed', 0):5} {why}", flush=True)
    if it["step"] == "affected, no edit" and it["by"] and it["by"][0][0] != "- … -":
        print("    entered the standard library at:", "; ".join(f"{n} {w}" for w, n in it["by"]), flush=True)


class Rows(list):
    """The rows of a project, each shown as it is added."""

    def __init__(self, name):
        super().__init__()
        self.name = name

    def append(self, it):
        show(self.name, it)
        super().append(it)


def project(name, args):
    spec = bench.PROJECTS[name]
    source = fetch(name, spec)
    root = tempfile.mkdtemp(prefix=f"blaze-tsb-{name}-")
    home = tempfile.mkdtemp(prefix="rj-tsb-", dir="/tmp")
    ws = os.path.join(root, name)
    shutil.copytree(source, ws, symlinks=True, ignore=shutil.ignore_patterns("target"))
    # A pass is kept only for a sandboxed test, an experiment the workspace opts into.
    with open(os.path.join(ws, "Cargo.toml"), "a") as manifest:
        manifest.write('\n[workspace.metadata.blaze]\nexperimental = ["sandboxed-tests"]\n')
    bin_dir = os.path.dirname(os.path.abspath(BLAZE))
    env = dict(os.environ, PATH=f"{bin_dir}{os.pathsep}{os.environ['PATH']}", BLAZE_HOME=home,
               RUST_BACKTRACE="0", RUSTUP_TOOLCHAIN=TOOLCHAIN, **spec.get("env", {}))
    for var in ("CARGO_TARGET_DIR", "CARGO_INCREMENTAL", "RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS", "BLAZE_TESTS"):
        env.pop(var, None)
    args = args if args is not None else spec.get("test", [])
    rows = Rows(name)
    try:
        rows.append(row(ws, env, args, "all", "every test, first"))
        rows.append(row(ws, env, args, "all", "every test"))
        rows.append(row(ws, env, args, "affected", "affected, first"))
        rows.append(row(ws, env, args, "affected", "affected, no edit"))
        if KEEP:
            # What the daemon wrote beside each program, before a run of every test removes it.
            for top in (os.path.join(ws, "target"), home):
                for path in glob.glob(os.path.join(top, "**", "tests-reach*"), recursive=True):
                    kept = os.path.join(KEEP, "reach", os.path.basename(top), os.path.relpath(path, top))
                    os.makedirs(os.path.dirname(kept), exist_ok=True)
                    shutil.copy(path, kept)
        leaf = spec["edits"]["leaf"]
        edit(ws, leaf)
        rows.append(row(ws, env, args, "affected", "affected, leaf edit"))
        edit(ws, [(file, new, new + bench.LET.replace("_bench_edit", "_bench_edit2")) for file, _, new in leaf])
        rows.append(row(ws, env, args, "affected", "affected, second edit"))
        rows.append(row(ws, env, args, "all", "every test, after it"))
    finally:
        subprocess.run(["blaze", "stop"], cwd=ws, env=env, capture_output=True, timeout=300)
        shutil.rmtree(root, ignore_errors=True)
        shutil.rmtree(home, ignore_errors=True)
    return rows


def main():
    argv, out, args = sys.argv[1:], None, None
    if "--json" in argv:
        at = argv.index("--json")
        out, argv = argv[at + 1], argv[:at] + argv[at + 2:]
    if "--keep" in argv:
        global KEEP
        at = argv.index("--keep")
        KEEP, argv = argv[at + 1], argv[:at] + argv[at + 2:]
    if "--args" in argv:
        at = argv.index("--args")
        args, argv = re.split(r"[ ,]+", argv[at + 1].strip()), argv[:at] + argv[at + 2:]
    results = {}
    for name in argv or ["ripgrep", "helix", "nushell"]:
        results[name] = project(name, args)
    if out:
        json.dump(results, open(out, "w"), indent=1)
    unserved = [(name, it) for name, rows in results.items() for it in rows if not it["served"]]
    for name, it in unserved:
        print(f"NOT SERVED BY THE DAEMON: {name} {it['step']}: no `blaze-work` line (exit {it['exit']})")
    sys.exit(1 if unserved else 0)


if __name__ == "__main__":
    main()
