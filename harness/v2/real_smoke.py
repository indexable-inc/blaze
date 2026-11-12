#!/usr/bin/env python3
"""real_smoke.py: a small real workspace built by an installed blaze the way a user starts it.

Why it exists (2026-11-12, round 1 of int-1112): twice a tree that built and passed its small
tests failed a real project's cold build at its build scripts. The second time ten build scripts
of reth were refused because the embedded sandbox interposer had a RUNPATH naming blaze-cargo's
own build directory, which no hello-world sees (it has no build script). So a round's tree is
offered to nobody as "the tree" until this passes on it, and it runs before any test table
(agents/howto: real-smoke).

The workspace is `harness/v2/smoke`: crossbeam-utils and crossbeam-deque (build scripts blaze
proves pure), serde with derive and serde_json (a proc macro and a build script), getrandom,
parking_lot, and a local `-sys` crate whose build script compiles C with `cc` (no bindgen). The
program prints one line that holds the value `answer()` returns, so each step's proof is the
line of that step's sources.

    python3 -I harness/v2/real_smoke.py '{"blaze": "/root/wt/round/inst-e5f0d2abe", "work": "/root/wt/round/smoke", "tmpfs": "/dev/shm/round-smoke"}'

The input is one JSON object (`Input`), unknown fields refused:

    blaze   the directory of an installed blaze (`blaze` and `blaze-engine` copied out of the
            build tree: a blaze run from `target/` or under `cargo run` inherits cargo's
            library path and is not what a user has)
    work    a directory on a disk (refused when it is on a tmpfs, where /proc/mounts can say):
            the workspace's copy and the home are made new under it
    tmpfs   a directory on a tmpfs for the second run (outputs are placed otherwise there:
            a tmpfs has no clone); "" leaves that run out and says so
    steps   which of `cold`, `noop`, `edit`, in that order (all by default); `edit` needs `cold`
    path    the PATH of the fixed environment: where `cc` is

Each step is `blaze run` under `env -i`-like conditions (ran.Command: the whole environment is
HOME, PATH, TERM and LANG, nothing inherited), held to exit 0 and to the program's line
(ran.py: no number without both). `cold` builds from nothing in a home of its own; `noop` runs
again with nothing changed; `edit` changes the body of `answer` and must print the new value.
Beside each time it prints what blaze said it built. Exits 1 when any row is INVALID.

The workspace's Cargo.lock and blaze.lock are committed (the versions a run on 2026-11-12
resolved), so two rounds build the same crates. Without them the cold build writes both into
the checkout and `noop` then makes one unit (`cause="what its narrow key holds of the crate
smoke, Cargo.lock and blaze.lock changed"`, 5.9 s on int-1112 e5f0d2abe4): lane
firstedit-1112's lock-file feedback, open; its fix belongs in
`crates/blaze-cargo/src/serve/job.rs`, `ready` (the view must already hold the lock the build
writes). To see that cause, delete the two lock files from the copy.
"""
import json
import os
import re
import shutil
import subprocess
import sys
from dataclasses import dataclass
from pathlib import Path

# Run with `python3 -I`, which leaves the script's directory off the path: ran.py is beside it.
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import ran
from ran import Command, Proof

STEPS = ("cold", "noop", "edit")
SOURCE = Path(os.path.dirname(os.path.abspath(__file__))) / "smoke"


@dataclass(frozen=True)
class Input:
    blaze: str
    work: str
    tmpfs: str
    steps: tuple[str, ...] = STEPS
    path: str = "/run/current-system/sw/bin:/usr/local/bin:/usr/bin:/bin"


def filesystem(path: Path) -> str:
    """The type of the file system `path` is on by /proc/mounts (the longest mount point that
    holds it), or "" where there is no /proc/mounts to ask (macOS)."""
    try:
        mounts = Path("/proc/mounts").read_text().splitlines()
    except OSError:
        return ""
    at = os.path.realpath(path)
    best = ("", "")
    for line in mounts:
        parts = line.split()
        if len(parts) < 3:
            continue
        point = parts[1].replace("\\040", " ")
        if (at == point or at.startswith(point.rstrip("/") + "/")) and len(point) > len(best[0]):
            best = (point, parts[2])
    return best[1]


def line(x: int) -> str:
    """What the program prints when `answer()` returns `x` (harness/v2/smoke/src/main.rs)."""
    return f'smoke {{"x":{x},"y":2}} CachePadded {{ value: Point {{ x: {x}, y: 2 }} }} 1'


def said(r: ran.Ran) -> str:
    """What blaze said it built and why, from its status line."""
    status = [it for it in (r.stdout + "\n" + r.stderr).splitlines() if it.startswith("blaze: ")]
    if not status:
        return "no status line"
    kept = re.findall(r'\b(?:built|cached|errors|unbuilt)=\d+|\b(?:cause|why|recompiled)=(?:"[^"]*"|\S+)', status[-1])
    return " ".join(kept) or status[-1][:160]


def place(name: str, root: Path, asked: Input) -> bool:
    """The steps in a new copy and a new home under `root`; whether every row counted."""
    blaze = str(Path(asked.blaze) / "blaze")
    shutil.rmtree(root, ignore_errors=True)
    home = root / "home"
    home.mkdir(parents=True)
    project = root / "p"
    shutil.copytree(SOURCE, project)
    env = {"HOME": str(home), "PATH": asked.path, "TERM": "dumb", "LANG": "C.UTF-8"}
    main = project / "src/main.rs"
    good = True
    for step in asked.steps:
        x = 1
        if step == "edit":
            text = main.read_text()
            if text.count("\n    1\n") != 1:
                print(f"{name} edit: INVALID: src/main.rs has no one body `1` to edit", flush=True)
                good = False
                continue
            main.write_text(text.replace("\n    1\n", "\n    7\n"))
            x = 7
        r = ran.run(Command((blaze, "run"), str(project), env, timeout=1800), Proof(stdout=line(x)))
        good = good and not r.invalid
        text = f"INVALID: {r.invalid}" if r.invalid else f"{r.ms:8.0f} ms  exit and the program's line checked"
        print(f"{name + ' ' + step:16s} {text}  [{said(r)}]", flush=True)
    stop = subprocess.run([blaze, "stop"], cwd=project, env=env, capture_output=True, text=True, timeout=300, check=False)
    if stop.returncode != 0:
        print(f"{name} stop: INVALID: `blaze stop` exited {stop.returncode}: {ran.tail(stop.stdout + stop.stderr)}", flush=True)
        good = False
    return good


def main(argv: list[str]) -> None:
    if len(argv) != 2:
        sys.exit("real_smoke.py takes one JSON value (see its module docs)")
    asked = ran.parsed(Input, json.loads(argv[1]))
    unknown = [it for it in asked.steps if it not in STEPS]
    if unknown or not asked.steps or list(asked.steps) != [it for it in STEPS if it in asked.steps]:
        sys.exit(f"steps is some of {STEPS} in that order, not {asked.steps}")
    if "edit" in asked.steps and "cold" not in asked.steps:
        sys.exit("the `edit` step needs `cold` before it: it edits the sources the cold build built")
    for name in ("blaze", "blaze-engine"):
        if not (Path(asked.blaze) / name).is_file():
            sys.exit(f"{asked.blaze} holds no `{name}`: give the directory of an installed blaze")
    disk = filesystem(Path(asked.work).parent if not Path(asked.work).exists() else Path(asked.work))
    if disk == "tmpfs":
        sys.exit(f"work {asked.work} is on a tmpfs: the first run is on a disk (the second, `tmpfs`, is the tmpfs one)")
    print(f"real smoke of {asked.blaze}, load {os.getloadavg()[0]:.0f}", flush=True)
    good = place(f"disk({disk or '?'})", Path(asked.work), asked)
    if asked.tmpfs:
        parent = Path(asked.tmpfs) if Path(asked.tmpfs).exists() else Path(asked.tmpfs).parent
        memory = filesystem(parent)
        if memory != "tmpfs":
            sys.exit(f"tmpfs {asked.tmpfs} is on {memory or 'a file system /proc/mounts does not name'}, not a tmpfs")
        good = place("tmpfs", Path(asked.tmpfs), asked) and good
        shutil.rmtree(asked.tmpfs, ignore_errors=True)
    else:
        print("tmpfs            NOT RUN: no `tmpfs` directory was given", flush=True)
    sys.exit(0 if good else 1)


if __name__ == "__main__":
    main(sys.argv)
