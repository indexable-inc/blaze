"""budgets.py: the performance budgets of docs/v2/plan/objectives.md, and docs/v2/plan.md's release
code speed against rustc's, each an outcome with the number measured, the budget, today's figure from the
table, and pass or fail.

    uv run --frozen python harness/v2/budgets.py '{"engine": {"kind": "v2", "blaze": "target/release/blaze"}, "out": "/tmp/budgets.json"}'
    uv run --frozen python harness/v2/budgets.py '{"engine": {...}, "out": "...", "budgets": ["body", "noop"], "runs": 3}'
    uv run --frozen python harness/v2/budgets.py '{"command": "smoke", "engine": {"kind": "v2", "blaze": "target/release/blaze"}}'
    uv run --frozen python harness/v2/budgets.py selftest

The input is one JSON object (inline, a file's path, or `-`):

    command         `run` (the budgets, the default) or `smoke` (below)
    engine          the blaze measured (engine.EngineSpec)
    out             the result's path (report.py's JSON); `run` needs one
    work            where checkouts and each one's daemon home go. Empty: a new directory under
                    /tmp, or where /tmp cannot clone files (tmpfs) under `blaze-harness` in the
                    user's cache directory, removed at the end when nothing is kept. Either way
                    it is probed before any build, and one on a file system blaze cannot place
                    outputs on (no FICLONE or clonefile: tmpfs, ext4) stops the run naming it
                    (engine.work_dir). Everything a daemon keeps is under its home
                    (engine.layout): there is no store directory to name
    projects        whose rows run, of nushell, ripgrep, ruff and blaze (this checkout); empty
                    for every project the budgets asked for have a row for
    budgets         which budgets run (the names below); empty for every budget the projects
                    asked for have a row for. Nothing asked for is dropped: a budget named here
                    with no row for any project named in `projects`, or a project named there
                    with no row for any budget named here, stops the run before any build,
                    naming it and whose rows it has (`select`). `bounds` is no project's
    runs            the fewest times an edit, a no-op build, a warm start, a restart or a bound's
                    program is timed: the median is the number, with its 95% interval from 6
                    runs on (10, hyperfine's least). Runs go on until the interval is at most
                    10% of the budget wide or leaves the budget out (`enough`), so "within 10%
                    of the bound" is a measured answer and not noise; each outcome says how many
                    runs it took and how many its spread needs (`runs_for`)
    most_runs       the most times one is timed, settled or not (60)
    runtime_runs    the fewest times each program runs for the code-speed budgets, after two runs
                    that warm the page cache (10); more until each side's median is known to 5%
    slack           the signature budget: at most `slack` functions compiled per place the edit
                    changed (4.0)
    noise           how far over 1.0 a code-speed ratio may be and still pass (0.05)
    critical_slack  the cold build of blaze takes at most this many times its critical path (1.25)
    restart_slack_ms  the restart budget: the first answer after a restart at most this many ms
                    over the warm answer, beyond the Linux statx sweep (5.0)
    activation      the restart budget with the daemon socket-activated: `blaze daemon install`
                    in the harness's environment before it and `blaze daemon uninstall` after,
                    which replaces the user's own units (false: the command line starts it)
    self_edit       the body edit of the CI budget: `file` of this checkout and `opening`, the
                    line that opens a function, after which a statement is added
    self_test       the arguments of `blaze test` in the CI budget (none: the workspace's default)
    keep            keep each project's directory under `work` (false)
    record          `smoke`: write harness/v2/guards-known.toml again from this run's guards
                    instead of holding the run to it (guards.py; false)
    duckdb          `smoke`: the DuckDB `blaze debug sql` runs (`BLAZE_DUCKDB`), as an absolute
                    path; empty for blaze's pin, which its first query fetches

Numbers are JSON numbers; a float field takes one with a point (4.0, not 4).

The budgets, each with the project it is measured on (projects come from bench.PROJECTS, at
their pinned revisions, except blaze, which is the files of this checkout as they are now):

    body           nushell: bench's `body` edit, `blaze build` until it returns: at most 30 ms
    signature      nushell: bench's `signature` edit: functions compiled at most `slack` times
                   the places it changed (bench.write_edit's count); the time is reported
    format         nushell: blank lines added in the function bench's `body` edit opens, and
                   its body indented further: nothing compiled
    comment        nushell: a comment line added at the top of that function's body, and an
                   inline comment after its `{` that moves no line: nothing compiled
    rename, move   nushell: a private function and a call of it are added (not timed); then it
                   is renamed with its call, or moved next to the function that calls it:
                   nothing compiled
    memory         nushell and ripgrep: the most blaze's processes hold at once over its cold
                   build (memory.Sampler over everything below this script, from the first
                   `blaze build`, which starts the daemon, until the engine says it is ready)
                   at most cargo's cold build's (memory.run)
    warm-start     nushell: the daemon stopped, then a build, until the engine says it is ready
                   again on the warm store: under 1 s
    noop           ripgrep: `blaze build` with nothing changed: under 5 ms
    dev-speed      ripgrep: bench's `runtime` workload, blaze's dev program against cargo's debug
                   build: the ratio of their medians at most 1.0 + noise
    release-speed  ripgrep: the same, `blaze build --release` against `cargo build --release`
    big-crate      ruff: bench's `body` edit: at most 50 ms
    cold-blaze     blaze: a cold `blaze build` (empty home, store and target directory) until
                   ready: the share of the machine's cores busy over it (/proc/stat) at least
                   0.8, and its wall time at most `critical_slack` times its critical path
                   (engine.critical_path_s)
    ci             blaze, warm after the cold build and one `blaze test`: `self_edit`, then
                   `blaze build` and `blaze test --reached` (only the tests the edit can have
                   changed run; docs/cargo.md, "Which tests run"): under 120 s, every test
                   passing
    restart        blaze (the corpus's biggest workspace), warm after its cold build: the daemon
                   stopped cleanly (`blaze stop`), or its process group sent SIGKILL, then the
                   time from a `blaze check` with nothing changed to its result: at most the
                   median warm no-op check plus `restart_slack_ms`, plus on Linux the statx
                   sweep of the workspace's files at 0.5 us a file (docs/v2/plan/design/restore.md:
                   about 50 ms for 100k); each run's warm check is timed just before its restart
    memory         (also) nushell: the cold peak per thousand lines of the workspace's .rs files,
                   at most cargo's
    layout         nushell after its cold build: bytes per element of each table
                   docs/v2/plan/design/data.md sizes (Engine.tables): a token 4, a syntax node 8,
                   a HIR expression 12, a MIR statement 16, a memo 24 and an edge 4, a
                   relocation 16
    cache-misses   nushell: the machine's cache misses over the build of bench's `body` edit
                   (`perf stat -a`, Linux); docs/v2/plan/design/data.md asks for it and sets no budget
    bounds         every speed-of-light bound (bounds.py: the `bound` blocks of
                   docs/v2/plan/bounds.md), each program beside `blaze` run
                   until settled: at most the bound x 1.1 (at least the bound / 1.1 for a rate)

The timed budgets (body, big-crate, noop) also count the instructions and cycles the daemon's
processes retired over each timed build, from quiet to quiet (counters.py: Linux's counters in
user mode on every thread of the daemon and what it starts, macOS's totals; the `blaze` command
itself is not counted here): `instructions` and `cycles` are their medians, `instructions_samples`
every sample, and where they cannot be counted `counts_gap` says why. warm-start's daemon is new
each run, so it is not counted.

Each project's checkout has a daemon home of its own, which holds its store and its build
directory (engine.layout; blaze writes no `target` into the checkout), and a cargo target
directory outside the checkout (engine.reference_env). A build blaze did not serve (`Engine.not_blazes`)
or that fails is a fail. The program after the first of every edit's runs, and after a warm
start, has to print what blaze's own build of the same source from nothing prints (a home
and a target directory of its own beside the checkout, removed after it;
blaze_cli.program_output, each build directory's path read as the same): a fast
wrong build passes nothing. cargo is the baseline of the speed and memory rows alone, and in
`dev-speed` and `release-speed` blaze's program has to exit 0. Every edit is varied from run to run (bench.py's statement and
parameter with another value, blank lines and indentation by more, comments with another text,
another name), so no run is a store's hit for the one before it.

`smoke` is what keeps this script able to run against the blaze of the tree: the merge gate
runs it with the blaze the change built (blaze-platform.yaml, `profile-gate`). After the
engine's preflight (engine.V2.preflight) it writes fixture.py's workspace (three small crates),
builds it cold in a new home, and measures one no-op build and one body edit through the code
the budgets run (`Lane.noop`, `Lane.timed_edit`, with the edited program checked against a build
from nothing in a home of its own), once each and against no budget. Then it holds the guards
of guards.py: on that workspace and on a one-file program it times a warm no-op build, a comment
at the end of a line and a statement added to a body, and holds each build's time, and what the
daemon's profile and log say it did (a kept front end let go, queries run anew or for an
untracked read, time lowering or maintaining what did not change, an answer held after the last
task, a loop turn no span covers), to guards.py's one table of bounds, with the bounds known to
fail listed in harness/v2/guards-known.toml (`record` writes it); and it builds and runs a
program whose build script runs `$RUSTC` as anyhow's does, within `PROBE_SECS`, so blaze hanging
or failing inside its own build-script sandbox is a failure naming that step. It exits 0 only when
the two rows gave a number, the daemon's lock and store are where engine.layout says, and every
outcome of `guards.names()` is there and passed: nothing asked for is dropped. It needs no cargo,
and no network but for blaze's own toolchain and, with no `duckdb` given, blaze's pinned DuckDB. It took 64 s with the release of 2026-11-05 on 32
cores, and says where (`steps_s` in the result, a line per `blaze` command): it builds the
fixture cold twice, once in the lane's home and once from nothing for the check of the edited
program.

`selftest` checks the parts that need neither blaze nor cargo nor the network: edit generation,
budget evaluation, /proc/stat, which rows an input selects, the daemon home's layout against the
daemon's rule, and that the smoke's edit applies to the fixture. The guards' own selftest is
guards.py's.
"""

import bisect
import contextlib
import dataclasses
import io
import json
import math
import os
import re
import shutil
import signal
import statistics
import subprocess
import sys
import tempfile
import time
from collections.abc import Callable, Iterable, Sequence
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Literal, Protocol, cast, get_args

HERE = Path(__file__).resolve().parent
sys.path[:0] = [str(HERE), str(HERE.parent)]
from typed_input import fail, parse
import engine
import report
import bench
import bounds
import blaze_cli
import counters
import fixture
import guards
import memory
import storelib

SCRIPT = "budgets.py"
Budget = Literal[
    "body", "signature", "format", "comment", "rename", "move", "memory", "warm-start", "noop", "dev-speed",
    "release-speed", "big-crate", "cold-blaze", "ci", "restart", "layout", "cache-misses", "bounds",
]
# Not a `type` alias: `get_args` of one is empty, and `own` runs a restart row for each of these.
Restart = Literal["clean", "kill9"]
Project = Literal["nushell", "ripgrep", "ruff", "blaze"]
# A row is a project's, or the smoke's, which is of fixture.py's workspace.
type Where = Project | Literal["fixture"]
type Command = Literal["run", "smoke"]
type Change = tuple[str, str | None, str]
COLD_BLAZE_SECS = 4 * 3600
RUN_SECS = 600
# docs/v2/plan/design/restore.md: Linux's one batched statx sweep, about 50 ms for 100k files.
STATX_US_PER_FILE = 0.5
GONE_SECS = 60.0

PROJECTS: dict[str, dict[str, Any]] = getattr(bench, "PROJECTS")
LET: str = getattr(bench, "LET")
write_edit: Callable[[str, list[Change], bool], int] = getattr(bench, "write_edit")
library: Callable[[], str] = getattr(bench, "library")
fetch: Callable[[str, dict[str, Any]], str] = getattr(blaze_cli, "fetch")
program_output: Callable[[object], tuple[int, str, str]] = getattr(blaze_cli, "program_output")
difference: Callable[[object, object], str | None] = getattr(blaze_cli, "difference")
memory_run: Callable[..., tuple[float, subprocess.CompletedProcess[str], dict[str, int | None]]] = getattr(
    memory, "run")


class Sampling(Protocol):
    """memory.Sampler, as this script uses it."""

    @property
    def peak_mb(self) -> int: ...

    def add(self, *pids: int, itself: bool = True) -> "Sampling": ...

    def start(self) -> "Sampling": ...

    def stop(self) -> "Sampling": ...


Sampler: Callable[[], Sampling] = getattr(memory, "Sampler")


@dataclass(frozen=True)
class SelfEdit:
    file: str = "crates/blaze-cas/src/lib.rs"
    opening: str = "fn location(tier: usize, pack: u32, offset: u64) -> u64 {"


@dataclass(frozen=True)
class Input:
    engine: engine.EngineSpec
    out: str = ""
    command: Command = "run"
    work: str = ""
    projects: tuple[Project, ...] = ()
    budgets: tuple[Budget, ...] = ()
    runs: int = 10
    most_runs: int = 60
    runtime_runs: int = 10
    slack: float = 4.0
    noise: float = 0.05
    critical_slack: float = 1.25
    restart_slack_ms: float = 5.0
    activation: bool = False
    self_edit: SelfEdit = SelfEdit()
    self_test: tuple[str, ...] = ()
    keep: bool = False
    record: bool = False
    duckdb: str = ""


@dataclass(frozen=True)
class Row:
    """One budget measured on one project. `limit` is the budget unless the measurement sets it
    (what cargo held, `slack` times the places changed, 1.0 + noise)."""

    key: str
    budget: Budget
    project: Where
    name: str
    limit: float
    at_most: bool
    unit: str
    today: str


FORMATTING = "recompiles the file's functions' places"
COLD_BLAZE = "~12 min of mostly serial codegen"
ROWS = (
    Row("body", "body", "nushell", "body edit, edit to result (nushell)", 30, True, "ms", "~0.29 s (nushell)"),
    Row("signature", "signature", "nushell", "signature edit (nushell): functions compiled for the places changed", 0,
        True, "functions", "~0.31 s, 6,269 roots asked"),
    Row("format-blank", "format", "nushell", "formatting edit (nushell): blank lines", 0, True, "functions", FORMATTING),
    Row("format-indent", "format", "nushell", "formatting edit (nushell): re-indentation", 0, True, "functions",
        FORMATTING),
    Row("comment-line", "comment", "nushell", "comment edit (nushell): a comment line", 0, True, "functions",
        FORMATTING),
    Row("comment-inline", "comment", "nushell", "comment edit (nushell): an inline comment that moves no line", 0,
        True, "functions", FORMATTING),
    Row("rename", "rename", "nushell", "rename of an item (nushell)", 0, True, "functions", "recompiles dependents"),
    Row("move", "move", "nushell", "move of an item (nushell)", 0, True, "functions", "recompiles dependents"),
    Row("memory-nushell", "memory", "nushell", "memory: nushell's cold build at most cargo's peak", 0, True, "MB",
        "nushell 12.5 GB (#734)"),
    Row("warm-start", "warm-start", "nushell", "daemon start, warm (nushell)", 1000, True, "ms",
        "35 s (nushell); up to 238 s in CI"),
    Row("noop", "noop", "ripgrep", "no-op build (ripgrep)", 5, True, "ms", "11 ms (ripgrep)"),
    Row("memory-ripgrep", "memory", "ripgrep", "memory: ripgrep's cold build at most cargo's peak", 0, True, "MB",
        "cold ripgrep 2.0 GB live vs cargo's 0.56 GB (#806)"),
    Row("dev-speed", "dev-speed", "ripgrep", "dev-build code speed (ripgrep): blaze's dev program against cargo's debug "
        "build", 1.0, True, "x cargo's time", "ripgrep's dev build runs 5.8x slower than cargo's debug build (#1404)"),
    Row("release-speed", "release-speed", "ripgrep", "release code speed (ripgrep): `blaze build --release` against "
        "`cargo build --release`", 1.0, True, "x cargo's time", "no figure in docs/v2/plan/objectives.md"),
    Row("big-crate", "big-crate", "ruff", "body edit of a big top crate (ruff)", 50, True, "ms",
        "ruff 75 s, ty 147 s; cargo 7 s; Astral's fork 0.3 s (#1450)"),
    Row("cold-busy", "cold-blaze", "blaze", "cold build of blaze: the share of the machine's cores busy", 0.8, False,
        "of all cores", COLD_BLAZE),
    Row("cold-critical", "cold-blaze", "blaze", "cold build of blaze: wall time against its critical path", 0, True,
        "x the critical path", COLD_BLAZE),
    Row("ci", "ci", "blaze", "blaze's own CI, small PR: a body edit, `blaze build` and the affected tests, warm", 120,
        True, "s", "30 min+"),
    Row("restart-first-answer-clean", "restart", "blaze", "restart to first answer after a clean stop (blaze): "
        "the warm answer plus a few ms", 0, True, "ms", "35 s (nushell); up to 238 s in CI"),
    Row("restart-first-answer-kill9", "restart", "blaze", "restart to first answer after kill -9 (blaze): "
        "the warm answer plus a few ms", 0, True, "ms", "35 s (nushell); up to 238 s in CI"),
    Row("memory-kloc", "memory", "nushell", "memory per thousand lines (nushell's own source): blaze's cold peak "
        "at most cargo's", 0, True, "MB per kloc", "nushell 12.5 GB (#734)"),
    *(Row(f"layout-{table}", "layout", "nushell", f"bytes per {what} (nushell)", size, True, "bytes", today)
      for table, what, size, today in (
          ("token", "token", 4, "a rowan token and its green node"),
          ("syntax-node", "syntax node", 8, "rowan green and red nodes"),
          ("hir-expr", "HIR expression", 12, "rust-analyzer's `Expr` enum"),
          ("mir-statement", "MIR statement", 16, "hir-ty's MIR statement enum"),
          ("memo", "memo, its edges apart", 24, "salsa's memos"),
          ("memo-edge", "memo edge", 4, "salsa's edges: 0.8 GB of nushell's 12.5 GB (#734)"),
          ("relocation", "relocation", 16, "the object file's relocations"),
      )),
    Row("cache-misses", "cache-misses", "nushell", "cache misses over a body edit's build (nushell, `perf stat -a`)",
        math.inf, True, "misses", "not measured before"),
)
# The smoke's two rows: measured once and held to no budget, so each passes when it gave a number.
SMOKE_ROWS = (
    Row("noop", "noop", "fixture", "smoke: no-op build (fixture.py's workspace)", math.inf, True, "ms",
        "no budget: the smoke asks only for a number"),
    Row("body", "body", "fixture", "smoke: body edit, edit to result (fixture.py's workspace)", math.inf, True, "ms",
        "no budget: the smoke asks only for a number"),
)
# The smoke's body edit, as bench.py writes one: a statement after the line that opens a function.
SMOKE_FILE, SMOKE_OPENING = "base/src/a.rs", "pub fn alpha() -> u32 {"
SMOKE_PROGRAM = "oracle_app"
# The most the build and the run of guards.py's probe may take: a build script that runs
# `$RUSTC` twice, on a program of one line.
PROBE_SECS = 300


def select(projects: Sequence[Project], budgets: Sequence[Budget]) -> list[Row]:
    """The rows an input asks for: those of `budgets` on `projects`, each empty for all. What
    was named and has no row is a ValueError naming it and where its rows are, never dropped:
    a budget named with no row on any project named, a project named with no row of any budget
    named. `bounds` has no row here (its rows are docs/v2/plan/bounds.md's, measured beside
    `blaze` on no project), so it needs none and gives a project none."""
    chosen = [row for row in ROWS if row.budget in (budgets or get_args(Budget))
              and row.project in (projects or get_args(Project))]
    wrong: list[str] = []
    for budget in dict.fromkeys(budgets):
        if budget != "bounds" and not any(row.budget == budget for row in chosen):
            its = ", ".join(dict.fromkeys(row.project for row in ROWS if row.budget == budget))
            wrong.append(f"the budget {budget!r} has no row for {', '.join(projects)}: its rows are {its}'s")
    for project in dict.fromkeys(projects):
        if not any(row.project == project for row in chosen):
            its = ", ".join(dict.fromkeys(row.budget for row in ROWS if row.project == project))
            wrong.append(f"the project {project!r} has no row for {', '.join(budgets)}: its budgets are {its}")
    if wrong:
        raise ValueError("; ".join(wrong))
    return chosen


# Section 17's sizes: what `Engine.tables` names each table, and the row it is judged by.
TABLES = {row.key: row.key.removeprefix("layout-") for row in ROWS if row.budget == "layout"}


class Wrong(Exception):
    """A finding about the compiler: a build that failed or was not blaze's, a warm program that
    prints something else than blaze's build of it from nothing, a test that failed."""


HARNESS_ERRORS = (AssertionError, OSError, ValueError, subprocess.SubprocessError)
SETUP_ERRORS = (Wrong, engine.Failed, engine.Unmeasurable, *HARNESS_ERRORS)


def failing[T](what: str, fn: Callable[[], T]) -> T:
    """`fn()`, with what went wrong in the harness (a clone, a file, a pin) as an engine.Failed."""
    try:
        return fn()
    except HARNESS_ERRORS as e:
        raise engine.Failed(f"{what}: {type(e).__name__}: {e}") from None


@dataclass(frozen=True)
class Measured:
    value: float
    limit: float | None = None
    detail: str = ""
    numbers: dict[str, report.Number] = field(default_factory=dict[str, report.Number])
    # Where `value`, a median of samples, lies with 95% confidence (`median_interval`).
    interval: tuple[float, float] | None = None


def verdict(value: float, limit: float, at_most: bool) -> report.Verdict:
    return "pass" if (value <= limit if at_most else value >= limit) else "fail"


def describe(value: float, limit: float, at_most: bool, unit: str) -> str:
    return f"{value:.4g} {unit} against a budget of {'at most' if at_most else 'at least'} {limit:.4g} {unit}"


def samples(xs: Iterable[float]) -> str:
    return " ".join(f"{x:.4g}" for x in xs)


def median_interval(xs: Sequence[float], level: float = 0.95) -> tuple[float, float] | None:
    """The distribution-free interval of the median of `xs` at `level`: the k-th smallest and
    k-th largest sample, k the largest with P(B < k) <= (1 - level) / 2 for B ~ Binomial(n, 1/2)
    (no assumption of normal timings, which builds are not). None when there are too few
    samples for any (5 or fewer at 95%)."""
    n, ordered = len(xs), sorted(xs)
    tail, below, k = (1 - level) / 2, 0.0, 0
    for j in range(n + 1):
        below += math.comb(n, j) / 2**n
        if below > tail:
            break
        k = j + 1
    return (ordered[k - 1], ordered[n - k]) if k else None


# "Within 10% of the bound" is measurable when the median's 95% interval is at most this share of
# the budget wide.
WIDTH = 0.1


def runs_for(cv: float, width: float = WIDTH) -> int:
    """The runs a median needs for its 95% interval to be at most `width` of it wide, for samples
    whose standard deviation is `cv` of their median: the median's standard error is sqrt(pi / 2)
    sigma / sqrt(n) for normal samples, so n = (2 x 1.96 x 1.2533 x cv / width)^2, about
    (49 cv)^2 at 10%; and never fewer than 6, the least that has a 95% interval at all."""
    return max(6, math.ceil((2 * 1.959964 * math.sqrt(math.pi / 2) * cv / width) ** 2))


def enough(xs: Sequence[float], limit: float, args: Input) -> bool:
    """Whether `xs` settle a budget of `limit`: `runs` of them at least, and the 95% interval of
    their median at most `WIDTH` of the limit wide or not taking the limit in; or `most_runs`
    of them, settled or not (the outcome then says `settled: false`)."""
    if len(xs) < args.runs:
        return False
    if len(xs) >= args.most_runs:
        return True
    if (interval := median_interval(xs)) is None:
        return False
    low, high = interval
    return high - low <= WIDTH * abs(limit) or not low <= limit <= high


def known(xs: Sequence[float], share: float, least: int, most: int) -> bool:
    """Whether the median of `xs` is known to `share` of itself (its interval that narrow), with
    `least` to `most` samples: for a ratio of two medians, each to half the width."""
    if len(xs) < least:
        return False
    if len(xs) >= most:
        return True
    interval = median_interval(xs)
    return interval is not None and interval[1] - interval[0] <= share * abs(statistics.median(xs))


def outliers(xs: Sequence[float]) -> int:
    """How many of `xs` are outliers by the modified Z-score (Iglewicz and Hoaglin: |0.6745 (x -
    median) / MAD| > 3.5), as hyperfine warns of them; with a MAD of 0, every sample off the
    median."""
    if not xs:
        return 0
    middle = statistics.median(xs)
    mad = statistics.median(abs(x - middle) for x in xs)
    if mad == 0:
        return sum(x != middle for x in xs)
    return sum(abs(0.6745 * (x - middle) / mad) > 3.5 for x in xs)


def spread(xs: Sequence[float], unit: str, prefix: str = "") -> dict[str, report.Number]:
    """hyperfine's summary of samples: mean, standard deviation, range, the outliers, and the
    95% interval of the median where there are samples enough for one."""
    out: dict[str, report.Number] = {
        f"{prefix}mean_{unit}": round(statistics.fmean(xs), 4),
        f"{prefix}stdev_{unit}": round(statistics.stdev(xs), 4) if len(xs) > 1 else 0.0,
        f"{prefix}min_{unit}": round(min(xs), 4),
        f"{prefix}max_{unit}": round(max(xs), 4),
        f"{prefix}outliers": outliers(xs),
    }
    if (interval := median_interval(xs)) is not None:
        out[f"{prefix}median_low_{unit}"], out[f"{prefix}median_high_{unit}"] = (round(v, 4) for v in interval)
    return out


def timed(xs: Sequence[float], unit: str, **numbers: report.Number) -> Measured:
    """The median of `xs` with its samples, their spread, its interval, how many runs it took
    and how many its spread needs for an interval `WIDTH` of it wide (`runs_for`)."""
    middle = statistics.median(xs)
    cv = statistics.stdev(xs) / abs(middle) if len(xs) > 1 and middle else 0.0
    return Measured(middle, numbers=dict(numbers, **{f"samples_{unit}": samples(xs)}, **spread(xs, unit), runs=len(xs),
                                         runs_needed=runs_for(cv)), interval=median_interval(xs))


def judge(rep: report.Report, row: Row, measure: Callable[[], Measured]) -> None:
    """`measure()` against `row`'s budget, as an outcome of its own."""

    def go() -> None:
        try:
            got = measure()
        except Wrong as e:
            rep.add(row.name, "fail", str(e), today=row.today)
            return
        except HARNESS_ERRORS as e:
            rep.add(row.name, "error", f"{type(e).__name__}: {e}", today=row.today)
            return
        limit = row.limit if got.limit is None else got.limit
        detail = f"{describe(got.value, limit, row.at_most, row.unit)}; today {row.today}"
        numbers = dict(got.numbers)
        if got.interval is not None:
            low, high = got.interval
            numbers["settled"] = not low <= limit <= high
            if not numbers["settled"]:
                detail += (f"; the 95% interval of the median, {low:.4g} to {high:.4g} {row.unit}, takes in the "
                           "budget: more runs would settle it")
        rep.add(row.name, verdict(got.value, limit, row.at_most), detail + (f"; {got.detail}" if got.detail else ""),
                measured=round(got.value, 4), budget=round(limit, 4), unit=row.unit, today=row.today, **numbers)

    rep.guarded(row.name, go)


def setup_failed(rep: report.Report, rows: Iterable[Row], error: BaseException) -> None:
    """Every row of a project whose setting up failed, failed or erred as that did."""
    said = f"{type(error).__name__}: {error}"
    how: report.Verdict = "error"
    if isinstance(error, Wrong):
        how, said = "fail", str(error)
    elif isinstance(error, engine.Unmeasurable):
        how, said = "unmeasured", str(error)
    elif isinstance(error, engine.Failed):
        said = str(error)
    for row in rows:
        rep.add(row.name, how, f"setting up {row.project}: {said}", today=row.today)


# Rust source, cut into code, literals and comments, for edits that change nothing but whitespace
# and comments.

type Kind = Literal["code", "literal", "comment"]
type Span = tuple[Kind, int, int]
IDENT = re.compile(r"[A-Za-z0-9_]")
RAW = re.compile(r'b?r(#*)"')


def spans(text: str) -> list[Span]:
    """`text` cut, in order, into code, string and character literals, and comments."""
    found: list[Span] = []
    code, i, n = 0, 0, len(text)
    while i < n:
        end, kind = -1, cast(Kind, "literal")
        raw = RAW.match(text, i) if text[i] in "br" else None
        if text.startswith("//", i):
            line = text.find("\n", i)
            end, kind = n if line < 0 else line, "comment"
        elif text.startswith("/*", i):
            depth, j = 1, i + 2
            while j < n and depth:
                if text.startswith("/*", j):
                    depth, j = depth + 1, j + 2
                elif text.startswith("*/", j):
                    depth, j = depth - 1, j + 2
                else:
                    j += 1
            end, kind = j, "comment"
        elif raw and (i == 0 or not IDENT.match(text[i - 1])):
            close = '"' + raw[1]
            j = text.find(close, raw.end())
            end = n if j < 0 else j + len(close)
        elif text[i] == '"':
            j = i + 1
            while j < n and text[j] != '"':
                j += 2 if text[j] == "\\" else 1
            end = min(j + 1, n)
        elif text[i] == "'":
            if text.startswith("\\", i + 1):
                j = text.find("'", i + 3)
                end = n if j < 0 else j + 1
            elif i + 2 < n and text[i + 2] == "'":
                end = i + 3
        if end < 0:
            i += 1
            continue
        if code < i:
            found.append(("code", code, i))
        found.append((kind, i, end))
        code = i = end
    if code < n:
        found.append(("code", code, n))
    return found


class Cut:
    """`spans(text)`, asked where offsets are."""

    def __init__(self, text: str):
        self.spans = spans(text)
        self.starts = [start for _, start, _ in self.spans]

    def in_code(self, at: int) -> bool:
        """Whether the offset `at` is in code, or where a literal or a comment begins."""
        k = bisect.bisect_right(self.starts, at) - 1
        if k < 0:
            return True
        kind, start, end = self.spans[k]
        return kind == "code" or start == at or end <= at


def tokens(text: str) -> list[str]:
    """`text` without whitespace and comments: what a whitespace or comment edit keeps."""
    out: list[str] = []
    for kind, start, end in spans(text):
        if kind == "code":
            out += text[start:end].split()
        elif kind == "literal":
            out.append(text[start:end])
    return out


def closing(text: str, brace: int) -> int:
    """The offset of the `}` that closes the `{` at `brace`."""
    depth = 0
    for kind, start, end in spans(text):
        if kind != "code" or end <= brace:
            continue
        for at in range(max(start, brace), end):
            if text[at] == "{":
                depth += 1
            elif text[at] == "}":
                depth -= 1
                if depth == 0:
                    return at
    raise ValueError(f"no `}}` closes the `{{` at offset {brace}")


@dataclass(frozen=True)
class Function:
    """Where the function `anchor` opens is, by offsets: the start of its line, its `{`, the
    start of the line after it, and its `}` and the start of that one's line."""

    line: int
    brace: int
    body: int
    close_line: int
    close: int


def locate(text: str, anchor: str) -> Function:
    if text.count(anchor) != 1:
        raise ValueError(f"{anchor!r} is {text.count(anchor)} times in the file, not once")
    if not anchor.rstrip().endswith("{"):
        raise ValueError(f"{anchor!r} does not end with the `{{` of a body")
    start = text.index(anchor)
    brace = start + len(anchor.rstrip()) - 1
    line_end = text.find("\n", brace)
    close = closing(text, brace)
    close_line = text.rfind("\n", 0, close) + 1
    if line_end < 0 or close_line <= line_end:
        raise ValueError(f"the body {anchor!r} opens is not on lines of its own")
    if not Cut(text).in_code(line_end + 1):
        raise ValueError(f"the line after {anchor!r} begins inside a literal or a comment")
    return Function(text.rfind("\n", 0, start) + 1, brace, line_end + 1, close_line, close)


def indentation(line: str) -> str:
    return line[: len(line) - len(line.lstrip(" \t"))]


def body_indent(text: str, f: Function) -> str:
    first = text[f.body:text.find("\n", f.body)]
    return indentation(first) if first.strip() else indentation(text[f.line:f.brace]) + "    "


def blank_lines(text: str, anchor: str, n: int = 0) -> str:
    """n + 1 blank lines at the top of the body `anchor` opens, and as many at its end."""
    f = locate(text, anchor)
    gap = "\n" * (n + 1)
    return text[:f.body] + gap + text[f.body:f.close_line] + gap + text[f.close_line:]


def reindent(text: str, anchor: str, n: int = 0) -> str:
    """Every line of the body `anchor` opens indented n + 1 steps further, but for an empty line
    and one that begins inside a literal or a comment, whose text that would change."""
    f = locate(text, anchor)
    cut = Cut(text)
    pad = "    " * (n + 1)
    out, at = [text[:f.body]], f.body
    while at < f.close_line:
        end = text.find("\n", at, f.close_line) + 1 or f.close_line
        line = text[at:end]
        if line.strip() and cut.in_code(at):
            out.append(pad)
        out.append(line)
        at = end
    out.append(text[f.close_line:])
    return "".join(out)


def comment_line(text: str, anchor: str, n: int = 0) -> str:
    """A line of comment at the top of the body `anchor` opens."""
    f = locate(text, anchor)
    return text[:f.body] + f"{body_indent(text, f)}// blaze v2 budgets: comment {n}\n" + text[f.body:]


def inline_comment(text: str, anchor: str, n: int = 0) -> str:
    """A comment after the `{` of the body `anchor` opens, on its line: no line moves."""
    f = locate(text, anchor)
    return text[:f.brace + 1] + f" /* blaze v2 budgets: comment {n} */" + text[f.brace + 1:]


NAMED = "blaze_v2_named"
ITEM = f"\n#[inline(never)]\nfn {NAMED}(x: u32) -> u32 {{\n    x.wrapping_mul(31)\n}}\n"


def with_item(text: str, anchor: str) -> str:
    """A private function at the end of the file, and a call of it at the top of the body
    `anchor` opens: what the rename and the move start from."""
    if NAMED in text:
        raise ValueError(f"{NAMED} is in the file already")
    f = locate(text, anchor)
    call = f"{body_indent(text, f)}let _ = std::hint::black_box({NAMED}(1u32));\n"
    edited = text[:f.body] + call + text[f.body:]
    return (edited if edited.endswith("\n") else edited + "\n") + ITEM


def renamed(text: str, n: int = 0) -> str:
    """`with_item`'s function renamed, and its call."""
    if text.count(NAMED) != 2:
        raise ValueError(f"{NAMED} is {text.count(NAMED)} times in the file, not twice (the function and its call)")
    return text.replace(NAMED, f"blaze_v2_renamed_{n}")


def moved(text: str, anchor: str) -> str:
    """`with_item`'s function moved from the end of the file to after the function that calls it."""
    if not text.endswith(ITEM):
        raise ValueError(f"the file does not end with {NAMED}")
    rest = text[: -len(ITEM)]
    f = locate(rest, anchor)
    if rest[f.line] in " \t":
        raise ValueError(f"{anchor!r} is indented: no item can go after its function")
    line_end = rest.find("\n", f.close)
    at = len(rest) if line_end < 0 else line_end + 1
    if at >= len(rest):
        raise ValueError(f"the function {anchor!r} opens ends the file: a move there moves nothing")
    return rest[:at] + ITEM + rest[at:]


def varied(changes: Sequence[Change], n: int) -> list[Change]:
    """bench.py's edit for run `n`: run 0 as bench.py has it, later runs with another value of its
    statement (`_bench_edit = 1u32`) and another type of its parameter (`_bench_edit: ()`), so
    that no run is the one before it again."""
    if n == 0:
        return list(changes)

    def vary(new: str) -> str:
        new = new.replace("_bench_edit = 1u32", f"_bench_edit = {n + 1}u32")
        return new.replace("_bench_edit: ()", f"_bench_edit: [(); {n}]").replace("(())", f"([(); {n}])")

    return [(pattern, old, vary(new)) for pattern, old, new in changes]


# What the machine's cores did.

@dataclass(frozen=True)
class CpuTimes:
    """Clock ticks of all cores since boot: busy (user, nice, system, interrupts), and all."""

    busy: int
    total: int


def cpu_times(stat: str) -> CpuTimes:
    """The `cpu` line of /proc/stat. Guest time is in user time already; steal counts as idle."""
    for line in stat.splitlines():
        words = line.split()
        if words[:1] == ["cpu"]:
            user, nice, system, idle, iowait, irq, softirq, steal = ([int(w) for w in words[1:9]] + [0] * 8)[:8]
            busy = user + nice + system + irq + softirq
            return CpuTimes(busy, busy + idle + iowait + steal)
    raise ValueError("/proc/stat has no `cpu` line")


def busy_share(before: CpuTimes, after: CpuTimes) -> float:
    """The share of all the machine's cores busy between two readings, on average."""
    if after.total <= before.total:
        raise ValueError("no clock ticks between the two readings of /proc/stat")
    return (after.busy - before.busy) / (after.total - before.total)


def proc_stat() -> CpuTimes | None:
    try:
        return cpu_times(Path("/proc/stat").read_text())
    except OSError:
        return None


# The projects.

def read(path: Path) -> str:
    return path.read_bytes().decode()


def write(path: Path, text: str) -> None:
    path.write_bytes(text.encode())


def left_out(at: str, names: list[str]) -> list[str]:
    return [name for name in names if name in ("target", "target-ref", ".git")]


def checkout(name: str, into: Path) -> tuple[dict[str, Any], Path]:
    """bench.PROJECTS' `name` copied to `into` from its pinned clone (blaze_cli.fetch), which is
    never written to; and its workspace (its `dir`, codex's codex-rs)."""
    if name not in PROJECTS:
        raise ValueError(f"no project {name} in bench.PROJECTS (they are {', '.join(PROJECTS)})")
    spec = PROJECTS[name]
    if "prepare" in spec:
        raise engine.Failed(f"{name}: its tree is made by bench.py's `prepare` step ({spec['prepare']}), not here")
    shutil.copytree(fetch(name, spec), into, symlinks=True, ignore=left_out)
    return spec, into / spec["dir"] if "dir" in spec else into


def project_env(spec: dict[str, Any]) -> dict[str, str]:
    return dict(cast(dict[str, str], spec.get("env") or {}))


def rust_lines(ws: Path) -> int:
    """Lines of the `.rs` files of `ws`, outside its target directories and `.git`."""
    total = 0
    for root, dirs, files in os.walk(ws):
        dirs[:] = [d for d in dirs if d not in ("target", "target-ref", ".git")]
        for name in files:
            if name.endswith(".rs"):
                with open(os.path.join(root, name), "rb") as f:
                    total += sum(chunk.count(b"\n") for chunk in iter(lambda: f.read(1 << 20), b""))
    return total


def perf_counts(text: str) -> dict[str, int]:
    """The counts of `perf stat -x ,`'s output by event, its `:u`-style modifiers taken off. A
    counter the machine does not have is an engine.Failed naming it, never a zero."""
    counts: dict[str, int] = {}
    for line in text.splitlines():
        if not line.strip() or line.startswith("#"):
            continue
        fields = line.split(",")
        if len(fields) < 3:
            continue
        value, event = fields[0].strip(), fields[2].strip().split(":")[0]
        if not value.isdigit():
            raise engine.Failed(f"perf stat: {event} is {value}")
        counts[event] = int(value)
    for event in ("cache-misses", "cache-references"):
        if event not in counts:
            raise engine.Failed(f"perf stat said nothing of {event}:\n{text[-1000:]}")
    return counts


FLOOR_RUNS = 9


def harness_floor_ms(cwd: Path, env: dict[str, str]) -> float:
    """The most of a timed command that is the harness's: the median `engine.run` of `true`, which
    does nothing, so its time is the spawn, the pipes, the reap and `true`'s own exec."""
    true = shutil.which("true")
    if true is None:
        raise engine.Failed("no `true` on PATH: the harness's own share of a timed command is timed on it")
    return statistics.median(engine.run("the harness's floor", [true], cwd, env).seconds for _ in range(FLOOR_RUNS)) * 1000


def placed(said: tuple[int, str, str], build_dir: Path) -> tuple[int, str, str]:
    """What a program said (blaze_cli.program_output), with the build directory it is in named
    `<build>`: two builds of one source into two directories then say the same."""
    status, out, err = said
    return status, out.replace(str(build_dir), "<build>"), err.replace(str(build_dir), "<build>")


def served(eng: engine.Engine, step: str, ran: engine.Ran) -> engine.Ran:
    """`ran`, when it ended well and blaze served it; else a Wrong saying what happened."""
    if ran.returncode != 0:
        raise Wrong(f"{step}: `blaze {ran.argv[1] if len(ran.argv) > 1 else ''}` exited {ran.returncode}:\n{ran.said()}")
    if (who := eng.not_blazes(ran)) is not None:
        raise Wrong(f"{step}: built by {who}, not blaze")
    return ran


def count_numbers(counted: Sequence[counters.Counts]) -> dict[str, report.Number]:
    """counters.summary of a budget's samples, as numbers of its outcome."""
    out: dict[str, report.Number] = {}
    for key, value in counters.summary(counted).items():
        if isinstance(value, list):
            out[key] = " ".join(str(v) for v in cast(list[int], value))
        elif value is None or isinstance(value, (int, float, str, bool)):
            out[key] = value
    return out


@dataclass(frozen=True)
class Edits:
    seconds: tuple[float, ...]
    compiled: tuple[int, ...]
    places: int
    counted: tuple[counters.Counts, ...] = ()

    def numbers(self) -> dict[str, report.Number]:
        ms = [s * 1000 for s in self.seconds]
        return dict(median_ms=round(statistics.median(ms), 3), samples_ms=samples(ms), places=self.places,
                    compiled_each=" ".join(map(str, self.compiled)) or None, **spread(ms, "ms"),
                    **count_numbers(self.counted))


@dataclass(frozen=True)
class Cold:
    cargo_s: float
    cargo_peak_mb: int
    blaze_s: float
    blaze_peak_mb: int


type Apply = Callable[[int], int]
type Revert = Callable[[int], None]


class Lane:
    """One project checked out under `work` (bench.PROJECTS' `name`, or what `source` writes:
    its bench.py spec and its workspace), with a daemon home of its own, which holds its store
    and its build directory (engine.layout), and a cargo target directory of its own outside the
    checkout."""

    def __init__(self, eng: engine.Engine, args: Input, work: Path, name: str,
                 source: Callable[[Path], tuple[dict[str, Any], Path]] | None = None):
        self.eng, self.args, self.name = eng, args, name
        self.top = work / name
        shutil.rmtree(self.top, ignore_errors=True)
        self.top.mkdir(parents=True)
        self.spec, self.ws = checkout(name, self.top / "src") if source is None else source(self.top / "src")
        extra = project_env(self.spec)
        if args.duckdb:
            extra["BLAZE_DUCKDB"] = args.duckdb
        self.home = self.top / "home"
        self.env = eng.env(self.home, None, extra)
        self.ref = self.top / "cargo-target"
        self.cargo_env = engine.reference_env(self.ref, extra)
        self.build_args: list[str] = list(self.spec.get("build") or [])
        self.run_args: list[str] | None = None if self.spec.get("run") is None else list(self.spec["run"])
        # The daemon's processes, counted from its cold build on (`count_daemon`).
        self.meter = counters.Meter()

    def count_daemon(self) -> None:
        """Counts the daemon serving this checkout, and what it starts, from now on."""
        try:
            self.meter.follow(daemon_pid(self.home))
        except engine.Failed as e:
            # The budgets stand without counts; each count says why it is missing.
            self.meter.gap = f"no daemon to count: {e}"

    def counted_build(self, step: str, count: bool) -> tuple[float, engine.Work | None, counters.Counts]:
        """`build`, with what the daemon's processes retired from quiet before it to quiet after."""
        before = self.meter.settle()
        took, work = self.build(step, count)
        return took, work, self.meter.since(before, self.meter.settle())

    def anchor(self) -> tuple[str, str]:
        """The file and the opening line of bench.py's `body` edit."""
        changes: list[Change] = self.spec["edits"]["body"]
        file, opening, _ = changes[0]
        if opening is None:
            raise ValueError(f"{self.name}: bench.py's body edit has no opening line")
        return file, opening

    def cargo(self, step: str, args: list[str]) -> engine.Ran:
        ran = engine.cargo(step, args, self.ws, self.cargo_env)
        if ran.returncode != 0:
            raise engine.Failed(f"{self.name}: {step}: `cargo {' '.join(args)}` exited {ran.returncode}:\n{ran.said()}")
        return ran

    def cold(self) -> Cold:
        """cargo's cold build, then blaze's (whose first `blaze build` starts the daemon) until the
        engine says it is ready: the time and the most their processes held at once."""
        self.cargo("cargo fetch", ["fetch"])
        took, ran, used = memory_run(["cargo", "build", *self.build_args], cwd=self.ws, env=self.cargo_env,
                                     timeout=7200)
        if ran.returncode != 0:
            raise engine.Failed(f"{self.name}: the cold cargo build failed:\n{(ran.stdout + ran.stderr)[-3000:]}")
        sampler = Sampler().add(os.getpid(), itself=False).start()
        began = time.monotonic()
        try:
            self.build("the cold build", False)
            self.eng.wait_ready(self.ws, self.env)
        finally:
            sampler.stop()
        return Cold(took, used["peak_mb"] or 0, time.monotonic() - began, sampler.peak_mb)

    def build(self, step: str, count: bool, args: Sequence[str] | None = None) -> tuple[float, engine.Work | None]:
        """`blaze build`, timed from its spawn to its exit (`engine.Ran.seconds`), so decoding and
        reading its output are not in the time; with `count`, what it did."""
        mark = self.eng.mark(self.home) if count else None
        given = list(self.build_args if args is None else args)
        # What a v2 build did is its `blaze-work` line, printed under JSON only (engine.V2.work_since).
        if count and "--message-format=json" not in given:
            given.append("--message-format=json")
        ran = self.eng.build(f"{self.name}: {step}", self.ws, self.env, given)
        served(self.eng, f"{self.name}: {step}", ran)
        return ran.seconds, None if mark is None else self.eng.work_since(self.home, mark)

    def wrong(self, step: str) -> str | None:
        """None when the program the warm daemon built prints what blaze's build of the same
        source from nothing prints: a home (so a store) and a target directory of its own, made
        for it and removed after it."""
        if self.run_args is None:
            return None
        argv = [*self.build_args, "--", *self.run_args]
        mine = self.eng.build(f"{self.name}: {step}: blaze run", self.ws, self.env, argv, "run")
        # Outside the checkout, as the warm build's directory is: the warm daemon watches the checkout.
        home, target = self.top / "scratch-home", self.top / "scratch-target"
        for path in (home, target):
            shutil.rmtree(path, ignore_errors=True)
        env = self.eng.env(home, target, project_env(self.spec))
        try:
            fresh = self.eng.build(f"{self.name}: {step}: blaze run from nothing", self.ws, env, argv, "run")
            served(self.eng, f"{self.name}: {step}: from nothing", fresh)
        finally:
            try:
                self.eng.stop(self.ws, env)
            finally:
                for path in (home, target):
                    shutil.rmtree(path, ignore_errors=True)
        # The two programs are in two build directories, which one that prints its own path names.
        ours = placed(program_output(mine), self.eng.build_dir(self.ws, self.env))
        scratch = placed(program_output(fresh), target)
        if ours == scratch:
            return None
        return (f"{self.name}: {step}: the warm program says something else than blaze's build of it from "
                f"nothing: {difference(ours, scratch)}")

    def edits(self, step: str, apply: Apply, revert: Revert, count: bool, limit_ms: float | None = None) -> Edits:
        """The edit, a timed build (with `count`, what it compiled), and the edit taken back and
        built: `runs` times, or with `limit_ms` until the times settle it (`enough`). After the
        first, the program is checked against blaze's build of it from nothing (`wrong`)."""
        seconds: list[float] = []
        compiled: list[int] = []
        counted: list[counters.Counts] = []
        places = 0
        n = 0
        while (len(seconds) < self.args.runs if limit_ms is None
               else not enough([s * 1000 for s in seconds], limit_ms, self.args)):
            places = apply(n)
            try:
                took, work, used = self.counted_build(f"{step} {n + 1}", count)
                seconds.append(took)
                counted.append(used)
                if work is not None:
                    if work.compiled is None:
                        raise engine.Unmeasurable(f"{self.eng.kind} did not say how many functions the {step} compiled")
                    compiled.append(work.compiled)
                if n == 0 and (bad := self.wrong(step)):
                    raise Wrong(bad)
            finally:
                revert(n)
            self.build(f"{step} {n + 1} taken back", False)
            n += 1
        return Edits(tuple(seconds), tuple(compiled), places, tuple(counted))

    def text_edit(self, file: str, make: Callable[[str, int], str]) -> tuple[Apply, Revert]:
        path = self.ws / file
        before = read(path)

        def apply(n: int) -> int:
            write(path, make(before, n))
            return 1

        def revert(n: int) -> None:
            write(path, before)

        return apply, revert

    def bench_edit(self, kind: str) -> tuple[Apply, Revert]:
        changes: list[Change] = self.spec["edits"][kind]

        def apply(n: int) -> int:
            return write_edit(str(self.ws), varied(changes, n), False)

        def revert(n: int) -> None:
            write_edit(str(self.ws), varied(changes, n), True)

        return apply, revert

    def timed_edit(self, step: str, limit_ms: float) -> Measured:
        e = self.edits(step, *self.bench_edit("body"), False, limit_ms)
        return timed([s * 1000 for s in e.seconds], "ms", places=e.places, **count_numbers(e.counted))

    def nothing_compiled(self, step: str, make: Callable[[str, int], str]) -> Measured:
        file, _ = self.anchor()
        e = self.edits(step, *self.text_edit(file, make), True)
        return Measured(max(e.compiled), numbers=e.numbers())

    def signature(self) -> Measured:
        e = self.edits("signature edit", *self.bench_edit("signature"), True)
        return Measured(max(e.compiled), self.args.slack * e.places, f"{e.places} places changed",
                        dict(e.numbers(), slack=self.args.slack))

    def noop(self, limit_ms: float) -> Measured:
        self.build("the no-op build that warms up", False)
        ms: list[float] = []
        counted: list[counters.Counts] = []
        while not enough(ms, limit_ms, self.args):
            took, _, used = self.counted_build(f"no-op build {len(ms) + 1}", False)
            ms.append(took * 1000)
            counted.append(used)
        return timed(ms, "ms", harness_floor_ms=round(harness_floor_ms(self.ws, self.env), 3), **count_numbers(counted))

    def warm_start(self, limit_ms: float) -> Measured:
        ms: list[float] = []
        while not enough(ms, limit_ms, self.args):
            n = len(ms)
            self.eng.stop(self.ws, self.env)
            began = time.monotonic()
            self.build(f"the first build after stopping the daemon {n + 1}", False)
            self.eng.wait_ready(self.ws, self.env)
            ms.append((time.monotonic() - began) * 1000)
        if bad := self.wrong("after a warm start"):
            raise Wrong(bad)
        return timed(ms, "ms")

    def speed(self, profile: Literal["debug", "release"]) -> Measured:
        """bench.py's `runtime` workload, blaze's program timed against cargo's of `profile`, in
        turns; blaze's has to exit 0."""
        workload: dict[str, Any] = self.spec["runtime"]
        program = str(workload["bin"]) + engine.EXE
        args = [str(a).replace("{library}", library()) for a in workload["args"]]
        if profile == "release":
            self.build("the release build", False, [*self.build_args, "--release"])
            self.cargo("cargo build --release", ["build", "--release", *self.build_args])
        # blaze's program is in the build directory under its daemon's home, not in the checkout.
        programs = {"blaze": self.eng.build_dir(self.ws, self.env) / profile / program,
                    "cargo": self.ref / profile / program}
        env = engine.scrubbed()
        times: dict[str, list[float]] = {label: [] for label in programs}
        said: dict[str, tuple[int, str]] = {}
        least, most = self.args.runtime_runs, max(self.args.runtime_runs, self.args.most_runs)
        turn = 0
        while turn < 2 or not all(known(xs, WIDTH / 2, least, most) for xs in times.values()):
            turn += 1
            for label, path in programs.items():
                ran = engine.run(f"{self.name}: {label}'s {profile} {program}", [str(path), *args], self.ws, env,
                                 RUN_SECS)
                said.setdefault(label, (ran.returncode, ran.stdout))
                if turn > 2:
                    times[label].append(ran.seconds)
        if said["cargo"][0] != 0:
            raise engine.Failed(f"{self.name}: cargo's {profile} {program} exited {said['cargo'][0]}")
        if said["blaze"][0] != 0:
            raise Wrong(f"{self.name}: blaze's {profile} {program} exited {said['blaze'][0]}")
        ours, cargos = statistics.median(times["blaze"]), statistics.median(times["cargo"])
        return Measured(ours / cargos, 1.0 + self.args.noise,
                        numbers=dict(blaze_median_s=round(ours, 4), cargo_median_s=round(cargos, 4),
                                     blaze_samples_s=samples(times["blaze"]), cargo_samples_s=samples(times["cargo"]),
                                     **spread(times["blaze"], "s", "blaze_"), **spread(times["cargo"], "s", "cargo_"),
                                     runs=len(times["blaze"])))

    def layout(self, key: str) -> Measured:
        name = TABLES[key]
        tables = self.eng.tables(self.ws, self.env)
        table = tables.get(name)
        if table is None or table.elements == 0:
            raise engine.Failed(f"{self.name}: the engine's tables have no {name!r} with elements in it "
                                f"(it has {', '.join(sorted(tables)) or 'none'})")
        return Measured(table.bytes / table.elements, numbers=dict(elements=table.elements, bytes=table.bytes))

    def memory_per_kloc(self, cold: Cold) -> Measured:
        lines = rust_lines(self.ws)
        if lines == 0:
            raise engine.Failed(f"{self.name}: no Rust source under {self.ws}")
        kloc = lines / 1000
        return Measured(cold.blaze_peak_mb / kloc, cold.cargo_peak_mb / kloc, f"{lines} lines of the workspace's .rs files",
                        dict(lines=lines, blaze_peak_mb=cold.blaze_peak_mb, cargo_peak_mb=cold.cargo_peak_mb))

    def cache_misses(self) -> Measured:
        """bench.py's `body` edit `runs` times, each build under `perf stat -a`: the machine's
        cache misses over it, which counts the daemon's however it is started."""
        if shutil.which("perf") is None:
            raise engine.Failed("perf is not on PATH: cache misses are read with `perf stat`")
        apply, revert = self.bench_edit("body")
        misses: list[int] = []
        references: list[int] = []
        counts_file = self.top / "perf-stat.csv"
        for n in range(self.args.runs):
            apply(n)
            try:
                step = f"{self.name}: body edit {n + 1} under perf stat"
                ran = engine.run(step, ["perf", "stat", "-a", "-x", ",", "-e", "cache-misses,cache-references",
                                        "-o", str(counts_file), "--", str(self.eng.blaze), "build", *self.build_args],
                                 self.ws, self.env, RUN_SECS)
                if ran.returncode != 0:
                    raise engine.Failed(f"{step}: exited {ran.returncode}:\n{ran.said()}")
                served(self.eng, step, ran)
                counts = perf_counts(counts_file.read_text())
                misses.append(counts["cache-misses"])
                references.append(counts["cache-references"])
            finally:
                revert(n)
            self.build(f"body edit {n + 1} taken back", False)
        return Measured(statistics.median(misses), detail="docs/v2/plan/design/data.md asks for the number and sets no budget",
                        numbers=dict(samples=samples(misses), references_median=statistics.median(references)))

    def items(self, rep: report.Report, rows: dict[str, Row]) -> None:
        """The rename and the move, from a private function and its call added first."""
        chosen = [rows[key] for key in ("rename", "move") if key in rows]
        if not chosen:
            return
        try:
            file, opening = self.anchor()
            path = self.ws / file
            original = read(path)
        except SETUP_ERRORS as e:
            setup_failed(rep, chosen, e)
            return
        try:
            write(path, with_item(original, opening))
            self.build("a function and its call added", False)
        except SETUP_ERRORS as e:
            write(path, original)
            setup_failed(rep, chosen, e)
            return
        try:
            if "rename" in rows:
                judge(rep, rows["rename"], lambda: self.nothing_compiled("rename", renamed))
            if "move" in rows:
                judge(rep, rows["move"], lambda: self.nothing_compiled("move", lambda text, n: moved(text, opening)))
        finally:
            write(path, original)
        try:
            self.build("the function and its call taken out", False)
        except SETUP_ERRORS as e:
            rep.add(f"{self.name}: the function and its call taken out", "error", f"{type(e).__name__}: {e}")

    def stop(self, rep: report.Report) -> None:
        self.meter.close()
        try:
            self.eng.stop(self.ws, self.env)
        except engine.Failed as e:
            rep.add(f"{self.name}: stopping the daemon", "error", str(e))


def third_party(eng: engine.Engine, rep: report.Report, args: Input, work: Path, name: str,
                rows: dict[str, Row]) -> None:
    try:
        lane = Lane(eng, args, work, name)
    except SETUP_ERRORS as e:
        setup_failed(rep, rows.values(), e)
        return
    try:
        try:
            cold = lane.cold()
            if bad := lane.wrong("the cold build"):
                raise Wrong(bad)
            lane.count_daemon()
        except SETUP_ERRORS as e:
            setup_failed(rep, rows.values(), e)
            return
        if f"memory-{name}" in rows:
            judge(rep, rows[f"memory-{name}"], lambda: Measured(
                cold.blaze_peak_mb, cold.cargo_peak_mb, numbers=dict(
                    cargo_peak_mb=cold.cargo_peak_mb, blaze_s=round(cold.blaze_s, 3), cargo_s=round(cold.cargo_s, 3))))
        if "memory-kloc" in rows:
            judge(rep, rows["memory-kloc"], lambda: lane.memory_per_kloc(cold))
        for key in TABLES:
            if key in rows:
                judge(rep, rows[key], lambda: lane.layout(key))
        if "noop" in rows:
            judge(rep, rows["noop"], lambda: lane.noop(rows["noop"].limit))
        if "dev-speed" in rows:
            judge(rep, rows["dev-speed"], lambda: lane.speed("debug"))
        if "release-speed" in rows:
            judge(rep, rows["release-speed"], lambda: lane.speed("release"))
        for key in ("body", "big-crate"):
            if key in rows:
                judge(rep, rows[key], lambda: lane.timed_edit("body edit", rows[key].limit))
        if "signature" in rows:
            judge(rep, rows["signature"], lane.signature)
        if "cache-misses" in rows:
            judge(rep, rows["cache-misses"], lane.cache_misses)
        generated: dict[str, Callable[[str, str, int], str]] = {
            "format-blank": blank_lines, "format-indent": reindent, "comment-line": comment_line,
            "comment-inline": inline_comment,
        }
        for key, make in generated.items():
            if key in rows:
                judge(rep, rows[key], lambda: lane.nothing_compiled(key, lambda text, n: make(text, lane.anchor()[1], n)))
        lane.items(rep, rows)
        if "warm-start" in rows:
            judge(rep, rows["warm-start"], lambda: lane.warm_start(rows["warm-start"].limit))
    finally:
        lane.stop(rep)


def copy_checkout(root: Path, into: Path) -> None:
    """The files of the checkout at `root` as they are now, tracked or not but not ignored."""
    listed = subprocess.run(["git", "-C", str(root), "ls-files", "-z", "--cached", "--others", "--exclude-standard"],
                            capture_output=True, check=True).stdout.decode()
    for name in sorted(set(listed.split("\0")) - {""}):
        source, dest = root / name, into / name
        if not source.exists() and not source.is_symlink():
            continue
        dest.parent.mkdir(parents=True, exist_ok=True)
        if source.is_symlink():
            os.symlink(os.readlink(source), dest)
        elif source.is_dir():
            shutil.copytree(source, dest, symlinks=True)
        else:
            shutil.copy2(source, dest)


def own(eng: engine.Engine, rep: report.Report, args: Input, work: Path, rows: dict[str, Row]) -> None:
    """The cold build of this checkout, then its CI for a small pull request."""
    top = work / "blaze"
    ws = top / "src"
    home = top / "home"
    try:
        shutil.rmtree(top, ignore_errors=True)
        copy_checkout(engine.ROOT, ws)
        env = eng.env(home)
    except SETUP_ERRORS as e:
        setup_failed(rep, rows.values(), e)
        return
    try:
        try:
            before = proc_stat()
            began = time.monotonic()
            # JSON: the critical path is the build's `blaze-work` line (engine.V2.critical_path_s).
            served(eng, "the cold build of blaze", eng.build("the cold build of blaze", ws, env,
                                                             ["--message-format=json"], timeout=COLD_BLAZE_SECS))
            eng.wait_ready(ws, env, COLD_BLAZE_SECS)
            wall = time.monotonic() - began
            after = proc_stat()
        except SETUP_ERRORS as e:
            setup_failed(rep, rows.values(), e)
            return

        def busy() -> Measured:
            if before is None or after is None:
                raise engine.Failed(f"no /proc/stat on {sys.platform}: the share of the cores busy is read from it")
            return Measured(busy_share(before, after), numbers=dict(wall_s=round(wall, 1), cores=os.cpu_count()))

        def critical() -> Measured:
            path = eng.critical_path_s(ws, env)
            return Measured(wall / path, args.critical_slack,
                            numbers=dict(wall_s=round(wall, 1), critical_path_s=round(path, 1)))

        if "cold-busy" in rows:
            judge(rep, rows["cold-busy"], busy)
        if "cold-critical" in rows:
            judge(rep, rows["cold-critical"], critical)
        if "ci" in rows:
            judge(rep, rows["ci"], lambda: ci(eng, args, ws, env, rows["ci"].limit))
        for how in get_args(Restart):
            if (key := f"restart-first-answer-{how}") in rows:
                judge(rep, rows[key], lambda: restart(eng, args, ws, env, how))
    finally:
        try:
            eng.stop(ws, env)
        except engine.Failed as e:
            rep.add("blaze: stopping the daemon", "error", str(e))


def reached_args(self_test: Sequence[str]) -> list[str]:
    """`blaze test`'s arguments for the CI budget: `self_test`, and before any `--` of it
    `--reached` (the daemon runs only the tests the edit can have changed and reports a kept
    pass as cached) with JSON messages, where each result is a `blaze-test-result` line
    (docs/cargo.md, "Which tests run"). v1's `BLAZE_TESTS=affected` is gone with v1."""
    at = self_test.index("--") if "--" in self_test else len(self_test)
    return [*self_test[:at], "--reached", "--message-format=json", *self_test[at:]]


def tests_run(step: str, ran: engine.Ran) -> int:
    """How many tests a `blaze test --reached --message-format=json` ran (not kept from a pass
    before), from its `blaze-test-result` lines; a Wrong when it exited other than 0 or a test
    failed, an engine.Failed when a line cannot be read."""
    try:
        lines = storelib.test_results(ran.stdout)
    except storelib.Failed as e:
        raise engine.Failed(f"{step}: {e}") from None
    failed = sorted(f"{line.binary}: {line.name}" for line in lines if line.outcome == "failed")
    if ran.returncode != 0 or failed:
        raise Wrong(f"{step}: `blaze test` exited {ran.returncode}; failed: {', '.join(failed[:20]) or 'none named'}\n"
                    f"{ran.said()}")
    return sum(line.cached is None and line.outcome in ("passed", "failed") for line in lines)


def ci(eng: engine.Engine, args: Input, ws: Path, env: dict[str, str], limit_s: float) -> Measured:
    def test(step: str) -> int:
        ran = eng.build(step, ws, env, reached_args(args.self_test), "test", timeout=COLD_BLAZE_SECS)
        if (who := eng.not_blazes(ran)) is not None:
            raise Wrong(f"{step}: run by {who}, not blaze")
        return tests_run(step, ran)

    test("the first blaze test")
    edit = args.self_edit
    changes: list[Change] = [(edit.file, edit.opening, edit.opening + LET)]
    seconds: list[float] = []
    ran_tests = 0
    while not enough(seconds, limit_s, args):
        n = len(seconds)
        write_edit(str(ws), varied(changes, n), False)
        try:
            began = time.monotonic()
            served(eng, f"the CI build {n + 1}", eng.build(f"the CI build {n + 1}", ws, env))
            ran_tests = test(f"the affected tests {n + 1}")
            seconds.append(time.monotonic() - began)
        finally:
            write_edit(str(ws), varied(changes, n), True)
        served(eng, f"the CI edit {n + 1} taken back", eng.build(f"the CI edit {n + 1} taken back", ws, env))
    return timed(seconds, "s", tests_run=ran_tests, edited=edit.file)


def daemon_pid(home: Path) -> int:
    """The daemon running on `home`, from its lock (`storelib.daemon_pid`): `blaze status` says
    its pid for people only, with no machine format."""
    pid = storelib.daemon_pid(str(home))
    if pid is None:
        raise engine.Failed(f"no daemon runs on {home}: {engine.layout(home).lock} names none that runs")
    return pid


def restart_limit(warm_ms: float, files: int, slack_ms: float, sweeps: bool) -> float:
    """The restart budget: the warm answer, `slack_ms` over it, and where no journal survives a
    restart (Linux) the statx sweep of `files`."""
    return warm_ms + slack_ms + (files * STATX_US_PER_FILE / 1000 if sweeps else 0.0)


def source_files(ws: Path) -> int:
    """Files of `ws` outside its target directories and `.git`: what a statx sweep looks at."""
    total = 0
    for _, dirs, files in os.walk(ws):
        dirs[:] = [d for d in dirs if d not in ("target", "target-ref", ".git")]
        total += len(files)
    return total


def until_gone(what: str, running: Callable[[], bool]) -> None:
    began = time.monotonic()
    while running():
        if time.monotonic() - began > GONE_SECS:
            raise engine.Failed(f"{what} still runs {GONE_SECS:.0f} s on")
        time.sleep(0.002)


def alive(pid: int) -> bool:
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    except PermissionError:
        # The number is another user's process now: the daemon is gone.
        return False
    return True


def group_runs(group: int) -> bool:
    try:
        os.killpg(group, 0)
    except ProcessLookupError:
        return False
    return True


def kill_group(pid: int) -> int:
    """`kill -9 -PGID` of the daemon's process group: the daemon and every process it started
    (workers, test fork servers, macro runtimes), at once. Its group."""
    if not hasattr(os, "killpg"):
        raise engine.Failed(f"no process groups on {sys.platform}: the kill -9 budget is POSIX's")
    group = os.getpgid(pid)
    if group == os.getpgrp():
        raise engine.Failed(f"the daemon {pid} is in the harness's process group {group}: a SIGKILL to it would "
                            "kill the harness (the daemon is started in a group of its own)")
    os.killpg(group, signal.SIGKILL)
    return group


def restart(eng: engine.Engine, args: Input, ws: Path, env: dict[str, str], how: Restart) -> Measured:
    """Until the first answers settle the budget (`enough`, against the limit of the warm checks so
    far): a warm no-op `blaze check`, the daemon stopped (`how`), and the first answer after it,
    a no-op `blaze check` timed from the command to its result."""

    def check(step: str) -> float:
        return served(eng, step, eng.build(step, ws, env, [], "check")).seconds * 1000

    def units(verb: str) -> None:
        eng.command(f"`blaze daemon {verb}`", "daemon", [verb], ws, env, 300).checked(f"`blaze daemon {verb}`")

    said = {"clean": "a clean stop", "kill9": "kill -9 of the daemon's process group"}[how]
    if args.activation:
        units("install")
    files = source_files(ws)
    sweeps = sys.platform == "linux"
    warm: list[float] = []
    first: list[float] = []
    try:
        check("the no-op check that warms up")
        while not first or not enough(first, restart_limit(statistics.median(warm), files, args.restart_slack_ms, sweeps),
                                      args):
            n = len(first)
            warm.append(check(f"warm no-op check {n + 1}"))
            pid = daemon_pid(Path(env["BLAZE_HOME"]))
            if how == "clean":
                eng.stop(ws, env)
                until_gone(f"the daemon {pid} after `blaze stop`", lambda: alive(pid))
            else:
                group = kill_group(pid)
                until_gone(f"the process group {group} of the daemon {pid} after SIGKILL", lambda: group_runs(group))
            first.append(check(f"the first answer after {said} {n + 1}"))
    finally:
        if args.activation:
            units("uninstall")
    warm_ms = statistics.median(warm)
    limit = restart_limit(warm_ms, files, args.restart_slack_ms, sweeps)
    got = timed(first, "ms", warm_median_ms=round(warm_ms, 3), warm_samples_ms=samples(warm), files=files,
                slack_ms=args.restart_slack_ms, activated=args.activation, **spread(warm, "ms", "warm_"))
    sweep = f" + the statx sweep of {files} files at {STATX_US_PER_FILE} us each" if sweeps else ""
    started = "socket-activated" if args.activation else "started by the command line"
    return Measured(got.value, limit, f"after {said}, {started}: the warm no-op check's median {warm_ms:.4g} ms + "
                    f"{args.restart_slack_ms:.4g} ms{sweep}", got.numbers, got.interval)


def bound_row(found: bounds.Found) -> Row:
    """A lane's bound as a row: its budget is the bound x 1.1 (a rate's, the bound / 1.1)."""
    b = found.bound
    return Row(f"bound-{b.name}", "bounds", "blaze", f"{b.name}: speed of light {b.bound:g} {b.unit} ({found.where})",
               found.budget, not b.rate, b.unit, "the bound, from the lane's arithmetic; not measured before")


def measure_bound(eng: engine.Engine, args: Input, found: bounds.Found) -> Measured:
    """The bound's program, beside the measured `blaze`, run until its values settle the budget."""
    b = found.bound
    program = Path(args.engine.blaze).resolve().parent / (b.program + engine.EXE)
    values: list[float] = []
    while not enough(values, found.budget, args):
        step = f"{b.name}: `{b.program}` run {len(values) + 1}"
        ran = engine.run(step, [str(program), *b.args], engine.ROOT, engine.scrubbed(), RUN_SECS, check=True)
        values.append(bounds.value_of(ran.stdout, step))
    return timed(values, b.unit, bound=b.bound, where=found.where)


def smoke_source(into: Path) -> tuple[dict[str, Any], Path]:
    """fixture.py's workspace in its default state written to `into`, and the bench.py spec the
    smoke's lane reads: the body edit (`SMOKE_FILE`, `SMOKE_OPENING`), and its program built
    and run by name."""
    fixture.write_workspace(into)
    spec = {"edits": {"body": [(SMOKE_FILE, SMOKE_OPENING, SMOKE_OPENING + LET)]},
            "build": ["-p", SMOKE_PROGRAM], "run": []}
    return spec, into


def smoke(eng: engine.Engine, args: Input, rep: report.Report, work: Path) -> None:
    """The smoke (the docstring's): one no-op build and one body edit of fixture.py's workspace
    through `Lane`, each an outcome that passes only with a number, and the daemon's lock and
    store found where engine.layout puts them."""
    began = time.monotonic()
    once = dataclasses.replace(args, runs=1, most_runs=1)
    rows = {row.key: row for row in SMOKE_ROWS}
    # The workspace's guard steps, or why it gave none: never dropped (`smoke_guards`).
    gathered: list[Gathered] = []
    try:
        lane = Lane(eng, once, work, "fixture", smoke_source)
    except SETUP_ERRORS as e:
        setup_failed(rep, rows.values(), e)
        gathered += no_facts("workspace", f"setting up the workspace: {type(e).__name__}: {e}")
        smoke_guards(eng, args, rep, work, gathered)
        return
    try:
        try:
            lane.build("the cold build", False)
            eng.wait_ready(lane.ws, lane.env)
            lane.count_daemon()
        except SETUP_ERRORS as e:
            setup_failed(rep, rows.values(), e)
            gathered += no_facts("workspace", f"the cold build of the workspace: {type(e).__name__}: {e}")
            return
        judge(rep, rows["noop"], lambda: lane.noop(rows["noop"].limit))
        judge(rep, rows["body"], lambda: lane.timed_edit("body edit", rows["body"].limit))
        laid = engine.layout(lane.home)
        missing = [f"{what} {path}" for what, path, there in (
            ("lock", laid.lock, os.path.isfile(laid.lock)), ("store", laid.store, os.path.isdir(laid.store)),
        ) if not there]
        rep.add("smoke: the daemon's lock and store are where engine.layout says", "error" if missing else "pass",
                f"after the builds, the home {lane.home} has no {' and no '.join(missing)}: the daemon keeps them "
                "elsewhere than storelib.Layout says" if missing else f"{laid.lock}, {laid.store}")
        gathered += guard_steps(eng, lane, "workspace", SMOKE_FILE, SMOKE_OPENING)
    finally:
        lane.stop(rep)
        smoke_guards(eng, args, rep, work, gathered)
    for row in rows.values():
        got = [o.numbers.get("measured") for o in rep.outcomes if o.name == row.name and o.verdict == "pass"]
        if not (len(got) == 1 and isinstance(got[0], (int, float)) and math.isfinite(got[0])):
            rep.add(f"{row.name}: a measured number", "error", f"the smoke measured no number for it: {got}")
    # Where the time went: each `blaze` command the smoke ran, with its seconds (the rest is
    # daemon stops, the readiness wait and the copies).
    steps: list[tuple[str, float]] = getattr(eng, "took", [])
    rep.extra.update(seconds=round(time.monotonic() - began, 1), steps_s={step: round(s, 2) for step, s in steps})
    for step, s in steps:
        print(f"smoke: {s:7.2f} s  {step}", flush=True)
    print(f"smoke: {time.monotonic() - began:.1f} s", flush=True)


def sql(eng: engine.Engine, lane: Lane, what: str, query: str) -> str:
    """What `blaze debug sql --json QUERY` prints on `lane`'s daemon; a failure is an engine.Failed naming `what`."""
    step = f"{lane.name}: {what}"
    ran = eng.command(step, "debug", ["sql", "--json", query], lane.ws, lane.env, 300)
    return ran.checked(step).stdout


def log_text(lane: Lane, since: int = 0) -> str:
    """The daemon's log from byte `since` on; empty where it has none yet."""
    try:
        with open(engine.layout(lane.home).log, "rb") as f:
            f.seek(since)
            return engine.text(f.read())
    except FileNotFoundError:
        return ""


type Facts = dict[str, float | None]
# What a guard step gave: its facts, or why it gave none.
type Gathered = tuple[guards.Fixture, guards.Step, Facts | None, str]


def guard_facts(eng: engine.Engine, lane: Lane, step: str, took_s: float, warm_from: int, log_from: int) -> Facts:
    """What `lane`'s last build did, as guards.py's bounds name it: the client's wait, the
    profile's row of it (`guards.FACTS_SQL`, the view `uncovered_time`) and the log's lines."""
    row = guards.one_row(sql(eng, lane, f"guards: {step}: the profile's facts", guards.facts_sql(warm_from)),
                         f"{lane.name}: {step}")
    facts: Facts = {column: guards.number(row, column, f"{lane.name}: {step}") for column in guards.PROFILE_FACTS}
    uncovered = guards.one_row(sql(eng, lane, f"guards: {step}: uncovered_time", guards.UNCOVERED_SQL),
                               f"{lane.name}: {step}: uncovered_time")
    facts["uncovered_ms"] = guards.number(uncovered, "uncovered_ms", f"{lane.name}: {step}: uncovered_time")
    facts["ms"] = took_s * 1000
    facts["let_go_logged"] = float(guards.let_go_lines(log_text(lane, log_from)))
    return facts


def no_facts(fixture: guards.Fixture, why: str) -> list[Gathered]:
    """Every step of `fixture` with no facts, and why."""
    return [(fixture, step, None, why) for step in guards.STEPS]


def guard_steps(eng: engine.Engine, lane: Lane, fixture: guards.Fixture, file: str, opening: str) -> list[Gathered]:
    """guards.py's three steps on `lane`, which was built cold: a warm no-op build, a comment at
    the end of the line `opening` is in `file`, and a statement added to the body it opens; each
    edit taken back and built before the next. A step that could not be run or read says why."""
    path = lane.ws / file
    try:
        before = read(path)
        lane.build("guards: the build that warms up", False)
        mark = guards.one_row(sql(eng, lane, "guards: the mark", guards.MARK_SQL), f"{lane.name}: the mark")
        warm_from = guards.number(mark, "warm_from", f"{lane.name}: the mark")
        if warm_from is None:
            raise engine.Failed(f"{lane.name}: the daemon's history has no build after the cold build and a warm one")
        log_from = len(log_text(lane).encode())
    except SETUP_ERRORS as e:
        return no_facts(fixture, f"setting up {fixture}: {type(e).__name__}: {e}")
    edits: dict[guards.Step, Callable[[str, str, int], str] | None] = {
        "noop": None, "comment": guards.eol_comment, "body": guards.body_statement}
    out: list[Gathered] = []
    for step in guards.STEPS:
        make = edits[step]
        try:
            if make is not None:
                write(path, make(before, opening, 1))
            try:
                took, _ = lane.build(f"guards: {step}", False)
                facts = guard_facts(eng, lane, step, took, int(warm_from), log_from)
            finally:
                if make is not None:
                    write(path, before)
            if make is not None:
                lane.build(f"guards: {step} taken back", False)
            out.append((fixture, step, facts, ""))
        except SETUP_ERRORS as e:
            out.append((fixture, step, None, f"{type(e).__name__}: {e}"))
    return out


def guard_outcomes(rep: report.Report, gathered: Sequence[Gathered], known: Sequence[guards.Known]) -> None:
    """An outcome for every bound of every step gathered: judged against `known`, or an error
    saying why the step gave no facts. Every name of `guards.names()` for those steps is added."""
    for fixture, step, facts, why in gathered:
        if facts is None:
            for bound in guards.BOUNDS:
                if bound.limit(step) is not None:
                    rep.add(guards.name(fixture, step, bound.check), "error", why)
            continue
        for it in guards.judge(fixture, step, facts, known):
            rep.add(it.name, it.verdict, it.detail, measured=it.measured, budget=it.limit, unit=it.unit, known=it.known)


def sandbox_probe(eng: engine.Engine, rep: report.Report, args: Input, work: Path) -> None:
    """guards.py's probe built and run within `PROBE_SECS`: its build script runs `$RUSTC`, which
    is blaze inside the build-script sandbox. A hang, a failure, or a probe that did not compile
    is the outcome `guards.PROBE` failing, naming the step."""
    step = "probe: `blaze run` of a program whose build script runs $RUSTC"
    try:
        lane = Lane(eng, args, work, "probe", guards.probe_source)
    except SETUP_ERRORS as e:
        rep.add(guards.PROBE, "error", f"setting up the probe: {type(e).__name__}: {e}")
        return
    ended = False
    try:
        ran = eng.build(step, lane.ws, lane.env, [], "run", PROBE_SECS)
        ended = True
        if ran.returncode != 0:
            rep.add(guards.PROBE, "fail", f"{step}: exited {ran.returncode}:\n{ran.said()}", seconds=round(ran.seconds, 2))
        elif guards.PROBE_SAYS not in ran.stdout:
            rep.add(guards.PROBE, "fail", f"{step}: the program did not print {guards.PROBE_SAYS!r}: its build script's "
                    f"probe was not seen to compile:\n{ran.said()}", seconds=round(ran.seconds, 2))
        else:
            rep.add(guards.PROBE, "pass", f"{ran.seconds:.1f} s against at most {PROBE_SECS}", seconds=round(ran.seconds, 2))
    except engine.Failed as e:
        rep.add(guards.PROBE, "fail", f"{e}\nblaze hung or could not be run inside its build-script sandbox "
                "(2026-11-10: its workers' wait ring was refused there, each ended silently, and the main thread parked "
                "with no bound)")
    finally:
        if ended:
            lane.stop(rep)
        else:
            # A daemon that hangs on the build does not answer a stop either.
            try:
                kill_group(daemon_pid(lane.home))
            except (engine.Failed, OSError):
                pass
            lane.meter.close()


def smoke_guards(eng: engine.Engine, args: Input, rep: report.Report, work: Path, gathered: list[Gathered]) -> None:
    """The guards after the workspace's (`gathered` holds its steps): the one-file program's
    steps, the outcomes of both against the known list (written first, with `record`), the
    sandbox's probe, and an error for any outcome of `guards.names()` that is still missing."""
    once = dataclasses.replace(args, runs=1, most_runs=1)
    try:
        lane = Lane(eng, once, work, "onefile", guards.onefile_source)
    except SETUP_ERRORS as e:
        gathered += no_facts("onefile", f"setting up onefile: {type(e).__name__}: {e}")
    else:
        try:
            try:
                lane.build("the cold build", False)
                eng.wait_ready(lane.ws, lane.env)
            except SETUP_ERRORS as e:
                gathered += no_facts("onefile", f"the cold build of onefile: {type(e).__name__}: {e}")
            else:
                gathered += guard_steps(eng, lane, "onefile", guards.ONEFILE_FILE, guards.ONEFILE_OPENING)
        finally:
            lane.stop(rep)
    try:
        known = guards.read_known(guards.KNOWN.read_text())
        if args.record:
            judged: list[tuple[guards.Fixture, guards.Step, guards.Judged]] = [
                (fixture, step, it) for fixture, step, facts, _ in gathered if facts is not None
                for it in guards.judge(fixture, step, facts, ())]
            known = guards.recorded_from(judged, known)
            guards.KNOWN.write_text(guards.render_known(known, dict(
                commit=report.commit() or "", blaze=getattr(eng, "version", ""), date=time.strftime("%Y-%m-%d"))))
            print(f"smoke: {guards.KNOWN} written: {len(known)} known failures", flush=True)
    except (ValueError, OSError) as e:
        rep.add("guard: the known list (harness/v2/guards-known.toml)", "error", f"{type(e).__name__}: {e}")
        known = []
    guard_outcomes(rep, gathered, known)
    sandbox_probe(eng, rep, once, work)
    given = {o.name for o in rep.outcomes}
    for wanted in guards.names():
        if wanted not in given:
            rep.add(wanted, "error", "the smoke gave no outcome for it: nothing asked for is dropped")


def main(args: Input) -> None:
    if args.runs < 1 or args.runtime_runs < 1:
        fail(SCRIPT, f"runs and runtime_runs: at least 1, not {args.runs} and {args.runtime_runs}")
    if args.most_runs < args.runs:
        fail(SCRIPT, f"most_runs: at least runs ({args.runs}), not {args.most_runs}")
    if args.command == "smoke" and (args.projects or args.budgets):
        fail(SCRIPT, "command `smoke`: it measures fixture.py's workspace alone and takes no `projects` or `budgets`")
    if args.command != "smoke" and args.record:
        fail(SCRIPT, "record: it writes the smoke's known list, and the command is not `smoke`")
    if args.duckdb and not os.path.isabs(args.duckdb):
        fail(SCRIPT, f"duckdb: an absolute path, not {args.duckdb!r}")
    if args.command == "run" and not args.out:
        fail(SCRIPT, "out: command `run` writes its result there, and was given none")
    try:
        chosen = select(args.projects, args.budgets)
    except ValueError as e:
        fail(SCRIPT, f"projects and budgets: {e}")
    measures_bounds = args.command == "run" and "bounds" in (args.budgets or get_args(Budget))
    found_bounds = bounds.read_all(engine.ROOT) if measures_bounds else []
    try:
        eng = engine.Engine.of(args.engine)
    except engine.Failed as e:
        fail(SCRIPT, str(e))
    rep = report.Report(SCRIPT, args.engine, args.out)
    prefix = "bz-smoke-" if args.command == "smoke" else "bz-budgets-"
    try:
        work = engine.work_dir(args.work, prefix)
    except engine.Failed as e:
        fail(SCRIPT, str(e))
    print(f"work: {work}", flush=True)
    if args.command == "smoke":
        smoke(eng, args, rep, work)
        if not args.keep:
            for name in ("fixture", "onefile", "probe"):
                shutil.rmtree(work / name, ignore_errors=True)
            if not args.work:
                shutil.rmtree(work, ignore_errors=True)
        rep.extra.update(work=str(work), blaze=getattr(eng, "version", ""))
        rep.finish()
    for project in dict.fromkeys(row.project for row in chosen):
        rows = {row.key: row for row in chosen if row.project == project}
        print(f"{project}: {', '.join(rows)}", flush=True)
        if project == "blaze":
            own(eng, rep, args, work, rows)
        else:
            third_party(eng, rep, args, work, project, rows)
        if not args.keep:
            shutil.rmtree(work / project, ignore_errors=True)
    for found in found_bounds:
        judge(rep, bound_row(found), lambda: measure_bound(eng, args, found))
    rep.extra.update(work=str(work), runs=args.runs, most_runs=args.most_runs, runtime_runs=args.runtime_runs,
                     slack=args.slack, noise=args.noise, critical_slack=args.critical_slack,
                     bounds=len(found_bounds))
    rep.finish()


# The self-test: no blaze, no cargo, no network.

SAMPLE = '''//! A crate.
use std::fmt;

#[inline]
fn helper<'a>(x: &'a str) -> &'a str { x }

fn main() -> Result<(), String> {
    let brace = '{';
    let quote = '"';
    let escaped = '\\'';
    let s = "a { string } with \\" quote";
    let raw = r#"raw { "inside" "#;
    let bytes = br#"{"#;
    let multi = "first line
  second line {";
    /* block /* nested { */ still } */
    let lifetime: &'static str = "x"; // a } in a comment
    if s.len() > 1 { println!("{}", helper(s)); }
    Ok(())
}

impl fmt::Display for Thing {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "}")
    }
}
'''
OPENING = "fn main() -> Result<(), String> {"
METHOD = "    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {"


def expect(holds: bool, what: str) -> None:
    if not holds:
        raise SystemExit(f"{SCRIPT} selftest: {what}")


def refused(what: str, fn: Callable[[], object], *kinds: type[BaseException]) -> str:
    """What `fn()` is refused with (one of `kinds`); the selftest stops when it is not refused."""
    try:
        fn()
    except kinds as e:
        return str(e)
    raise SystemExit(f"{SCRIPT} selftest: {what} is refused")


def selftest_select() -> None:
    """Which rows an input selects, and that nothing asked for is dropped."""
    keys: Callable[[Sequence[Project], Sequence[Budget]], list[str]] = lambda projects, budgets: [
        row.key for row in select(projects, budgets)]
    expect(select((), ()) == list(ROWS), "no projects and no budgets named: every row")
    expect(keys(("ripgrep",), ("noop", "memory")) == ["noop", "memory-ripgrep"], "ripgrep's no-op and memory rows")
    expect(keys((), ("body",)) == ["body"] and keys((), ("memory",)) == ["memory-nushell", "memory-ripgrep", "memory-kloc"],
           "a budget alone runs on every project that has a row for it")
    expect({row.project for row in select(("ruff",), ())} == {"ruff"} and keys(("ruff",), ()) == ["big-crate"],
           "a project alone runs every budget it has a row for")
    # The run of 2026-11-09: seven budgets asked of ripgrep, which has rows for two of them.
    asked: tuple[Budget, ...] = ("noop", "body", "comment", "format", "rename", "warm-start", "memory")
    said = refused("budgets with no row for ripgrep", lambda: select(("ripgrep",), asked), ValueError)
    for budget in ("body", "comment", "format", "rename", "warm-start"):
        expect(f"the budget {budget!r} has no row for ripgrep: its rows are nushell's" in said, f"{budget} is named: {said}")
    expect("'noop'" not in said and "'memory'" not in said, f"the budgets ripgrep has rows for are not named: {said}")
    said = refused("a project with no row for the budgets", lambda: select(("ripgrep", "ruff"), ("noop",)), ValueError)
    expect(said == "the project 'ruff' has no row for noop: its budgets are big-crate", said)
    expect(select((), ("bounds",)) == [] and keys(("blaze",), ("bounds", "ci")) == ["ci"], "bounds needs no project's row")
    refused("a project asked for bounds alone", lambda: select(("blaze",), ("bounds",)), ValueError)


# The daemon's rule for where it keeps things (storelib.Layout, which names the code), as a
# table: a home, then its socket, state, store, lock, log, profile history, build directories
# and fetched sources.
LAYOUTS = (
    ("/tmp/bz-budgets-x/ripgrep/home", "/tmp/bz-budgets-x/ripgrep/home/v2.sock", "/tmp/bz-budgets-x/ripgrep/home/v2",
     "/tmp/bz-budgets-x/ripgrep/home/v2/store", "/tmp/bz-budgets-x/ripgrep/home/v2/serve.lock",
     "/tmp/bz-budgets-x/ripgrep/home/v2/serve.log", "/tmp/bz-budgets-x/ripgrep/home/v2/profile",
     "/tmp/bz-budgets-x/ripgrep/home/build", "/tmp/bz-budgets-x/ripgrep/home/sources"),
    ("/b", "/b/v2.sock", "/b/v2", "/b/v2/store", "/b/v2/serve.lock", "/b/v2/serve.log", "/b/v2/profile", "/b/build",
     "/b/sources"),
    ("/c/blaze", "/c/blaze/v2.sock", "/c/blaze/v2", "/c/blaze/v2/store", "/c/blaze/v2/serve.lock",
     "/c/blaze/v2/serve.log", "/c/blaze/v2/profile", "/c/blaze/build", "/c/blaze/sources"),
)


def selftest_layout() -> None:
    """engine.layout against the daemon's rule, the environment that names a home and nothing
    else, a build's directory, a seeded store, and the preflight's reading of `blaze --help`."""
    for home, *rest in LAYOUTS:
        laid = engine.layout(Path(home))
        got = (laid.socket, laid.state, laid.store, laid.lock, laid.log, laid.profile, laid.builds, laid.sources)
        expect(laid.home == home and got == tuple(rest), f"the layout of the home {home}: {laid}")
        expect(engine.store_of(Path(home)) == Path(laid.store) and storelib.store_of(home) == laid.store
               and storelib.state_of(home) == laid.state, f"one layout, whoever asks: {home}")
    eng = engine.V2(engine.EngineSpec("v2", sys.executable))
    with tempfile.TemporaryDirectory() as tmp:
        top = Path(tmp).resolve()
        home, ws = top / "home", top / "my repo"
        env = eng.env(home, None, {"X": "1"})
        expect(env["BLAZE_HOME"] == str(home) and home.is_dir() and env["X"] == "1", "the environment names the home")
        for name in ("BLAZE_CACHE", "CARGO_TARGET_DIR", "XDG_RUNTIME_DIR", "BLAZE_BACKEND"):
            expect(name not in env, f"the environment sets no {name}: the daemon's home says where everything is")
        named = eng.env(home, top / "t")
        expect(named["CARGO_TARGET_DIR"] == str(top / "t") and eng.build_dir(ws, named) == top / "t",
               "a target directory named is the build directory")
        refused("a build directory before any build", lambda: eng.build_dir(ws, env), engine.Failed)
        builds = Path(engine.layout(home).builds)
        mine = builds / ("my_repo-" + "0123456789abcdef" * 2)
        for made in (mine, builds / ("my_repo-other-" + "0" * 32), builds / "my_repo-0123", builds / ("x-" + "0" * 32)):
            made.mkdir(parents=True)
        expect(engine.build_dir_name(ws) == "my_repo-" and eng.build_dir(ws, env) == mine,
               f"the default build directory of {ws} is the one named for it: {sorted(p.name for p in builds.iterdir())}")
        (builds / ("my_repo-" + "f" * 32)).mkdir()
        said = refused("two build directories for one name", lambda: eng.build_dir(ws, env), engine.Failed)
        expect("2 directories" in said, said)
        kept = top / "kept-store"
        kept.mkdir()
        (kept / "head").write_text("h")
        engine.seed_store(kept, home)
        expect((engine.store_of(home) / "head").read_text() == "h", "a seeded store is a copy at the home's store")
        engine.seed_store(None, home)
        expect(not engine.store_of(home).exists(), "seeded from no store, a home has none")
        refused("a seed that is no directory", lambda: engine.seed_store(top / "none", home), engine.Failed)
    helped = ("Commands:\n    build, b    Compile\n    tables --format=json   The size\n    debug sql QUERY   SQL\n"
              "    stop        Make\n    tables-of X\n")
    expect(all(engine.help_lists(helped, name) for name in ("build", "tables --format=json", "debug sql", "stop")),
           "the commands `blaze --help` lists are found")
    expect(not any(engine.help_lists(helped, name) for name in ("check", "tables --format=cbor", "debug live", "table", "trace --out PATH")),
           "a command `blaze --help` does not list, or lists another way, is not found")
    # The preflight against a binary that is no blaze: refused, naming what it assumed.
    said = refused("a binary that is not blaze", eng.preflight, engine.Failed)
    expect("preflight" in said and "--version" in said, said)
    expect(placed((0, "at /h/build/src-0/debug/app\n", "/h/build/src-0"), Path("/h/build/src-0"))
           == placed((0, "at /t/debug/app\n", "/t"), Path("/t")), "two build directories' programs say the same")
    expect(reached_args(("-p", "x", "--", "--nocapture")) == ["-p", "x", "--reached", "--message-format=json", "--",
                                                              "--nocapture"], "the CI budget's test arguments")
    line = ('{"reason": "blaze-test-result", "name": "t::%s", "binary": "b", "executable": 0, "outcome": "%s", '
            '"millis": 1, "outside": false, "output": "", "cached": %s}')
    kept_pass = '{"basis": "executed", "why": null, "millis": 2, "outcome": "passed"}'
    said_tests = "\n".join([line % ("a", "passed", "null"), line % ("b", "passed", kept_pass), line % ("c", "ignored", "null")])
    ran = engine.Ran(("blaze", "test"), 1.0, 0, said_tests, "")
    expect(tests_run("tests", ran) == 1, "of a run, a kept pass and an ignored test, one test ran")
    failed = engine.Ran(("blaze", "test"), 1.0, 101, line % ("a", "failed", "null"), "")
    expect("b: t::a" in refused("a failed test", lambda: tests_run("tests", failed), Wrong), "a failed test is named")


def selftest_smoke() -> None:
    """The smoke's input, and its edit on the workspace fixture.py writes: no blaze runs."""
    spec = '{"command": "smoke", "engine": {"kind": "v2", "blaze": "blaze"}}'
    given = parse(Input, spec, SCRIPT)
    expect(given.command == "smoke" and given.out == "" and not given.projects and not given.budgets, f"the smoke's input: {given}")
    refused("another command", lambda: parse(Input, spec.replace("smoke", "smokes"), SCRIPT), SystemExit)
    expect([row.key for row in SMOKE_ROWS] == ["noop", "body"] and all(row.limit == math.inf for row in SMOKE_ROWS),
           "the smoke's rows are a no-op build and a body edit, held to no budget")
    with tempfile.TemporaryDirectory() as tmp:
        made, ws = smoke_source(Path(tmp) / "src")
        path = ws / SMOKE_FILE
        before = read(path)
        expect(before.count(SMOKE_OPENING) == 1 and (ws / "Cargo.toml").is_file(),
               f"fixture.py's {SMOKE_FILE} opens the smoke's function once")
        changes: list[Change] = made["edits"]["body"]
        expect(write_edit(str(ws), varied(changes, 0), False) == 1 and read(path) != before, "the smoke's edit applies")
        expect(write_edit(str(ws), varied(changes, 0), True) == 1 and read(path) == before, "and is taken back")
        expect(f'name = "{SMOKE_PROGRAM}"' in read(ws / "app" / "Cargo.toml"), "the smoke's program is the fixture's")
    once = dataclasses.replace(given, runs=1, most_runs=1)
    expect(enough([3.0], math.inf, once) and not enough([], math.inf, once), "the smoke times each once")
    # The guards: the workspace's edits apply to fixture.py's file, every step that gave no facts
    # still gives each of its outcomes, and a run with every step gathered gives every name.
    with tempfile.TemporaryDirectory(prefix="bz-budgets-selftest-") as tmp:
        _, ws = smoke_source(Path(tmp) / "src")
        source = read(ws / SMOKE_FILE)
        expect(guards.eol_comment(source, SMOKE_OPENING).count("\n") == source.count("\n")
               and guards.body_statement(source, SMOKE_OPENING) != source, "the guards' edits apply to the workspace")
    rep = report.Report(SCRIPT, given.engine, "")
    facts: Facts = dict(guards.FIXED["comment"])
    some: list[Gathered] = [(fixture, step, None if step == "noop" else facts, "the daemon did not start")
                            for fixture in guards.FIXTURES for step in guards.STEPS]
    with contextlib.redirect_stdout(io.StringIO()):
        guard_outcomes(rep, some, ())
    names = [o.name for o in rep.outcomes]
    expect(sorted(names) == sorted(set(guards.names()) - {guards.PROBE}),
           "the guards' outcomes are every bound of every step of both fixtures, with or without facts")
    expect({o.verdict for o in rep.outcomes if ": noop: " in o.name} == {"error"}, "a step with no facts errs, bound by bound")
    refused("`record` without the smoke", lambda: main(parse(Input, spec.replace('"smoke"', '"run"')[:-1]
                                                              + ', "record": true, "out": "/tmp/x"}', SCRIPT)), SystemExit)
    got = timed([3.0], "ms")
    expect(verdict(got.value, math.inf, True) == "pass" and got.interval is None, "one sample is a number, and passes no budget")


def selftest_work() -> None:
    """The work directory: probed for what blaze needs of its file system, chosen where the
    probe passes, refused by name where it does not."""
    bases = storelib.work_bases()
    expect(bases[0] == "/tmp" and bases[1].endswith("blaze-harness"), f"/tmp, then the cache directory: {bases}")
    with tempfile.TemporaryDirectory() as tmp:
        why = storelib.cannot_clone(tmp)
        expect(os.listdir(tmp) == [], "the probe leaves nothing behind")
        named = Path(tmp) / "given"
        if why is None:
            expect(engine.work_dir(str(named), "x-") == named.resolve() and named.is_dir(), "a directory that clones is taken")
        else:
            said = refused("a work directory that cannot clone", lambda: engine.work_dir(str(named), "x-"), engine.Failed)
            expect(str(named.resolve()) in said and "FICLONE" in said and why in said, said)
    clones = [base for base in bases if os.path.isdir(base) and storelib.cannot_clone(base) is None]
    if clones:
        made = engine.work_dir("", "bz-selftest-")
        expect(made.is_dir() and str(made.parent) == os.path.realpath(clones[0]), f"a new one in {clones[0]}: {made}")
        made.rmdir()
    else:
        said = refused("no base that clones", lambda: engine.work_dir("", "bz-selftest-"), engine.Failed)
        expect(all(base in said for base in bases) and "give `work`" in said, said)


def selftest_ledger() -> None:
    """Every check of the objectives ledger (docs/v2/plan/objectives.json) that runs this script
    gives it an input it takes and rows it has: a row naming a budget that is gone, or a pair
    with no row, is found here and not when the ledger's runner gets to it."""
    ledger = json.loads((engine.ROOT / "docs/v2/plan/objectives.json").read_text())
    checks = [(row["id"], check) for row in ledger["rows"] for check in row["checks"]
              if check["script"] == "harness/v2/budgets.py"]
    expect(len(checks) > 0, "the ledger has rows this script checks")
    for row_id, check in checks:
        given = dict(check["input"] or {}, engine={"kind": "v2", "blaze": "blaze"}, out="o.json")
        expect(not check["command"] and set(check["takes"]) == {"engine", "out"},
               f"the ledger's {row_id}: this script takes one JSON value with `engine` and `out`, not {check}")
        try:
            it = parse(Input, json.dumps(given), SCRIPT)
            chosen = select(it.projects, it.budgets)
        except (SystemExit, ValueError) as e:
            raise SystemExit(f"{SCRIPT} selftest: the ledger's {row_id} gives an input this script refuses: {e}") from None
        # The smoke's rows are its own (`SMOKE_ROWS`, `guards.names()`): it takes no `budgets`.
        expect((it.command == "smoke" and not it.projects and not it.budgets)
               or (it.command == "run" and (bool(chosen) or "bounds" in it.budgets)),
               f"the ledger's {row_id} selects no row: {given}")


def selftest() -> None:
    f = locate(SAMPLE, OPENING)
    expect(SAMPLE[f.close:].startswith("}\n\nimpl"), f"the end of main is found: {SAMPLE[f.close:f.close + 12]!r}")
    expect(SAMPLE[f.body:].startswith("    let brace"), "the body starts on the line after the opening")
    m = locate(SAMPLE, METHOD)
    expect(SAMPLE[m.close - 4:m.close + 1] == "    }", "a `}` in a string does not close the method")
    lines = SAMPLE.count("\n")
    for label, edited, more in (
        ("blank lines", blank_lines(SAMPLE, OPENING), 2),
        ("more blank lines", blank_lines(SAMPLE, OPENING, 2), 6),
        ("re-indentation", reindent(SAMPLE, OPENING), 0),
        ("a comment line", comment_line(SAMPLE, OPENING), 1),
        ("an inline comment", inline_comment(SAMPLE, OPENING), 0),
    ):
        expect(tokens(edited) == tokens(SAMPLE), f"{label} changes only whitespace and comments")
        expect(edited != SAMPLE, f"{label} changes the text")
        expect(edited.count("\n") == lines + more, f"{label} adds {more} lines, not {edited.count(chr(10)) - lines}")
    expect("\n  second line {" in reindent(SAMPLE, OPENING), "re-indentation leaves a string's second line as it is")
    expect("\n        let brace" in reindent(SAMPLE, OPENING), "re-indentation indents the body")
    expect(reindent(SAMPLE, OPENING, 1) != reindent(SAMPLE, OPENING), "re-indentation varies with the run")
    inline = inline_comment(SAMPLE, OPENING)
    expect([a for a, b in zip(SAMPLE.splitlines(), inline.splitlines()) if a != b] == [OPENING],
           "an inline comment changes the opening line alone")
    expect(comment_line(SAMPLE, OPENING) != comment_line(SAMPLE, OPENING, 1), "a comment varies with the run")
    base = with_item(SAMPLE, OPENING)
    expect(base.endswith(ITEM) and base.count(NAMED) == 2, "the function and its call are added")
    first, second = renamed(base), renamed(base, 1)
    expect(NAMED not in first and first.count("blaze_v2_renamed_0") == 2 and first != second, "a rename renames both")
    expect(len(tokens(first)) == len(tokens(base)), "a rename changes names alone")
    away = moved(base, OPENING)
    expect(sorted(tokens(away)) == sorted(tokens(base)) and away != base, "a move moves the function's text")
    expect(away.index(NAMED + "(x") < away.index("impl fmt::Display"), "the function moves next to main")
    try:
        moved(with_item(SAMPLE, METHOD), METHOD)
        expect(False, "a move next to an indented function is refused")
    except ValueError:
        pass
    try:
        locate(SAMPLE, "Ok(())")
        expect(False, "an anchor that opens no body is refused")
    except ValueError:
        pass
    for name in ("nushell", "ripgrep", "ruff"):
        expect("body" in PROJECTS[name]["edits"], f"bench.PROJECTS' {name} has a body edit")
        for kind in ("body", "signature"):
            changes: list[Change] | None = PROJECTS[name]["edits"].get(kind)
            if changes is None:
                continue
            runs = [varied(changes, n) for n in range(4)]
            expect(all([old for _, old, _ in run] == [old for _, old, _ in changes] for run in runs),
                   f"{name} {kind}: a varied edit finds the same text")
            expect(len({tuple(new for _, _, new in run) for run in runs}) == 4, f"{name} {kind}: every run differs")
    expect("signature" in PROJECTS["nushell"]["edits"] and "runtime" in PROJECTS["ripgrep"],
           "nushell has a signature edit and ripgrep a runtime workload")
    expect(len({row.key for row in ROWS}) == len(ROWS) and len({row.name for row in ROWS}) == len(ROWS),
           "row keys and names are unique")
    expect({row.budget for row in ROWS} == set(get_args(Budget)) - {"bounds"},
           "every budget but bounds (whose rows are docs/v2/plan/bounds.md's) has a row")
    expect({row.project for row in ROWS} == set(get_args(Project)), "every project has a row")
    selftest_select()
    selftest_layout()
    selftest_smoke()
    selftest_ledger()
    selftest_work()
    expect(verdict(30, 30, True) == "pass" and verdict(30.01, 30, True) == "fail", "at most")
    expect(verdict(0.8, 0.8, False) == "pass" and verdict(0.79, 0.8, False) == "fail", "at least")
    expect(describe(12.5, 30, True, "ms") == "12.5 ms against a budget of at most 30 ms", describe(12.5, 30, True, "ms"))
    before = cpu_times("cpu  100 0 50 800 50 0 0 0 0 0\ncpu0 50 0 25 400 25 0 0 0 0 0\n")
    after = cpu_times("cpu  400 0 150 1000 50 0 0 0 7 0\ncpu0 200 0 75 500 25 0 0 0 7 0\n")
    expect(before == CpuTimes(150, 1000) and after == CpuTimes(550, 1600), f"/proc/stat read: {before}, {after}")
    expect(abs(busy_share(before, after) - 400 / 600) < 1e-9, "the busy share of two readings")
    try:
        busy_share(after, after)
        expect(False, "readings with no ticks between are refused")
    except ValueError:
        pass
    perf = "# started on Thu\n\n1234,,cache-misses:u,100.00,,\n98765,,cache-references,100.00,,\n"
    expect(perf_counts(perf) == {"cache-misses": 1234, "cache-references": 98765}, f"perf stat read: {perf_counts(perf)}")
    try:
        perf_counts("<not supported>,,cache-misses,0,,\n1,,cache-references,1,,\n")
        expect(False, "a counter the machine lacks is refused")
    except engine.Failed:
        pass
    expect(median_interval([1.0, 2, 3, 4, 5]) is None, "5 samples are too few for a 95% interval")
    expect(median_interval([6.0, 1, 5, 2, 4, 3]) == (1, 6), "6 samples: the interval is their range (96.9%)")
    tens = [float(x) for x in (9, 2, 7, 4, 1, 10, 3, 8, 5, 6)]
    expect(median_interval(tens) == (2, 9), f"10 samples: the 2nd to the 9th (97.9%), not {median_interval(tens)}")
    expect(median_interval([float(x) for x in range(1, 21)]) == (6, 15), "20 samples: the 6th to the 15th (95.9%)")
    expect(outliers([10.0, 10.2, 9.9, 10.1, 10.0, 30.0]) == 1, "one slow run among steady ones is an outlier")
    expect(outliers([10.0, 11, 12, 13, 14]) == 0 and outliers([5.0, 5, 5, 6]) == 1 and outliers([]) == 0,
           "no outlier in an even spread; with a MAD of 0, every sample off the median")
    got = timed(tens, "ms", places=3)
    expect(got.value == 5.5 and got.interval == (2, 9) and got.numbers["places"] == 3
           and got.numbers["median_low_ms"] == 2 and got.numbers["median_high_ms"] == 9
           and got.numbers["min_ms"] == 1 and got.numbers["max_ms"] == 10 and got.numbers["outliers"] == 0,
           f"timed: {got}")
    expect("median_low_ms" not in timed([1.0, 2, 3], "ms").numbers, "no interval is reported from 3 runs")
    expect(set(TABLES.values()) == {"token", "syntax-node", "hir-expr", "mir-statement", "memo", "memo-edge",
                                    "relocation"}, f"the tables of docs/v2/plan/design/data.md: {TABLES}")
    with tempfile.TemporaryDirectory() as tmp:
        home = Path(tmp)
        lock = Path(engine.layout(home).lock)
        lock.parent.mkdir()
        lock.write_text(str(os.getpid()))
        expect(daemon_pid(home) == os.getpid(), "the daemon's pid from its lock")
        for said in (f"{os.getpid()} stopped", "", "not a pid"):
            lock.write_text(said)
            try:
                daemon_pid(home)
                expect(False, f"a lock naming no daemon that runs is refused: {said!r}")
            except engine.Failed:
                pass
    expect(restart_limit(12.0, 100_000, 5.0, True) == 67.0, "the restart budget with Linux's statx sweep")
    expect(restart_limit(12.0, 100_000, 5.0, False) == 17.0, "the restart budget where a journal survives")
    restarts = {row.key: row for row in ROWS if row.budget == "restart"}
    expect(set(restarts) == {f"restart-first-answer-{how}" for how in get_args(Restart)},
           f"a restart row for each way the daemon stops: {sorted(restarts)}")
    expect(all(row.project == "blaze" and row.at_most and row.unit == "ms" for row in restarts.values()),
           "the restart rows: blaze, at most, in ms")
    spec = '{"engine": {"kind": "v2", "blaze": "blaze"}, "out": "o.json"'
    given = parse(Input, spec + ', "budgets": ["restart"], "restart_slack_ms": 3.5}', SCRIPT)
    expect(given.budgets == ("restart",) and given.restart_slack_ms == 3.5, f"the restart input: {given}")
    plain = parse(Input, spec + "}", SCRIPT)
    expect(plain.restart_slack_ms == 5.0 and not plain.activation, "5 ms of slack and no activation unless given")
    expect(parse(Input, spec + ', "activation": true}', SCRIPT).activation, "the restart budget socket-activated")
    expect([runs_for(cv) for cv in (0.0, 0.01, 0.04, 0.05, 0.07, 0.1)] == [6, 6, 6, 7, 12, 25],
           f"runs for an interval 10% wide: {[runs_for(cv) for cv in (0.0, 0.01, 0.04, 0.05, 0.07, 0.1)]}")
    few = parse(Input, spec + ', "runs": 6, "most_runs": 20}', SCRIPT)
    expect(not enough([10.0] * 5, 10.0, few) and enough([10.0] * 6, 10.0, few), "6 runs at least, then a narrow interval")
    wide = [5.0, 15.0] * 3
    expect(not enough(wide, 10.0, few), "an interval wider than 10% that takes the budget in needs more runs")
    expect(enough(wide * 4, 10.0, few) and len(wide * 4) >= few.most_runs, "most_runs ends it, settled or not")
    expect(enough([1.0, 1, 1, 1, 1, 2], 10.0, few), "an interval that leaves the budget out settles it")
    expect(not known([9.0, 11.0] * 5, 0.05, 6, 20) and known([10.0] * 6, 0.05, 6, 20),
           "a median known to 5% of itself")
    expect(engine.text(b"a\r\nb\rc\n\xff") == "a\nb\nc\n\ufffd", "a pipe's bytes decoded as text=True decodes them")
    floor = harness_floor_ms(Path.cwd(), engine.scrubbed())
    expect(0 < floor < 1000, f"the harness's floor, `true` run through engine.run, is under a second: {floor} ms")
    got_runs = timed([10.0, 10.2, 9.8, 10.1, 9.9, 10.0], "ms").numbers
    expect(got_runs["runs"] == 6 and got_runs["runs_needed"] == 6, f"timed says the runs taken and needed: {got_runs}")
    try:
        parse(Input, spec + ', "most_runs": 5.0}', SCRIPT)
        expect(False, "most_runs is a whole number")
    except SystemExit:
        pass
    for bad in (', "restart_slack_ms": 5}', ', "budgets": ["restart-clean"]}', ', "activation": 1}'):
        try:
            parse(Input, spec + bad, SCRIPT)
            expect(False, f"the input {bad!r} is refused")
        except SystemExit:
            pass
    child = subprocess.Popen([sys.executable, "-c", ""])
    child.wait()
    expect(alive(os.getpid()) and not alive(child.pid), "a running process is alive and a reaped one not")
    if hasattr(os, "killpg"):
        try:
            kill_group(os.getpid())
            expect(False, "a SIGKILL to the harness's own process group is refused")
        except engine.Failed:
            pass
    print(f"{SCRIPT} selftest: ok")


if __name__ == "__main__":
    if len(sys.argv) != 2:
        sys.exit(f"usage: {SCRIPT} '<json>' | selftest\n\n{__doc__}")
    if sys.argv[1] == "selftest":
        selftest()
    else:
        main(parse(Input, sys.argv[1], SCRIPT))
