#!/usr/bin/env python3
"""restart_rows.py: what a restart costs, a row a project (lane records-restart, 2026-11-12;
docs/v2/plan/design/restore.md, "Restart reads records (d)").

    python3 -I harness/v2/restart_rows.py '{"blaze": "/root/wt/round/inst-<commit>", "work": "/root/wt/round/restart"}'

The input is one JSON object (`Input`), unknown fields refused:

    blaze     the directory of an installed blaze (`blaze` and `blaze-engine`)
    work      a directory: each project's copy and home are made new under it
    projects  which of `hello`, `six`, `smoke`, in that order (all by default)
    kill      true: the daemon is ended with `kill -9` in place of `blaze stop` (what a crash
              leaves is what the start has)
    env       more variables for every command (a switch under test)
    path      the PATH of the fixed environment
    label     printed with every row

For each project, in a home of its own: a cold `blaze run`; a `blaze run` with nothing changed
(so the first build after a cold one, which makes the units that called an unseen proc macro
again, is not the one measured); a wait until the daemon is idle; its footprint; the stop; then
the rows:

    restart no-op    the first `blaze run` after the stop, nothing changed
    warm no-op       the next one, for the difference
    restart edit     stop again, change one function's body, `blaze run`

Each row: milliseconds, engine rows executed and verified and inputs written (`blaze explain
edit_work`), the resolution, expansion and item-tree queries run by kind (`blaze explain
counts_by_cause`), units made, and the daemon's footprint after it (`blaze debug inspect`:
the number the kernel enforces, rule one-memory-measure). A row counts only when its command
exited 0 and printed the program's line for that step's sources (ran.py); a row that does not
says INVALID and why, and the script exits 1. The last line is every row as one JSON value.

The projects: `hello` (one file, no dependency); `six` (regex, hashbrown, bitflags, itoa,
memchr, smallvec: the fixture of restore.md's Relax table; its versions are resolved by the
cold build, so two runs on different days may build different crates: the lock it wrote is
printed by hash); `smoke` (harness/v2/smoke: build scripts, a proc macro, C).
"""
import hashlib
import json
import os
import shutil
import sys
import time
import typing
from dataclasses import dataclass, field
from pathlib import Path

# Run with `python3 -I`, which leaves the script's directory off the path: ran.py is beside it.
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import ran
from ran import Command, Proof

PROJECTS = ("hello", "six", "smoke")
SMOKE = Path(os.path.dirname(os.path.abspath(__file__))) / "smoke"
BSTD = "../../../../crates/blaze-std"
# The queries a crate read from its records must not run (restore.md, d): name resolution,
# macro expansion, item trees and parsing. A kind is counted when it holds one of these.
FRONT = ("item_tree", "parse", "macro_expan", "expand", "def_map", "nameres", "macro_arg", "ast_id_map", "span_map")
# A row of this script's output, or one JSON object a command printed.
type Row = dict[str, object]


def objects(out: str) -> list[Row]:
    """Each line of `out` that is one JSON object, decoded; any other line is not a row."""
    rows: list[Row] = []
    for line in out.splitlines():
        if not line.startswith("{"):
            continue
        try:
            value: object = json.loads(line)
        except ValueError:
            continue
        if isinstance(value, dict):
            rows.append(typing.cast(Row, value))
    return rows


def at(value: object, *keys: str) -> object:
    """`value[keys[0]][keys[1]]..`, or None where a step is not an object holding the key."""
    for key in keys:
        if not isinstance(value, dict):
            return None
        value = typing.cast(Row, value).get(key)
    return value


@dataclass(frozen=True)
class Input:
    blaze: str
    work: str
    projects: tuple[str, ...] = PROJECTS
    kill: bool = False
    env: dict[str, str] = field(default_factory=dict[str, str])
    path: str = "/run/current-system/sw/bin:/usr/local/bin:/usr/bin:/bin"
    label: str = ""


@dataclass(frozen=True)
class Project:
    """A project's copy: where it is, what `blaze run` takes, the file an edit changes, the
    text of it that holds the value, and the line the program prints for a value."""
    dir: Path
    args: tuple[str, ...]
    file: Path
    line: str


def make(name: str, root: Path) -> Project:
    d = root / "p"
    if name == "smoke":
        shutil.copytree(SMOKE, d)
        member = d / "edition/Cargo.toml"
        text = member.read_text()
        if text.count(BSTD) != 1:
            sys.exit(f"{member} does not name bstd as {BSTD}")
        member.write_text(text.replace(BSTD, str(SMOKE.parents[2] / "crates/blaze-std")))
        return Project(d, ("-p", "smoke"), d / "src/main.rs", 'smoke {{"x":{x},"y":2}} CachePadded {{ value: Point {{ x: {x}, y: 2 }} }} 1')
    (d / "src").mkdir(parents=True)
    deps = ""
    body = 'fn main() {\n    println!("restart {}", answer());\n}\n'
    if name == "six":
        deps = ('\n[dependencies]\nregex = "1"\nhashbrown = "0.15"\nbitflags = "2"\nitoa = "1"\nmemchr = "2"\nsmallvec = "1"\n')
        body = (
            "bitflags::bitflags! {\n    struct Flags: u8 {\n        const A = 1;\n    }\n}\n"
            "fn main() {\n"
            '    let re = regex::Regex::new("a+").unwrap();\n'
            "    let mut map = hashbrown::HashMap::new();\n"
            '    map.insert(memchr::memchr(b\'a\', b"ba").unwrap(), Flags::A.bits());\n'
            "    let mut small: smallvec::SmallVec<[u8; 4]> = smallvec::SmallVec::new();\n"
            "    small.push(map[&1]);\n"
            '    assert!(re.is_match("aa") && small.len() == 1);\n'
            '    println!("restart {}", itoa::Buffer::new().format(answer()));\n'
            "}\n"
        )
    (d / "Cargo.toml").write_text(f'[package]\nname = "restart-{name}"\nversion = "0.1.0"\nedition = "2024"\n{deps}')
    (d / "src/main.rs").write_text("fn answer() -> i32 {\n    1\n}\n" + body)
    return Project(d, (), d / "src/main.rs", "restart {x}")


def explain(blaze: str, view: str, build: int, project: Project, env: dict[str, str]) -> list[Row]:
    """`blaze explain <view> build=N --json`: its rows, or one row `{"error": ..}`."""
    code, out, err = ran.spawn(Command((blaze, "explain", view, f"build={build}", "limit=2000", "--json"), str(project.dir), env, timeout=300))
    rows = objects(out)
    if code != 0 and not rows:
        # A build that counted no work has no rows and says so: that is zeros, not a failure.
        if "counted no work in any stage" in err:
            return []
        return [{"error": f"`blaze explain {view} build={build}` exited {code}: {ran.tail(err)}"}]
    return rows


def footprint(blaze: str, project: Project, env: dict[str, str]) -> int | None:
    """The daemon's footprint in bytes (`blaze debug inspect`), or None when it does not say."""
    code, out, _ = ran.spawn(Command((blaze, "debug", "inspect"), str(project.dir), env, timeout=60))
    if code != 0:
        return None
    for it in objects(out):
        value = at(it, "memory", "footprint")
        return value if isinstance(value, int) else None
    return None


def idle(blaze: str, project: Project, env: dict[str, str]) -> str:
    """Waits until the daemon runs no task (two looks a second apart, at most 120 s): what a
    build left for after its answer is stored. Says why when it never is."""
    began = time.time()
    quiet = 0
    while time.time() - began < 120:
        code, out, _ = ran.spawn(Command((blaze, "debug", "inspect"), str(project.dir), env, timeout=60))
        running = None
        if code == 0:
            for it in objects(out):
                tasks = it.get("tasks", [])
                if isinstance(tasks, list):
                    running = sum(1 for task in typing.cast(list[object], tasks) if at(task, "state") == "running")
        quiet = quiet + 1 if running == 0 else 0
        if quiet >= 2:
            return ""
        time.sleep(1)
    return "the daemon was not idle within 120 s"


def daemon_pid(blaze: str, project: Project, env: dict[str, str]) -> int | None:
    code, out, _ = ran.spawn(Command((blaze, "debug", "inspect"), str(project.dir), env, timeout=60))
    if code != 0:
        return None
    for it in objects(out):
        pid = it.get("pid")
        return pid if isinstance(pid, int) else None
    return None


def end(asked: Input, blaze: str, project: Project, env: dict[str, str]) -> str:
    """The daemon ended, by `blaze stop` or `kill -9`; why not, when it is not."""
    if not asked.kill:
        code, out, err = ran.spawn(Command((blaze, "stop"), str(project.dir), env, timeout=600))
        return "" if code == 0 else f"`blaze stop` exited {code}: {ran.tail(out + err)}"
    pid = daemon_pid(blaze, project, env)
    if pid is None:
        return "no daemon answered `blaze debug inspect`, so there is none to kill"
    os.kill(pid, 9)
    began = time.time()
    while time.time() - began < 30:
        try:
            os.kill(pid, 0)
        except ProcessLookupError:
            return ""
        time.sleep(0.05)
    return f"daemon {pid} outlived kill -9 by 30 s"


def step(name: str, x: int, asked: Input, blaze: str, project: Project, env: dict[str, str]) -> Row:
    """One `blaze run`, held to exit 0 and the program's line for value `x`, with what the
    build did."""
    args = (blaze, "run", "--message-format=json", *project.args)
    r = ran.run(Command(args, str(project.dir), env, timeout=3600), Proof(stdout=project.line.format(x=x)))
    row: Row = {"row": name, "ms": round(r.ms, 1), "invalid": r.invalid}
    build: object = None
    noop = False
    for value in objects(r.stdout):
        if value.get("reason") == "blaze-work":
            row["units"] = value.get("compiled")
        if value.get("reason") == "build-finished":
            build = value.get("build_id")
            noop = build is None
    row["build"] = build
    row["answered_as_the_last"] = noop
    if noop:
        # No build ran (the no-op lane): no row ran and no input was written.
        row.update(executed=0, verified=0, written=0, front={}, units=row.get("units") or 0)
    elif isinstance(build, int):
        for it in explain(blaze, "edit_work", build, project, env):
            if "error" in it:
                row["invalid"] = row["invalid"] or it["error"]
            counter = it.get("counter")
            if it.get("stage") == "engine memos" and isinstance(counter, str) and counter in ("executed", "verified", "written"):
                row[counter] = it.get("total")
        front: dict[str, int] = {}
        for it in explain(blaze, "counts_by_cause", build, project, env):
            if "error" in it:
                row["invalid"] = row["invalid"] or it["error"]
                continue
            kind, executed = str(it.get("query", "")), it.get("executed")
            if isinstance(executed, int) and executed > 0 and any(part in kind for part in FRONT):
                front[kind] = executed
        row["front"] = dict(sorted(front.items(), key=lambda it: -it[1]))
        row["front_total"] = sum(front.values())
    else:
        row["invalid"] = row["invalid"] or "the build's number was not in its messages"
    row["footprint"] = footprint(blaze, project, env)
    return row


def said(label: str, project: str, row: Row) -> None:
    def mb(it: object) -> str:
        return f"{it / (1 << 20):.0f} MiB" if isinstance(it, int) else "-"

    def n(key: str) -> str:
        return "-" if row.get(key) is None else str(row[key])

    ms = row["ms"]
    state = f"INVALID: {row['invalid']}" if row["invalid"] else "exit and the program's line checked"
    print(
        f"{label} | {project} | {row['row']!s:14s} | {ms if isinstance(ms, float) else 0.0:9.0f} ms | executed {n('executed')} verified {n('verified')} "
        f"written {n('written')} | front-end queries run {n('front_total')} | units {n('units')} | footprint {mb(row.get('footprint'))} | {state}",
        flush=True,
    )


def one(name: str, asked: Input) -> tuple[list[Row], bool]:
    blaze = str(Path(asked.blaze) / "blaze")
    root = Path(asked.work) / name
    shutil.rmtree(root, ignore_errors=True)
    home = root / "home"
    home.mkdir(parents=True)
    project = make(name, root)
    env = {"HOME": str(home), "PATH": asked.path, "TERM": "dumb", "LANG": "C.UTF-8", **asked.env}
    rows: list[Row] = []

    def add(row: Row) -> None:
        row["project"] = name
        rows.append(row)
        said(asked.label, name, row)

    def note(what: str, why: str) -> None:
        if why:
            add({"row": what, "ms": 0.0, "invalid": why})

    add(step("cold", 1, asked, blaze, project, env))
    lock = project.dir / "Cargo.lock"
    if lock.is_file():
        print(f"{asked.label} | {name} | Cargo.lock sha256 {hashlib.sha256(lock.read_bytes()).hexdigest()[:16]}", flush=True)
    add(step("second", 1, asked, blaze, project, env))
    add(step("warm no-op 0", 1, asked, blaze, project, env))
    note("idle", idle(blaze, project, env))
    before = footprint(blaze, project, env)
    note("stop", end(asked, blaze, project, env))
    row = step("restart no-op", 1, asked, blaze, project, env)
    row["footprint_before_stop"] = before
    add(row)
    add(step("warm no-op", 1, asked, blaze, project, env))
    note("idle", idle(blaze, project, env))
    note("stop", end(asked, blaze, project, env))
    text = project.file.read_text()
    if text.count("\n    1\n") != 1:
        note("restart edit", f"{project.file} has no one body `1` to edit")
    else:
        project.file.write_text(text.replace("\n    1\n", "\n    7\n"))
        add(step("restart edit", 7, asked, blaze, project, env))
    # `blaze stop`, whatever `kill` says: nothing of this home runs on.
    code, out, err = ran.spawn(Command((blaze, "stop"), str(project.dir), env, timeout=600))
    if code != 0:
        note("last stop", f"`blaze stop` exited {code}: {ran.tail(out + err)}")
    return rows, all(not it["invalid"] for it in rows)


def main(argv: list[str]) -> None:
    if len(argv) != 2:
        sys.exit("restart_rows.py takes one JSON value (see its module docs)")
    asked = ran.parsed(Input, json.loads(argv[1]))
    if not asked.projects or [it for it in asked.projects if it not in PROJECTS]:
        sys.exit(f"projects is some of {PROJECTS}, not {asked.projects}")
    for name in ("blaze", "blaze-engine"):
        if not (Path(asked.blaze) / name).is_file():
            sys.exit(f"{asked.blaze} holds no `{name}`: give the directory of an installed blaze")
    print(f"restart rows of {asked.blaze}, load {os.getloadavg()[0]:.0f}, ended by {'kill -9' if asked.kill else 'blaze stop'}", flush=True)
    rows: list[Row] = []
    good = True
    for name in asked.projects:
        got, ok = one(name, asked)
        rows.extend(got)
        good = good and ok
    print(json.dumps({"label": asked.label, "blaze": asked.blaze, "kill": asked.kill, "rows": rows}))
    sys.exit(0 if good else 1)


if __name__ == "__main__":
    main(sys.argv)
