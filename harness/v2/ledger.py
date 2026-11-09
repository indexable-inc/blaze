"""ledger.py: the objectives ledger (docs/v2/plan.md, "How the work is run"). The rows are data,
`docs/v2/plan/objectives.json`; this script checks them, runs their checks and writes what they
said, picks what to work on next, and writes the table people read, docs/v2/plan/objectives.md.

    uv run --frozen python harness/v2/ledger.py '{"command": "check"}'
    uv run --frozen python harness/v2/ledger.py '{"command": "write"}'
    uv run --frozen python harness/v2/ledger.py '{"command": "pick", "count": 4}'
    uv run --frozen python harness/v2/ledger.py '{"command": "run", "engine": {"kind": "v2", "blaze": "target/release/blaze"}, "ids": ["T5"]}'
    uv run --frozen python harness/v2/ledger.py selftest

The input is one JSON object:

    command   check   the rows are well formed and objectives.md is what `write` would write (CI)
              write   regenerate objectives.md from the rows
              pick    print up to `count` rows to work on now: not met, not blocked, everything
                      they need met, no two touching the same paths
              run     run the checks of the rows in `ids` (all rows that have one when empty; with
                      `only`, those in that state) and write each row's status
    root      the checkout (".")
    engine    `run`: the blaze measured (engine.EngineSpec), handed to each check
    work      `run`: where each check's result goes; a new directory under /tmp when empty
    ids, only, count   as above

A row of objectives.json:

    id          short and never reused (T5, D12)
    group       targets, correctness, design or app
    objective   the outcome, one sentence a check can decide
    checks      what decides it, all of which must pass. Each is a harness script that takes one
                JSON object and exits 0 when it holds: `script`, `input` (its fields but the ones
                below), `command` (its first argument, for a script that has several) and `takes`
                (which of `engine`, `blaze` and `out` this script is handed; engine and out)
    missing     the check still to be written, in words; empty when `checks` decides the row. A
                row with something missing is never met: writing it is the first part of the work
    paths       the directories the work touches, so `pick` never hands out two that collide
    needs       ids that must be met first
    known       the last figure anyone measured, with its date: a lead, not a status
    status      written by `run`, except `blocked`, which a person or an agent writes with why:
                state (`?` not measured, `met`, `open`, `blocked`), commit, date, detail

`run` never trusts a tree it cannot name: the status carries the commit, with `+dirty` when the
checkout had changes. Exit status: `check` 1 on any problem; `run` 1 when a row it ran is not met.
"""

import json
import subprocess
import sys
import tempfile
import time
from dataclasses import asdict, dataclass, field, replace
from pathlib import Path
from typing import Any, Literal

HERE = Path(__file__).resolve().parent
sys.path[:0] = [str(HERE), str(HERE.parent)]
from engine import EngineSpec
from typed_input import fail, parse

SCRIPT = "ledger.py"
DATA = "docs/v2/plan/objectives.json"
TABLE = "docs/v2/plan/objectives.md"
type State = Literal["?", "met", "open", "blocked"]
type Group = Literal["targets", "correctness", "design", "app"]
type Take = Literal["engine", "blaze", "out"]
GROUPS: dict[str, tuple[str, str]] = {
    "targets": ("Targets", "What a user feels, on one warm machine."),
    "correctness": ("Correctness", "What must hold for any of the speed to count."),
    "design": ("The design, part by part", "The parts of [the design](../plan.md#the-design) no check has shown yet."),
    "app": ("The agent app", "The app in `crates/harness/`, where people watch the work ([tasks](design/tasks.md))."),
}


@dataclass(frozen=True)
class Check:
    script: str
    input: Any = None
    command: str = ""
    takes: tuple[Take, ...] = ("engine", "out")


@dataclass(frozen=True)
class Status:
    state: State = "?"
    commit: str = ""
    date: str = ""
    detail: str = ""


@dataclass(frozen=True)
class Row:
    id: str
    group: Group
    objective: str
    paths: tuple[str, ...]
    checks: tuple[Check, ...] = ()
    missing: str = ""
    needs: tuple[str, ...] = ()
    known: str = ""
    status: Status = field(default_factory=Status)


@dataclass(frozen=True)
class Ledger:
    rows: tuple[Row, ...]
    questions: tuple[str, ...] = ()


@dataclass(frozen=True)
class Input:
    command: Literal["check", "write", "pick", "run"]
    root: str = "."
    engine: EngineSpec | None = None
    work: str = ""
    ids: tuple[str, ...] = ()
    only: State | None = None
    count: int = 4


def dump(ledger: Ledger) -> str:
    """One row a line, so a status change is a one-line diff."""
    rows = ",\n".join("    " + json.dumps(asdict(r), ensure_ascii=False) for r in ledger.rows)
    questions = ",\n".join("    " + json.dumps(q, ensure_ascii=False) for q in ledger.questions)
    return f'{{\n  "rows": [\n{rows}\n  ],\n  "questions": [\n{questions}\n  ]\n}}\n'


def problems(ledger: Ledger, root: Path | None) -> list[str]:
    """What is wrong with the rows; `root` also holds their scripts and paths to the tree."""
    out: list[str] = []
    ids = [r.id for r in ledger.rows]
    out += [f"{i}: the id is used twice" for i in sorted({i for i in ids if ids.count(i) > 1})]
    for r in ledger.rows:
        if not r.checks and not r.missing:
            out.append(f"{r.id}: no check and nothing said to be missing: nothing can decide it")
        if r.status.state == "met" and (r.missing or not r.status.commit):
            out.append(f"{r.id}: met needs every check written and the commit it passed on")
        if r.status.state == "blocked" and not r.status.detail:
            out.append(f"{r.id}: blocked says by what, in status.detail")
        if not r.paths:
            out.append(f"{r.id}: no paths, so `pick` cannot tell what it collides with")
        out += [f"{r.id}: needs {n}, which is no row" for n in r.needs if n not in ids]
        if root is not None:
            out += [f"{r.id}: {c.script} does not exist" for c in r.checks if not (root / c.script).is_file()]
            out += [f"{r.id}: the path {p} does not exist" for p in r.paths if not (root / p).exists()]
    return out


def shown(c: Check) -> str:
    given = "" if c.input in (None, {}) else " " + json.dumps(c.input, ensure_ascii=False)
    return f"`{Path(c.script).name}{' ' + c.command if c.command else ''}{given}`".replace("|", "\\|")


def table(ledger: Ledger) -> str:
    counts = {s: sum(r.status.state == s for r in ledger.rows) for s in ("met", "open", "blocked", "?")}
    lines = [
        "---", "status: plan",
        "summary: The ledger. Every objective of the plan, the check that decides it and where it stands; "
        "generated from objectives.json by harness/v2/ledger.py.",
        "date: 2026-11-09", "---", "# Objectives", "",
        "<!-- Generated by harness/v2/ledger.py from objectives.json: edit the rows there, then run `write`. -->", "",
        "One row per outcome, decided by its check on a named commit, never by a status someone",
        "remembered. How rows are picked, worked and landed is",
        "[How the work is run](../plan.md#how-the-work-is-run); the fields and the commands are in",
        "`harness/v2/ledger.py`. A check in *italics* is not written yet, and writing it is the first",
        "part of the work. `?` is not measured; \"last known\" is a lead, not a status.", "",
        f"{len(ledger.rows)} objectives: {counts['met']} met, {counts['open']} open, {counts['blocked']} blocked, "
        f"{counts['?']} not measured.", "",
    ]
    for group, (title, what) in GROUPS.items():
        lines += [f"## {title}", "", what, "", "| id | objective | check | last known | status |", "|---|---|---|---|---|"]
        for r in (r for r in ledger.rows if r.group == group):
            checks = "; ".join([shown(c) for c in r.checks] + ([f"*write: {r.missing}*"] if r.missing else []))
            s = r.status
            status = s.state + (f" `{s.commit[:10]}`" if s.commit else "") + (f": {s.detail}" if s.detail else "")
            cells = [r.id, r.objective, checks, r.known, status]
            lines.append("| " + " | ".join(c.replace("\n", " ") for c in cells) + " |")
        lines.append("")
    lines += ["## Open design questions", "", "Decide with evidence, fold the answer into the plan, delete the question.", ""]
    lines += [f"- {q}" for q in ledger.questions]
    return "\n".join(lines) + "\n"


def collide(a: Row, b: Row) -> bool:
    return any(p == q or p.startswith(q.rstrip("/") + "/") or q.startswith(p.rstrip("/") + "/")
               for p in a.paths for q in b.paths)


def pick(ledger: Ledger, count: int) -> list[Row]:
    met = {r.id for r in ledger.rows if r.status.state == "met"}
    picked: list[Row] = []
    for r in ledger.rows:
        ready = r.status.state in ("?", "open") and all(n in met for n in r.needs)
        if ready and len(picked) < count and not any(collide(r, p) for p in picked):
            picked.append(r)
    return picked


def judged(row: Row, codes: list[tuple[Check, int, str]], commit: str, date: str) -> Status:
    """A row's status from what each of its checks exited with and said."""
    failed = [f"{Path(c.script).name}: {said}" for c, code, said in codes if code != 0]
    if failed:
        return Status("open", commit, date, "; ".join(failed)[:300])
    if row.missing:
        return Status("open", commit, date, f"its checks pass; still to write: {row.missing}"[:300])
    return Status("met", commit, date)


def said(out: Path, code: int) -> str:
    """Why a check failed, from the result it wrote (report.py's shape), else its exit status."""
    try:
        outcomes = json.loads(out.read_text())["outcomes"]
        bad = [f"{o['name']} {o['verdict']}" + (f" ({o['detail']})" if o.get("detail") else "")
               for o in outcomes if o["verdict"] != "pass"]
        return "; ".join(bad) or f"exit {code}"
    except (OSError, ValueError, KeyError, TypeError):
        return f"exit {code} and no result read"


def run(it: Input, root: Path, ledger: Ledger) -> Ledger:
    if it.engine is None:
        fail(SCRIPT, "run needs `engine`")
    engine = it.engine

    def git(*args: str) -> str:
        return subprocess.run(["git", "-C", str(root), *args], capture_output=True, text=True).stdout.strip()

    commit = git("rev-parse", "HEAD") + ("+dirty" if git("status", "--porcelain", "--untracked-files=no") else "")
    work = Path(it.work or tempfile.mkdtemp(prefix="ledger-", dir="/tmp"))
    work.mkdir(parents=True, exist_ok=True)
    unknown = sorted(set(it.ids) - {r.id for r in ledger.rows})
    if unknown:
        fail(SCRIPT, f"no row {', '.join(unknown)}")
    rows: list[Row] = []
    for r in ledger.rows:
        chosen = r.checks and (r.id in it.ids if it.ids else True) and it.only in (None, r.status.state)
        if not chosen:
            rows.append(r)
            continue
        codes: list[tuple[Check, int, str]] = []
        for n, c in enumerate(r.checks):
            out = work / f"{r.id}-{n}.json"
            given = dict(c.input or {})
            given |= {"engine": asdict(engine)} if "engine" in c.takes else {}
            given |= {"blaze": engine.blaze} if "blaze" in c.takes else {}
            given |= {"out": str(out)} if "out" in c.takes else {}
            argv = ["uv", "run", "--frozen", "python", c.script, *([c.command] if c.command else []), json.dumps(given)]
            print(f"{r.id}: {' '.join(argv[4:6])} ...", flush=True)
            code = subprocess.run(argv, cwd=root).returncode
            codes.append((c, code, "" if code == 0 else said(out, code)))
        status = judged(r, codes, commit, time.strftime("%Y-%m-%d"))
        print(f"{r.id}: {status.state}{': ' + status.detail if status.detail else ''}", flush=True)
        rows.append(replace(r, status=status))
    return replace(ledger, rows=tuple(rows))


def main(it: Input) -> None:
    root = Path(it.root).resolve()
    ledger = parse(Ledger, str(root / DATA), SCRIPT)
    wrong = problems(ledger, root)
    if it.command == "check":
        if (root / DATA).read_text() != dump(ledger):
            wrong.append(f"{DATA} is not as `write` writes it")
        if (root / TABLE).read_text() != table(ledger):
            wrong.append(f"{TABLE} is stale: run ledger.py with command `write`")
    if wrong:
        sys.exit("\n".join(f"{SCRIPT}: {w}" for w in wrong))
    if it.command == "pick":
        for r in pick(ledger, it.count):
            first = f"write its check ({r.missing})" if r.missing else "run its check, then close the gap"
            print(f"{r.id}  {r.objective}\n    first: {first}\n    paths: {', '.join(r.paths)}")
    ran = run(it, root, ledger) if it.command == "run" else ledger
    if it.command in ("write", "run"):
        (root / DATA).write_text(dump(ran))
        (root / TABLE).write_text(table(ran))
    if it.command == "run":
        touched = [n for o, n in zip(ledger.rows, ran.rows) if o is not n]
        sys.exit(1 if any(r.status.state != "met" for r in touched) else 0)


def expect(holds: bool, what: str) -> None:
    if not holds:
        raise SystemExit(f"{SCRIPT} selftest: {what}")


def selftest() -> None:
    check = Check("harness/v2/budgets.py", {"budgets": ["noop"]})
    a = Row("A", "targets", "a", ("crates/x",), (check,))
    b = Row("B", "design", "b", ("crates/x/src",), (), "a lint", ("A",))
    c = Row("C", "design", "c", ("crates/y",), (check,), "an oracle")
    ledger = Ledger((a, b, c), ("q?",))
    expect(problems(ledger, None) == [], f"a sound ledger has no problems: {problems(ledger, None)}")
    bad = Ledger((a, replace(a, checks=()), replace(c, id="Z", needs=("N",), paths=(), status=Status("met", "abc"))))
    found = problems(bad, None)
    for what in ("A: the id is used twice", "nothing can decide it", "Z: needs N", "Z: no paths", "Z: met needs"):
        expect(any(what in f for f in found), f"`{what}` is reported, of {found}")
    expect(collide(a, b) and not collide(a, c), "a path under another collides; siblings do not")
    expect([r.id for r in pick(ledger, 4)] == ["A", "C"], "B waits for A and collides with it")
    met = replace(ledger, rows=(replace(a, status=Status("met", "abc", "2026-11-09")), b, c))
    expect([r.id for r in pick(met, 4)] == ["B", "C"], "once A is met, B is ready and A is not picked")
    expect([r.id for r in pick(met, 1)] == ["B"], "count bounds the pick")
    expect(judged(a, [(check, 0, "")], "abc", "d") == Status("met", "abc", "d"), "every check passing is met")
    expect(judged(a, [(check, 1, "noop fail")], "abc", "d").state == "open", "a failing check is open")
    still = judged(c, [(check, 0, "")], "abc", "d")
    expect(still.state == "open" and "an oracle" in still.detail, "a row with a check to write is never met")
    text = dump(met)
    back = parse(Ledger, text, SCRIPT)
    expect(back == met and dump(back) == text, "the rows round-trip byte for byte")
    page = table(met)
    expect("| A | a | `budgets.py {\"budgets\": [\"noop\"]}` |  | met `abc` |" in page, f"A's line: {page}")
    expect("*write: a lint*" in page and "3 objectives: 1 met, 0 open, 0 blocked, 2 not measured." in page, "the page")
    with tempfile.TemporaryDirectory() as tmp:
        out = Path(tmp) / "r.json"
        out.write_text(json.dumps({"outcomes": [{"name": "noop", "verdict": "fail", "detail": "11 ms over 5"},
                                                {"name": "x", "verdict": "pass"}]}))
        expect(said(out, 1) == "noop fail (11 ms over 5)", f"a failure's words: {said(out, 1)}")
        expect(said(Path(tmp) / "none.json", 2) == "exit 2 and no result read", "no result is said so")
    print(f"{SCRIPT} selftest: ok")


if __name__ == "__main__":
    if len(sys.argv) != 2:
        sys.exit(f"usage: {SCRIPT} '<json>' | selftest\n\n{__doc__}")
    if sys.argv[1] == "selftest":
        selftest()
    else:
        main(parse(Input, sys.argv[1], SCRIPT))
