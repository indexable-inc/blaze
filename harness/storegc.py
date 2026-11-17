#!/usr/bin/env python3
"""storegc.py: a home filled past a small budget, and what the store's collector does about it
(docs/gc.md, "The store's budget and collector").

    storegc.py '<json>' | FILE | -

One session, in a fresh home under `work`:

  fill       `projects` generated programs (each `modules` modules of `functions` functions,
             so their records differ) are built and run once each with a budget no home
             reaches (`big_gib`), then project 0 gets `edits` body edits: the edit loop's base
             rows, with no collection.
  shrink     `blaze gc --budget <budget>` sets the budget to `percent` percent of what the
             home holds; right after, project 0 gets `edits` more body edits while the collector
             works: the rows during collection.
  settle     the session waits (`settle_seconds`, asking `blaze gc` once) for the collector to
             rest, then `edits` more body edits: the rows after it.
  kept       project 0 and the last project are built again: each must answer as before, and
             what `blaze gc --dry-run` says is kept is printed.

It prints, and writes to `out` as one JSON object: the home's bytes (the store, the rest of
`v2`, the build dirs; `du`-like, blocks) at each step, the three sets of edit rows with their
medians and the load average when each began, whether any row during collection is more than
`slower_percent` percent slower than the base median, every `blaze gc` report, and the lines
of the daemon's log that name a step of the collector past its bound.

The grace of the store is set to `grace_seconds` in the home's `gc.json` before the daemon
starts (an hour by default, which no session waits for). Nothing here reads `~/.cache/blaze` or
writes outside `work`.
"""

from __future__ import annotations

import json
import os
import re
import shutil
import statistics
import subprocess
import sys
import time
from dataclasses import dataclass

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.join(HERE, "..", "tools", "typed-input", "src"))
from typed_input import fail, parse  # noqa: E402

SCRIPT = "storegc.py"


@dataclass(frozen=True)
class Run:
    # The `blaze` binary, absolute (`blaze-engine` beside it).
    blaze: str
    # What was built, for the report.
    commit: str
    # An empty or missing directory on a path with no symlink in it; emptied first.
    work: str
    # Where the answers go, as JSON.
    out: str
    projects: int = 6
    modules: int = 12
    functions: int = 60
    edits: int = 8
    # The budget while the home is filled, in GiB: one no session reaches.
    big_gib: int = 512
    # The small budget, in percent of what the home holds once filled.
    percent: int = 60
    grace_seconds: int = 5
    settle_seconds: int = 90
    # Between two turns, so an edit does not meet what the daemon does after the last answer.
    pause_seconds: float = 1.0
    # A row during collection this much slower than the base median fails the session.
    slower_percent: int = 10


def module(project: int, number: int, functions: int) -> str:
    lines = [f"//! module {number} of project {project}\n"]
    for f in range(functions):
        seed = project * 100_003 + number * 1_009 + f
        lines.append(
            f"pub fn f{f}(x: u64) -> u64 {{\n"
            f"    let mut acc = x.wrapping_mul({seed * 2 + 1});\n"
            f"    for i in 0..{3 + f % 5}u64 {{\n"
            f"        acc = acc.rotate_left({1 + f % 13}) ^ (i + {seed});\n"
            f"    }}\n"
            f"    acc.wrapping_add({seed})\n"
            f"}}\n"
        )
    lines.append("pub fn all(x: u64) -> u64 {\n    let mut acc = x;\n")
    for f in range(functions):
        lines.append(f"    acc = acc.wrapping_add(f{f}(acc));\n")
    lines.append("    acc\n}\n")
    return "".join(lines)


def main_rs(project: int, modules: int) -> str:
    mods = "".join(f"mod m{m};\n" for m in range(modules))
    calls = "".join(f"    acc = acc.wrapping_add(m{m}::all(acc));\n" for m in range(modules))
    return (
        f"{mods}\nfn edited() -> u64 {{\n    7 + 100\n}}\n\n"
        f"fn main() {{\n    let mut acc = {project}u64;\n{calls}"
        f'    println!("project {project}: {{}} {{}}", edited(), acc);\n}}\n'
    )


def blocks(path: str) -> int:
    """The bytes on disk under `path`, each inode once."""
    seen: set[tuple[int, int]] = set()
    total = 0
    for root, _, files in os.walk(path):
        for name in files:
            try:
                st = os.lstat(os.path.join(root, name))
            except FileNotFoundError:
                continue
            if (st.st_dev, st.st_ino) not in seen:
                seen.add((st.st_dev, st.st_ino))
                total += st.st_blocks * 512
    return total


def held(home: str) -> dict:
    store = blocks(os.path.join(home, "v2", "store"))
    state = blocks(os.path.join(home, "v2"))
    return {
        "store": store,
        "state_besides": state - store,
        "build_dirs": blocks(os.path.join(home, "build")),
        "home": blocks(home),
    }


def session(it: Run) -> dict:
    work = os.path.realpath(it.work)
    if work != os.path.abspath(it.work):
        fail(SCRIPT, f"work {it.work} is under a symlink ({work})")
    if not os.path.isfile(it.blaze):
        fail(SCRIPT, f"{it.blaze} is no file")
    shutil.rmtree(work, ignore_errors=True)
    home, xdg = os.path.join(work, "home"), os.path.join(work, "xdg")
    os.makedirs(home)
    os.makedirs(xdg)
    with open(os.path.join(home, "gc.json"), "w") as f:
        json.dump({"budget_gib": it.big_gib, "store_grace_s": it.grace_seconds}, f)
    dirs = []
    for p in range(it.projects):
        project = os.path.join(work, f"p{p}")
        os.makedirs(os.path.join(project, "src"))
        with open(os.path.join(project, "Cargo.toml"), "w") as f:
            f.write(f'[package]\nname = "p{p}"\nversion = "0.1.0"\nedition = "2021"\n')
        for m in range(it.modules):
            with open(os.path.join(project, "src", f"m{m}.rs"), "w") as f:
                f.write(module(p, m, it.functions))
        with open(os.path.join(project, "src", "main.rs"), "w") as f:
            f.write(main_rs(p, it.modules))
        dirs.append(project)
    env = {
        "HOME": os.environ["HOME"],
        "PATH": os.environ.get("PATH", "/usr/bin:/bin"),
        "BLAZE_HOME": home,
        "XDG_CACHE_HOME": xdg,
        "TMPDIR": os.environ.get("TMPDIR", "/tmp"),
    }
    rows: list[dict] = []
    bad: list[str] = []
    answers: dict[int, str] = {}

    def blaze(args: list[str], cwd: str) -> subprocess.CompletedProcess:
        return subprocess.run([it.blaze, *args], cwd=cwd, env=env, capture_output=True, text=True)

    def turn(phase: str, what: str, project: int, edited: int | None) -> None:
        start = time.monotonic()
        load = os.getloadavg()[0]
        done = blaze(["run", "-q"], dirs[project])
        seconds = time.monotonic() - start
        out = done.stdout.strip().splitlines()[-1] if done.stdout.strip() else ""
        ok = done.returncode == 0 and out.startswith(f"project {project}: ")
        if ok and edited is not None:
            ok = out.split()[2] == str(edited)
        if ok and edited is None:
            # The same program answers the same, whatever was forgotten meanwhile.
            ok = answers.setdefault(project, out.split()[3]) == out.split()[3]
        if not ok:
            bad.append(f"{phase}: {what} of p{project}: exit {done.returncode}: {out!r} {done.stderr[-800:]}")
        rows.append({"phase": phase, "what": what, "project": project, "seconds": seconds, "ok": ok, "load": load})
        time.sleep(it.pause_seconds)

    edit_count = 0

    def edits(phase: str) -> None:
        nonlocal edit_count
        path = os.path.join(dirs[0], "src", "main.rs")
        for _ in range(it.edits):
            edit_count += 1
            with open(path) as f:
                source = f.read()
            changed, n = re.subn(r"7 \+ \d+", f"7 + {100 + edit_count}", source, count=1)
            if n != 1:
                fail(SCRIPT, "the body edit found nothing to change")
            with open(path, "r+") as f:
                f.write(changed)
                f.truncate()
            turn(phase, "body edit", 0, 107 + edit_count)

    reports: dict[str, str] = {}

    def gc(label: str, args: list[str]) -> None:
        done = blaze(["gc", *args], dirs[0])
        reports[label] = done.stdout + done.stderr
        if done.returncode:
            bad.append(f"blaze gc {' '.join(args)}: exit {done.returncode}: {done.stderr[-800:]}")

    snapshots: dict[str, dict] = {}
    for p in range(it.projects):
        turn("fill", "cold build", p, None)
    # A first edit after a cold build does work no later one does: not a row of the base.
    edits("warm up")
    edits("base")
    time.sleep(8)
    snapshots["filled"] = held(home)
    budget_mib = max(1, snapshots["filled"]["home"] * it.percent // 100 >> 20)
    gc("budget", ["--budget", f"{budget_mib}M"])
    edits("collecting")
    snapshots["after the edits during collection"] = held(home)
    gc("asked", [])
    deadline = time.monotonic() + it.settle_seconds
    while time.monotonic() < deadline:
        time.sleep(5)
        now = held(home)
        if now["home"] <= budget_mib << 20:
            break
    snapshots["settled"] = held(home)
    edits("after")
    for p in (it.projects - 1, 1 % it.projects):
        turn("kept", "run again", p, None)
    gc("kept", ["--dry-run"])
    snapshots["end"] = held(home)
    stop = blaze(["stop"], dirs[0])
    if stop.returncode:
        bad.append(f"stop: exit {stop.returncode}: {stop.stderr[-400:]}")
    log = os.path.join(home, "v2", "serve.log")
    over = []
    if os.path.isfile(log):
        with open(log, errors="replace") as f:
            over = [line.strip() for line in f if "past its bound" in line][:40]

    def median(phase: str) -> float | None:
        times = [row["seconds"] for row in rows if row["phase"] == phase and row["what"] == "body edit"]
        return round(statistics.median(times) * 1000, 1) if times else None

    base = median("base")
    slow = [
        row
        for row in rows
        if row["phase"] == "collecting" and base and row["seconds"] * 1000 > base * (100 + it.slower_percent) / 100
    ]
    return {
        "commit": it.commit,
        "budget_mib": budget_mib,
        "snapshots": snapshots,
        "edit_ms": {phase: median(phase) for phase in ("base", "collecting", "after")},
        "load": {phase: [round(row["load"], 1) for row in rows if row["phase"] == phase][:1] for phase in ("base", "collecting", "after")},
        "rows_during_collection_slower_than_allowed": slow,
        "under_budget_at_the_end": snapshots["end"]["home"] <= budget_mib << 20,
        "steps_past_bound": over,
        "reports": reports,
        "turns": rows,
        "bad": bad,
    }


def main() -> None:
    match sys.argv[1:]:
        case [text]:
            it = parse(Run, text, SCRIPT)
        case _:
            fail(SCRIPT, "one argument: the run as JSON, a file of it, or - for stdin")
    answer = session(it)
    with open(it.out, "w") as f:
        json.dump(answer, f, indent=1)
    mib = lambda n: f"{n / (1 << 20):9.1f} MiB"  # noqa: E731
    print(f"commit {answer['commit']}; budget {answer['budget_mib']} MiB")
    print(f"{'':36}{'home':>13}{'store':>13}{'rest of v2':>13}{'build dirs':>13}")
    for label, it_ in answer["snapshots"].items():
        print(f"{label:36}{mib(it_['home'])}{mib(it_['store'])}{mib(it_['state_besides'])}{mib(it_['build_dirs'])}")
    print("body edit, median ms:", answer["edit_ms"], "load at start:", answer["load"])
    print("rows during collection past the allowed slowdown:", len(answer["rows_during_collection_slower_than_allowed"]))
    print("under budget at the end:", answer["under_budget_at_the_end"])
    print("collector steps past their bound, as the daemon's log says:", len(answer["steps_past_bound"]))
    for label, report in answer["reports"].items():
        print(f"--- blaze gc ({label})\n{report}")
    if answer["bad"]:
        print("BAD:")
        for line in answer["bad"]:
            print(" ", line)
        sys.exit(1)


if __name__ == "__main__":
    main()
