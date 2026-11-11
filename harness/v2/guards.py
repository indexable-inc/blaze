"""guards.py: the guards of the budget smoke (harness/v2/budgets.py, `smoke`): what a warm build
must not cost, held on two fixed fixtures by the merge gate with the blaze the change built.

    uv run --frozen python harness/v2/guards.py '{"command": "held", "result": "/tmp/budgets-smoke.json"}'
    uv run --frozen python harness/v2/guards.py '{"command": "table"}'
    uv run --frozen python harness/v2/guards.py selftest

The rule it enforces: work that should be zero is counted, and what is counted is held by the
gate, so a regression of ten times is a red run at the change that made it and not a figure
someone finds a week later (rule guard-the-cause; docs/harness.md, "The smoke's guards").

The fixtures are a one-file program (`onefile_source`) and fixture.py's workspace. On each the
smoke times three warm builds, one of each `Step`:

    noop      `blaze build` with nothing changed
    comment   a comment added at the end of a line (no line moves, no token changes), then a build
    body      a statement added to a function's body, then a build

and after each asks the daemon's profile for what that build did (`facts_sql`,
`uncovered_sql`, through `blaze debug sql --json`) and reads the daemon's log. `BOUNDS` is the one
table of what each may cost; `judge` holds a step's facts to it. The bounds are loose in absolute
terms: they are for the gate's machine class (16 cores, a blaze the tree built) and catch a
regression of an order of magnitude, not of 10%. They tighten as the numbers fall: edit the table.

A bound that fails today is listed in harness/v2/guards-known.toml with the number measured and
its cause, as harness/known-failures.toml lists the unit tests that fail: a listed check may fail
by no more than what is recorded (so it cannot get worse), one listed that passes fails the run
until it is taken out (so the list only shrinks), and one not listed that fails fails the run.
`budgets.py '{"command": "smoke", "record": true, ...}'` writes the list again from the run,
keeping the cause of an entry that was there.

Nothing asked for is dropped: `names()` is every outcome the smoke has to give, and `held`
(the gate's `ci` job runs it on the smoke's result) fails when one is missing or did not pass, or
when there is no result at all. A gate run whose smoke did not run is red.

`held`'s input is one JSON object: `result`, the path of the smoke's result (report.py's JSON).
`table` prints `BOUNDS` as docs/harness.md shows it. `selftest` needs no blaze: it replays the
finding each bound is for, in miniature, as facts the bound has to refuse, and the fixed state as
facts it has to pass.
"""

import json
import sys
import tomllib
from collections.abc import Mapping, Sequence
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Literal, cast, get_args

HERE = Path(__file__).resolve().parent
sys.path[:0] = [str(HERE), str(HERE.parent)]
from typed_input import fail, parse

SCRIPT = "guards.py"
KNOWN = HERE / "guards-known.toml"

Step = Literal["noop", "comment", "body"]
Fixture = Literal["onefile", "workspace"]
Verdict = Literal["pass", "fail", "unmeasured"]
STEPS: tuple[Step, ...] = get_args(Step)
FIXTURES: tuple[Fixture, ...] = get_args(Fixture)


@dataclass(frozen=True)
class Bound:
    """One thing counted on a warm build and the most it may be on each step (None: not held on
    that step). `needs_task`: the fact is of the build's tasks, so a build that ran none has
    none to give, and passes."""

    check: str
    unit: str
    noop: float | None
    comment: float | None
    body: float | None
    what: str
    catches: str
    needs_task: bool = False

    def limit(self, step: Step) -> float | None:
        return {"noop": self.noop, "comment": self.comment, "body": self.body}[step]


# The one table. The times are for the gate's machine class and loose on purpose; the counts
# are what the design says: zero where nothing changed. A comment at a line's end is held to
# what a no-op is (2026-11-10): the no-op lane answers it at the file, with no job, no
# lowering and no query (blaze-cargo's `serve::job::noop`, "An edit that changes no code").
BOUNDS: tuple[Bound, ...] = (
    Bound("ms", "ms", 300, 300, 2000, "the client's wait: `blaze build` from its spawn to its exit",
          "edits that took 25 to 39 s for days (2026-11-10)"),
    Bound("let_go", "spans", 0, 0, 0, "`kept front ends let go for memory` spans since the fixture was warm",
          "every build dropped the kept front end and lowered its program anew"),
    Bound("let_go_logged", "lines", 0, 0, 0,
          "lines of the daemon's log, since the fixture was warm, saying a kept front end is let go",
          "the same, from the log: \"a task waits for 0.03 GiB\""),
    Bound("ran_new", "runs", 0, 0, 256, "`query_counts.ran_new` summed: first runs of a row, in a database that was warm",
          "a warm edit that ran the library's queries anew, as a new front end does"),
    Bound("ran_untracked", "runs", 0, 0, None, "`query_counts.ran_untracked` summed: runs because of an untracked read",
          "`identity_of`, untracked, ran about 10,000 times on every build"),
    Bound("executed", "runs", 0, 0, None, "`query_counts.executed` summed: every query run",
          "work on a build that changed nothing"),
    Bound("lower_ms", "ms", 5, 5, None, "the `lower workspace` spans of the build, summed",
          "a comment edit spent about 200 ms lowering a workspace that had not changed"),
    Bound("maintain_ms", "ms", 5, 5, None, "the `maintain` spans of the build, summed (`maintainer reads` is inside them)",
          "a comment edit spent about 190 ms in maintainer reads though nothing they read changed"),
    Bound("answer_delay_ms", "ms", 100, 100, 100,
          "from the end of the build's last `task` span to the end of its `request: read to reply` span",
          "a build's answer held 4 s for a disk sync of derived data", needs_task=True),
    Bound("turn_uncovered_ms", "ms", 50, 50, 50,
          "the most time one `loop turn` span has that no child span of it covers",
          "the daemon's loop thread spent 0.8 to 1.1 s in one turn with no span"),
    Bound("uncovered_ms", "ms", 100, 100, 100, "the view `uncovered_time` of the build: wall time inside no span",
          "time the profile cannot name (every-wait-says-what, slow-is-p0-traced)"),
)
# The other outcome the smoke has to give: the build-script sandbox's (`probe_source`).
PROBE = "guard: sandbox: a build script that runs rustc ends, and its probe compiles"
# What a known failure may exceed its recorded number by: a count is exact, a time is noisy.
SLACK = {"ms": 1.5}
LET_GO_LINE = "is let go, and its next build lowers anew"


def name(fixture: Fixture, step: Step, check: str) -> str:
    return f"guard: {fixture}: {step}: {check}"


def names() -> list[str]:
    """Every outcome the smoke has to give: a bound on a step of a fixture, and the probe."""
    return [name(fixture, step, bound.check) for fixture in FIXTURES for step in STEPS for bound in BOUNDS
            if bound.limit(step) is not None] + [PROBE]


# What the profile says of the last build. `warm_from` is the daemon's clock (ns) when the
# fixture was warm (`mark_sql`). A span is taken by when it began, not by its `build_id`: the
# loop thread's spans are of no build. The reply's span may end after the build's row does (an
# answer held back), so the window runs to the later of the two.
FACTS_SQL = """
WITH b AS (
    SELECT build_id, started_ns AS t0, started_ns + coalesce(dur_ns, 0) AS t1
    FROM builds ORDER BY build_id DESC LIMIT 1
), reply AS (
    SELECT max(s.start_ns + s.dur_ns) AS at
    FROM spans s, b
    WHERE s.name = 'request: read to reply' AND s.start_ns <= b.t1 AND s.start_ns + s.dur_ns >= b.t0
), w AS (
    SELECT b.build_id, b.t0, greatest(b.t1, coalesce(reply.at, b.t1)) AS t1, reply.at AS replied
    FROM b, reply
), tasks AS (
    SELECT max(s.start_ns + s.dur_ns) AS last_end, count(*) AS n
    FROM spans s, w
    WHERE s.name = 'task' AND s.start_ns >= w.t0 AND s.start_ns <= w.t1
), turns AS (
    SELECT any_value(t.dur_ns) - coalesce(sum(c.dur_ns), 0) AS own_ns
    FROM spans t
    JOIN w ON t.start_ns >= w.t0 AND t.start_ns <= w.t1
    LEFT JOIN spans c ON c.parent = t.span_id AND c.process = t.process
    WHERE t.name = 'loop turn'
    GROUP BY t.process, t.span_id
), worst AS (
    SELECT coalesce(max(own_ns), 0) AS own_ns FROM turns
), named AS (
    SELECT
        coalesce(sum(s.dur_ns) FILTER (WHERE s.name = 'lower workspace'), 0) AS lower_ns,
        coalesce(sum(s.dur_ns) FILTER (WHERE s.name = 'maintain'), 0) AS maintain_ns
    FROM spans s, w
    WHERE s.start_ns >= w.t0 AND s.start_ns <= w.t1
), counts AS (
    SELECT coalesce(sum(q.executed), 0) AS executed, coalesce(sum(q.ran_new), 0) AS ran_new,
        coalesce(sum(q.ran_untracked), 0) AS ran_untracked
    FROM query_counts q, w
    WHERE q.build_id = w.build_id
), gone AS (
    SELECT count(*) AS n FROM spans s
    WHERE s.name = 'kept front ends let go for memory' AND s.start_ns >= @WARM_FROM@
)
SELECT w.build_id AS build_id,
    counts.executed AS executed, counts.ran_new AS ran_new, counts.ran_untracked AS ran_untracked,
    gone.n AS let_go,
    named.lower_ns / 1e6 AS lower_ms, named.maintain_ns / 1e6 AS maintain_ms,
    CASE WHEN tasks.n > 0 AND w.replied IS NOT NULL THEN (w.replied - tasks.last_end) / 1e6 END AS answer_delay_ms,
    worst.own_ns / 1e6 AS turn_uncovered_ms,
    tasks.n AS tasks
FROM w, counts, gone, named, tasks, worst
"""
UNCOVERED_SQL = "SELECT uncovered_s * 1000 AS uncovered_ms FROM uncovered_time"
MARK_SQL = "SELECT max(build_id) AS build_id, max(started_ns + coalesce(dur_ns, 0)) AS warm_from FROM builds"
# The facts `FACTS_SQL` and `UNCOVERED_SQL` give, by column; `ms` and `let_go_logged` are the harness's own.
PROFILE_FACTS = ("executed", "ran_new", "ran_untracked", "let_go", "lower_ms", "maintain_ms", "answer_delay_ms",
                 "turn_uncovered_ms", "tasks", "build_id")


def facts_sql(warm_from: int) -> str:
    return FACTS_SQL.replace("@WARM_FROM@", str(int(warm_from)))


def rows(stdout: str, what: str) -> list[dict[str, object]]:
    """The rows `blaze debug sql --json` printed: an object a line. A line that is none is a
    ValueError naming `what`."""
    out: list[dict[str, object]] = []
    for line in stdout.splitlines():
        if not line.strip():
            continue
        try:
            row = json.loads(line)
        except json.JSONDecodeError as e:
            raise ValueError(f"{what}: `blaze debug sql --json` printed a line that is no JSON ({e}): {line[:200]!r}") from None
        if not isinstance(row, dict):
            raise ValueError(f"{what}: `blaze debug sql --json` printed a line that is no object: {line[:200]!r}")
        out.append(cast(dict[str, object], row))
    return out


def one_row(stdout: str, what: str) -> dict[str, object]:
    got = rows(stdout, what)
    if len(got) != 1:
        raise ValueError(f"{what}: {len(got)} rows, where the query gives one (the history has no build?)")
    return got[0]


def number(row: Mapping[str, object], column: str, what: str) -> float | None:
    """`column` of `row` as a number, None where it is null. A row without it, or with something
    else in it, is a ValueError: the profile's schema moved and the query has to follow it."""
    if column not in row:
        raise ValueError(f"{what}: the row has no column `{column}`: {sorted(row)}")
    value = row[column]
    if value is None:
        return None
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        raise ValueError(f"{what}: `{column}` is {value!r}, no number")
    return float(value)


def let_go_lines(log: str) -> int:
    """How many lines of the daemon's log say a kept front end was let go."""
    return sum(LET_GO_LINE in line for line in log.splitlines())


@dataclass(frozen=True)
class Known:
    """A bound known to fail: which, the number measured when it was recorded, and why."""

    fixture: Fixture
    step: Step
    check: str
    measured: float
    cause: str

    @property
    def name(self) -> str:
        return name(self.fixture, self.step, self.check)


def read_known(text: str) -> list[Known]:
    """The list harness/v2/guards-known.toml holds. An entry of no bound, a field missing or of
    another type, and the same bound twice are each a ValueError naming the entry."""
    data = tomllib.loads(text)
    entries: object = data.get("known", [])
    if set(data) - {"known", "recorded"} or not isinstance(entries, list):
        raise ValueError(f"guards-known.toml: its tables are [[known]] and [recorded], not {sorted(data)}")
    out: list[Known] = []
    wanted = set(names())
    for at, raw in enumerate(cast(list[object], entries)):
        where = f"guards-known.toml: [[known]] {at + 1}"
        if not isinstance(raw, dict):
            raise ValueError(f"{where}: not a table")
        entry = cast(dict[str, Any], raw)
        if set(entry) != {"fixture", "step", "check", "measured", "cause"}:
            raise ValueError(f"{where}: its fields are fixture, step, check, measured and cause, not {sorted(entry)}")
        if entry["fixture"] not in FIXTURES or entry["step"] not in STEPS or not isinstance(entry["check"], str):
            raise ValueError(f"{where}: no fixture {entry['fixture']!r} with a step {entry['step']!r}")
        measured: object = entry["measured"]
        if isinstance(measured, bool) or not isinstance(measured, (int, float)):
            raise ValueError(f"{where}: `measured` is a number, not {measured!r}")
        if not isinstance(entry["cause"], str) or not entry["cause"].strip():
            raise ValueError(f"{where}: `cause` says in a line what fails and why, and is empty")
        known = Known(entry["fixture"], entry["step"], entry["check"], float(measured), entry["cause"])
        if known.name not in wanted:
            raise ValueError(f"{where}: `{known.name}` is no bound of guards.py's table")
        if any(it.name == known.name for it in out):
            raise ValueError(f"{where}: `{known.name}` is listed twice")
        out.append(known)
    return out


def render_known(known: Sequence[Known], recorded: Mapping[str, str]) -> str:
    """guards-known.toml's text: the entries sorted by name, then what they were recorded from."""
    lines = [
        "# The budget smoke's bounds known to fail (harness/v2/guards.py; docs/harness.md, \"The smoke's",
        "# guards\"). Each entry is one bound on one step of one fixture, the number measured when it was",
        "# recorded, and the cause in a line. A listed bound may fail by no more than `measured` (a time by",
        "# half as much again, for noise); one listed that passes fails the run until it is taken out; one",
        "# not listed that fails fails the run. The list only shrinks. Written by:",
        "#   uv run --frozen python harness/v2/budgets.py '{\"command\": \"smoke\", \"record\": true, \"engine\": {...}}'",
        "",
    ]
    for it in sorted(known, key=lambda it: it.name):
        measured = int(it.measured) if it.measured == int(it.measured) else round(it.measured, 3)
        lines += ["[[known]]", f"fixture = {json.dumps(it.fixture)}", f"step = {json.dumps(it.step)}",
                  f"check = {json.dumps(it.check)}", f"measured = {measured}", f"cause = {json.dumps(it.cause)}", ""]
    lines.append("[recorded]")
    lines += [f"{key} = {json.dumps(value)}" for key, value in sorted(recorded.items())]
    return "\n".join(lines) + "\n"


@dataclass(frozen=True)
class Judged:
    """One bound held to one step's facts: the outcome's name, its verdict and what to say."""

    name: str
    verdict: Verdict
    detail: str
    measured: float | None
    limit: float
    unit: str
    known: bool = False


def judge(fixture: Fixture, step: Step, facts: Mapping[str, float | None], known: Sequence[Known]) -> list[Judged]:
    """Every bound of `step` held to `facts` (a number a check, None where the profile gave
    none), with the known failures of the list allowed as far as they were recorded."""
    out: list[Judged] = []
    listed = {it.name: it for it in known}
    for bound in BOUNDS:
        limit = bound.limit(step)
        if limit is None:
            continue
        at = name(fixture, step, bound.check)
        was = listed.get(at)
        value = facts.get(bound.check)
        tail = f" ({bound.what}; it catches: {bound.catches})"
        if value is None:
            if bound.needs_task and facts.get("tasks") == 0:
                verdict, detail = "pass", "the build ran no task, so nothing was held back after one"
                if was is not None:
                    verdict, detail = "fail", detail + f"; it is listed as a known failure ({was.cause}): take it out"
                out.append(Judged(at, cast(Verdict, verdict), detail, None, limit, bound.unit))
            else:
                out.append(Judged(at, "unmeasured", "the profile gave no number for it" + tail, None, limit, bound.unit))
            continue
        said = f"{value:g} {bound.unit} against at most {limit:g}"
        if value <= limit:
            if was is None:
                out.append(Judged(at, "pass", said, value, limit, bound.unit))
            else:
                out.append(Judged(at, "fail", f"{said}: it passes and is listed as a known failure ({was.cause}); take it "
                                  "out of harness/v2/guards-known.toml, the list only shrinks", value, limit, bound.unit, True))
        elif was is None:
            out.append(Judged(at, "fail", said + tail, value, limit, bound.unit))
        elif value <= was.measured * SLACK.get(bound.unit, 1.0):
            out.append(Judged(at, "pass", f"{said}: a known failure, recorded at {was.measured:g} ({was.cause})", value,
                              limit, bound.unit, True))
        else:
            out.append(Judged(at, "fail", f"{said}: a known failure that got worse than the {was.measured:g} recorded "
                              f"({was.cause})" + tail, value, limit, bound.unit, True))
    return out


def recorded_from(judged: Sequence[tuple[Fixture, Step, Judged]], before: Sequence[Known]) -> list[Known]:
    """The known list a run gives: each bound that is over its limit, with the number measured,
    keeping the cause of an entry that was there."""
    causes = {it.name: it.cause for it in before}
    by_check = {bound.check: bound for bound in BOUNDS}
    out: list[Known] = []
    for fixture, step, it in judged:
        if it.measured is None or it.measured <= it.limit:
            continue
        check = it.name.rsplit(": ", 1)[1]
        cause = causes.get(it.name) or f"not yet said; it catches: {by_check[check].catches}"
        out.append(Known(fixture, step, check, it.measured, cause))
    return out


def held(result: object) -> list[str]:
    """What is wrong with a smoke's result (report.py's JSON) as the gate reads it: an outcome
    of `names()` that is missing, there twice, or not a pass. Empty when the guards ran and held."""
    if not isinstance(result, dict):
        return ["the result is no JSON object"]
    outcomes: object = cast(dict[str, object], result).get("outcomes")
    if not isinstance(outcomes, list):
        return ["the result has no `outcomes`"]
    seen: dict[str, list[str]] = {}
    for raw in cast(list[object], outcomes):
        if isinstance(raw, dict):
            outcome = cast(dict[str, object], raw)
            seen.setdefault(str(outcome.get("name")), []).append(str(outcome.get("verdict")))
    problems: list[str] = []
    for wanted in names():
        got = seen.get(wanted, [])
        if not got:
            problems.append(f"`{wanted}` did not run: the smoke gave no outcome for it")
        elif len(got) > 1:
            problems.append(f"`{wanted}` has {len(got)} outcomes")
        elif got[0] != "pass":
            problems.append(f"`{wanted}`: {got[0]}")
    return problems


# The fixtures and their edits.

ONEFILE_PROGRAM = "guard_onefile"
ONEFILE_FILE, ONEFILE_OPENING = "src/main.rs", "fn double(x: u32) -> u32 {"
ONEFILE = {
    "Cargo.toml": f'[package]\nname = "{ONEFILE_PROGRAM}"\nversion = "0.1.0"\nedition = "2021"\n\n[workspace]\n',
    ONEFILE_FILE: f'{ONEFILE_OPENING}\n    x.wrapping_mul(2)\n}}\n\nfn main() {{\n    println!("{{}}", double(21));\n}}\n',
}
PROBE_PROGRAM = "guard_probe"
PROBE_SAYS = "probed: true"
# As anyhow's build script asks: the version, then a small crate checked for its metadata.
PROBE_BUILD_RS = r'''use std::{env, fs, path::Path, process::{Command, Stdio}};

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rustc-check-cfg=cfg(probed)");
    let rustc = env::var_os("RUSTC").expect("the build script is given RUSTC");
    let version = Command::new(&rustc).arg("--version").output().expect("RUSTC starts");
    assert!(version.status.success(), "`$RUSTC --version` failed: {}", String::from_utf8_lossy(&version.stderr));
    let out = env::var_os("OUT_DIR").expect("the build script is given OUT_DIR");
    let dir = Path::new(&out).join("probe");
    fs::create_dir_all(&dir).expect("the probe's directory is made");
    let file = Path::new(&out).join("probe.rs");
    fs::write(&file, "pub fn probe() -> u32 { 1 }\n").expect("the probe is written");
    let mut probe = Command::new(&rustc);
    probe.stderr(Stdio::inherit()).arg("--edition=2021").arg("--crate-name=guard_probe_build").arg("--crate-type=lib")
        .arg("--emit=dep-info,metadata").arg("--cap-lints=allow").arg("--out-dir").arg(&dir).arg(&file);
    if let Some(target) = env::var_os("TARGET") {
        probe.arg("--target").arg(target);
    }
    let status = probe.status().expect("RUSTC starts for the probe");
    assert!(status.success(), "the probe did not compile: {status}");
    println!("cargo:rustc-cfg=probed");
}
'''
PROBE_FILES = {
    "Cargo.toml": f'[package]\nname = "{PROBE_PROGRAM}"\nversion = "0.1.0"\nedition = "2021"\nbuild = "build.rs"\n\n[workspace]\n',
    "build.rs": PROBE_BUILD_RS,
    "src/main.rs": 'fn main() {\n    println!("probed: {}", cfg!(probed));\n}\n',
}


def write_files(into: Path, files: Mapping[str, str]) -> None:
    for relative, text in files.items():
        path = into / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text)


def onefile_source(into: Path) -> tuple[dict[str, Any], Path]:
    """The one-file program written to `into`, and the bench.py spec a budgets.py `Lane` reads."""
    write_files(into, ONEFILE)
    return {"edits": {}, "build": [], "run": []}, into


def probe_source(into: Path) -> tuple[dict[str, Any], Path]:
    """A program whose build script runs `$RUSTC` as anyhow's does (its version, then a probe
    crate compiled for its metadata), so blaze runs inside its own build-script sandbox."""
    write_files(into, PROBE_FILES)
    return {"edits": {}, "build": [], "run": []}, into


def opening_line(text: str, opening: str) -> tuple[int, int]:
    """Where the one line `opening` is in `text`, and where it ends."""
    start = text.find(opening)
    if start < 0 or text.find(opening, start + 1) >= 0:
        raise ValueError(f"the text has {text.count(opening)} of {opening!r}, where an edit needs one")
    end = text.find("\n", start)
    if end < 0 or text[start + len(opening):end].strip():
        raise ValueError(f"{opening!r} does not end its line")
    return start, end


def eol_comment(text: str, opening: str, n: int = 0) -> str:
    """A comment at the end of the line `opening` is: no line moves and no token changes."""
    _, end = opening_line(text, opening)
    return text[:end] + f" // blaze guards: comment {n}" + text[end:]


def body_statement(text: str, opening: str, n: int = 0) -> str:
    """A statement as the first line of the body `opening` opens."""
    _, end = opening_line(text, opening)
    return text[:end + 1] + f"    let _blaze_guard: u32 = {n};\n" + text[end + 1:]


def table() -> str:
    """`BOUNDS` as a Markdown table (docs/harness.md's)."""

    def cell(limit: float | None) -> str:
        return "" if limit is None else f"{limit:g}"

    lines = ["| Check | Unit | No-op | Comment | Body | What is counted | The finding it catches |", "|---|---|---|---|---|---|---|"]
    lines += [f"| `{b.check}` | {b.unit} | {cell(b.noop)} | {cell(b.comment)} | {cell(b.body)} | {b.what} | {b.catches} |"
              for b in BOUNDS]
    return "\n".join(lines)


@dataclass(frozen=True)
class Input:
    command: Literal["held", "table"]
    result: str = ""


def main(args: Input) -> None:
    if args.command == "table":
        print(table())
        return
    if not args.result:
        fail(SCRIPT, "result: command `held` reads the smoke's result there, and was given none")
    path = Path(args.result)
    if not path.is_file():
        print(f"{SCRIPT}: the budget smoke did not run: there is no result at {path}. A gate run that did not run "
              "it is red (docs/harness.md, \"The smoke's guards\")")
        sys.exit(1)
    try:
        problems = held(json.loads(path.read_text()))
    except json.JSONDecodeError as e:
        problems = [f"the result at {path} is no JSON: {e}"]
    for problem in problems:
        print(f"{SCRIPT}: {problem}")
    print(f"{SCRIPT}: {len(names()) - len(problems)} of {len(names())} guards ran and held")
    sys.exit(1 if problems else 0)


# The self-test: no blaze. Each finding in miniature, as the facts its build gave.

def expect(holds: bool, what: str) -> None:
    if not holds:
        sys.exit(f"{SCRIPT} selftest: {what}")


def refused(what: str, text: str) -> None:
    try:
        read_known(text)
    except (ValueError, tomllib.TOMLDecodeError):
        return
    expect(False, f"{what} is refused")


# A warm build of a fixture as it should be: nothing dropped, nothing run that need not be.
FIXED: dict[Step, dict[str, float | None]] = {
    "noop": dict(ms=40, let_go=0, let_go_logged=0, ran_new=0, ran_untracked=0, executed=0, lower_ms=0, maintain_ms=0,
                 answer_delay_ms=None, turn_uncovered_ms=2, uncovered_ms=0, tasks=0),
    "comment": dict(ms=80, let_go=0, let_go_logged=0, ran_new=0, ran_untracked=0, executed=0, lower_ms=0,
                    maintain_ms=0, answer_delay_ms=1.5, turn_uncovered_ms=3, uncovered_ms=10, tasks=2),
    "body": dict(ms=300, let_go=0, let_go_logged=0, ran_new=20, ran_untracked=0, executed=90, lower_ms=40,
                 maintain_ms=30, answer_delay_ms=4, turn_uncovered_ms=8, uncovered_ms=10, tasks=6),
}


def failing(step: Step, known: Sequence[Known] = (), **facts: float | None) -> set[str]:
    """The checks that fail (or are unmeasured) on `step` with `facts` in place of the fixed ones."""
    judged = judge("onefile", step, {**FIXED[step], **facts}, known)
    return {it.name.rsplit(": ", 1)[1] for it in judged if it.verdict != "pass"}


def selftest() -> None:
    for step in STEPS:
        expect(not failing(step), f"the fixed {step} passes every bound: {failing(step)}")
    # Finding 1: the kept front end let go on every build, so a one-line edit lowered anew (25 s).
    dropped = failing("body", ms=25_000, let_go=1, let_go_logged=1, ran_new=41_000, lower_ms=24_000)
    expect(dropped == {"ms", "let_go", "let_go_logged", "ran_new"}, f"a dropped front end on a body edit: {dropped}")
    expect("let_go_logged" in failing("noop", let_go_logged=1) and "let_go" in failing("comment", let_go=2),
           "one front end let go on a warm build fails it, by the log and by the span")
    expect(let_go_lines("blaze serve: the kept front end of /w is let go, and its next build lowers anew: a task waits "
                        "for 0.03 GiB the process holds besides its tasks\nblaze serve: ready\n") == 1,
           "the daemon's line is counted")
    # Finding 2: the answer held 4 s for a sync; a loop turn of 0.9 s with no span.
    expect(failing("body", ms=4300, answer_delay_ms=4000) == {"ms", "answer_delay_ms"}, "an answer held for a sync")
    expect(failing("comment", answer_delay_ms=4000) == {"answer_delay_ms"}, "the delay alone fails, whatever the wall")
    expect(failing("body", turn_uncovered_ms=900) == {"turn_uncovered_ms"}, "a loop turn with 0.9 s no span covers")
    expect(not failing("noop", answer_delay_ms=None, tasks=0), "a build with no task has no delay to hold")
    expect(failing("comment", answer_delay_ms=None, tasks=3) == {"answer_delay_ms"},
           "a build with tasks and no reply span is unmeasured, never passed")
    # Finding 4: an untracked query run 10,000 times a build; 200 ms lowering and 190 ms maintaining nothing.
    expect(failing("noop", ran_untracked=10_000, executed=10_000) == {"ran_untracked", "executed"}, "untracked reruns")
    expect(failing("comment", lower_ms=200, maintain_ms=190) == {"lower_ms", "maintain_ms"}, "a comment edit's lowering")
    expect(not failing("body", lower_ms=200, ran_untracked=5), "a body edit's lowering is not held (yet)")
    expect("executed" in failing("comment", executed=None), "a fact the profile did not give is unmeasured")
    # The known list: a failure recorded passes as far as recorded, no further, and not once fixed.
    known = [Known("onefile", "noop", "ran_untracked", 10_000, "identity_of is untracked"),
             Known("onefile", "comment", "lower_ms", 200, "lowering is not incremental")]
    expect(not failing("noop", known, ran_untracked=10_000, executed=0), "a known failure at its number passes")
    expect(failing("noop", known, ran_untracked=10_001) == {"ran_untracked"}, "a count that got worse fails")
    expect(not failing("comment", known, lower_ms=290), "a known time has half again for noise")
    expect(failing("comment", known, lower_ms=301) == {"lower_ms"}, "and fails past it")
    expect(failing("noop", known) == {"ran_untracked"}, "a known failure that passes fails until it is taken out")
    judged: list[tuple[Fixture, Step, Judged]] = [
        ("onefile", step, it) for step in STEPS
        for it in judge("onefile", step, {**FIXED[step], **({"ran_untracked": 9000.0} if step == "noop" else {})}, known)]
    again = recorded_from(judged, known)
    expect([(it.name, it.measured, it.cause) for it in again]
           == [("guard: onefile: noop: ran_untracked", 9000.0, "identity_of is untracked")],
           f"a record keeps what fails with its new number and its cause, and drops what passes: {again}")
    text = render_known(again, {"tree": "abc"})
    expect(read_known(text) == again, "the list reads back as it was written")
    refused("an entry of no bound", text.replace('"ran_untracked"', '"ran_sideways"'))
    refused("an entry twice", text.replace("[recorded]", text.split("[recorded]")[0].split("\n\n", 1)[1] + "[recorded]"))
    refused("an entry with no cause", text.replace('"identity_of is untracked"', '""'))
    listed = [it.name for it in read_known(KNOWN.read_text())]
    expect(listed == sorted(listed), "harness/v2/guards-known.toml reads, its entries sorted as `render_known` writes them")
    # Nothing asked for is dropped.
    wanted = names()
    expect(len(wanted) == len(set(wanted)) and PROBE in wanted, "each outcome is named once")
    expect(len(wanted) == 2 * sum(b.limit(step) is not None for b in BOUNDS for step in STEPS) + 1,
           "every bound of every step of both fixtures is an outcome")
    full: dict[str, object] = {"outcomes": [{"name": it, "verdict": "pass"} for it in wanted]}
    expect(held(full) == [], "a result with every guard passed is held")
    expect(len(held({"outcomes": [{"name": it, "verdict": "pass"} for it in wanted[1:]]})) == 1
           and "did not run" in held({"outcomes": []})[0], "a guard with no outcome is named: it did not run")
    expect(len(held({"outcomes": [{"name": wanted[0], "verdict": "fail"}, *cast(list[object], full["outcomes"])[1:]]})) == 1,
           "a guard that failed is named")
    expect(held([]) != [] and held({}) != [], "a result of another shape is refused")
    # The edits apply to the fixtures, and move no line where they say so.
    source = ONEFILE[ONEFILE_FILE]
    commented = eol_comment(source, ONEFILE_OPENING, 3)
    expect(commented.count("\n") == source.count("\n") and commented != eol_comment(source, ONEFILE_OPENING, 4)
           and commented.splitlines()[0] == ONEFILE_OPENING + " // blaze guards: comment 3", "the comment ends the line")
    stated = body_statement(source, ONEFILE_OPENING, 7)
    expect(stated.splitlines()[1] == "    let _blaze_guard: u32 = 7;" and stated.count("\n") == source.count("\n") + 1,
           "the statement opens the body")
    try:
        eol_comment(source + source, ONEFILE_OPENING)
        expect(False, "an opening that is there twice is refused")
    except ValueError:
        pass
    expect("@" not in facts_sql(12) and ">= 12\n" in facts_sql(12), "the mark is written into the query")
    sql_columns = {line.split(" AS ")[-1].strip(" ,") for line in FACTS_SQL.split("SELECT w.build_id")[1].split("FROM")[0]
                   .replace("\n", " ").split(",")}
    expect(set(PROFILE_FACTS) <= sql_columns | {"build_id"}, f"the query gives every fact read from it: {sql_columns}")
    expect({b.check for b in BOUNDS} <= set(PROFILE_FACTS) | {"ms", "let_go_logged", "uncovered_ms"},
           "every bound is of a fact the smoke gathers")
    one = one_row('{"build_id":3,"executed":0,"answer_delay_ms":null}\n', "a row")
    expect(number(one, "executed", "a row") == 0.0 and number(one, "answer_delay_ms", "a row") is None, "a row's numbers")
    for bad in ('{"executed":"many"}', '{"other":1}'):
        try:
            number(one_row(bad, "a row"), "executed", "a row")
            expect(False, f"{bad} is refused")
        except ValueError:
            pass
    expect(table().count("\n") == len(BOUNDS) + 1, "the table has a line a bound")
    print(f"{SCRIPT} selftest: ok")


if __name__ == "__main__":
    if len(sys.argv) != 2:
        sys.exit(f"usage: {SCRIPT} '<json>' | selftest\n\n{__doc__}")
    if sys.argv[1] == "selftest":
        selftest()
    else:
        main(parse(Input, sys.argv[1], SCRIPT))
