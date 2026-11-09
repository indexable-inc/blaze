"""incremental.py: what an incremental `blaze check` of blaze's own workspace re-executes, held to
bounds by count, so a regression of the whole class (memory never given back, a forecast that
serialises the scheduler, a cache key over a volatile variable, proc-macro output marked
unknown, cycle results never reused) fails CI the day it lands rather than 40 minutes into
someone's edit loop. Linux and macOS (the daemon's Unix socket).

    uv run --frozen python harness/incremental.py '{"blaze": "target/release/blaze"}'

One daemon, a home of its own, five `blaze check ARGS` of the workspace in order:

1. warm: the first check (cold in a fresh home). Only its counts are reported.
2. no-op: the same check again, nothing changed, from another shell: another `TMPDIR`, `OLDPWD`,
   `SHLVL` and `PWD` (run from `noise_cwd` inside the workspace), and a variable the daemon has
   never seen. It must re-execute nothing: no plane node ran (`builds.misses` is 0), no query
   function ran (`query_counts.executed` sums to 0), no `misses` row. So a key over a
   variable that changes between shells is caught here.
3. restart: the daemon killed with SIGKILL, and the check again: the next daemon restores its
   image (`restored the daemon image` in `<home>/v2/serve.log`, no `starts empty`) and
   re-executes nothing, as in 2.
4. edit: `edit.find` replaced by `edit.replace` in `edit.file`, a body edit of a leaf function
   of `edit.package` that other members depend on. At most `edit.max_ran` plane nodes and
   `edit.max_executed` query runs, and no `misses` row of an item outside `edit.package`: a
   crate downstream whose signatures did not change verifies, it does not run again.
5. back: the edit taken back (the file's bytes restored, as they are on any failure), and the
   check again, under the same bounds as 4.

Daemon heap: the daemon's `live bytes` counter (blaze_mem) at the end of each check, and its
peak during it. The no-op's end may exceed the warm check's by at most `heap_growth_bytes`, and
so may back's exceed the no-op's (memory an edit took is given back once it is undone).

Every count is read from the profile history with `blaze debug sql --json`, which runs the
DuckDB `BLAZE_DUCKDB` names (`duckdb`, or the flake's `.#duckdb` built with nix when null): the
check's `builds` row (plane nodes that ran, settled and joined, its tree digest and the blaze
that ran it), its `query_counts` rows (each query function's runs, and `written`, each input's
fields its setters wrote: what the build changed) and its `misses` rows (each item computed
again, the step, the cause and the read or key part that changed). The bounds are counts, so the
verdict does not depend on the machine; wall times are reported beside them.

On failure it prints, for each check that broke a bound and in one pass, the query functions
that ran most, the inputs that were written (the cause of every run after them), and every
`misses` row (the item, its step, why, and what changed), then exits 1.

Input, one JSON value (inline, a file or `-`); every field but `blaze` has a default:

    {"blaze": "/path/blaze", "workspace": null, "home": null, "duckdb": null,
     "args": ["--workspace"], "noise_cwd": "crates",
     "edit": {"package": "blaze-format", "file": "crates/blaze-format/src/tiers.rs",
              "find": "...", "replace": "...", "max_ran": 16, "max_executed": 5000},
     "heap_growth_bytes": 67108864, "top": 20, "cold_s": 5400, "step_s": 1800,
     "history_s": 120, "keep_home": false}

`workspace` defaults to this checkout, edited in place and restored; `home` to a new directory
under /tmp (a socket path must be short on macOS), removed when the run passes unless
`keep_home`.
"""

import asyncio
import json
import os
import re
import shutil
import signal
import sys
import tempfile
import time
from collections.abc import Mapping
from dataclasses import dataclass, field
from pathlib import Path
from typing import cast

import storelib
from typed_input import fail, parse

SCRIPT = "incremental.py"
ROOT = Path(__file__).resolve().parent.parent
# Read by blaze or cargo from the caller's environment; set here alone, or not at all.
SCRUBBED = (
    "CARGO_TARGET_DIR",
    "CARGO_INCREMENTAL",
    "RUSTFLAGS",
    "CARGO_ENCODED_RUSTFLAGS",
    "RUSTC",
    "RUSTC_WRAPPER",
    "BLAZE_HOME",
    "XDG_RUNTIME_DIR",
    "BLAZE_DUCKDB",
)


@dataclass(frozen=True)
class Edit:
    # The package the edited file is of: a `misses` row of any other is a crate that ran again.
    package: str = "blaze-format"
    file: str = "crates/blaze-format/src/tiers.rs"
    # Found exactly once in `file`, and replaced: a statement added to a private leaf function,
    # which changes its body and no signature.
    find: str = "fn tail_words(len: usize, bits: u32) -> usize {\n"
    replace: str = "fn tail_words(len: usize, bits: u32) -> usize {\n    let _ = len;\n"
    # Plane nodes (`builds.misses`) and query runs (`query_counts.executed`, summed) the edited
    # check, and the one after it is undone, may take.
    max_ran: int = 16
    max_executed: int = 5000


@dataclass(frozen=True)
class Input:
    blaze: str
    workspace: str | None = None
    home: str | None = None
    duckdb: str | None = None
    args: tuple[str, ...] = ("--workspace",)
    noise_cwd: str = "crates"
    edit: Edit = field(default_factory=Edit)
    heap_growth_bytes: int = 64 << 20
    top: int = 20
    cold_s: float = 5400.0
    step_s: float = 1800.0
    history_s: float = 120.0
    keep_home: bool = False


class Failed(Exception):
    """A step that could not run: its name, the command, its exit code and its stderr's tail."""


@dataclass(frozen=True)
class Ran:
    code: int
    stdout: str
    stderr: str
    seconds: float


async def run(step: str, argv: list[str], cwd: Path, env: Mapping[str, str], bound_s: float) -> Ran:
    """`argv` in `cwd`, bounded by `bound_s`: its exit code, output and wall time. A command
    that cannot start or runs past its bound is a `Failed` naming the step."""
    began = time.monotonic()
    try:
        process = await asyncio.create_subprocess_exec(
            *argv, cwd=cwd, env=dict(env), stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE
        )
    except OSError as error:
        raise Failed(f"{step}: starting {argv!r} in {cwd}: {error}") from None
    try:
        async with asyncio.timeout(bound_s):
            out, err = await process.communicate()
    except TimeoutError:
        process.kill()
        _, err = await process.communicate()
        raise Failed(
            f"{step}: {argv!r} ran past its bound of {bound_s:.0f} s (killed, exit {process.returncode}); "
            f"stderr's tail:\n{err.decode(errors='replace')[-3000:]}"
        ) from None
    return Ran(
        process.returncode or 0,
        out.decode(errors="replace"),
        err.decode(errors="replace"),
        time.monotonic() - began,
    )


type Row = dict[str, object]


def number(row: Row, key: str) -> int | None:
    value = row.get(key)
    if value is None:
        return None
    if isinstance(value, bool) or not isinstance(value, int | float):
        raise Failed(f"the history's column {key} is {value!r}, not a number, in {row}")
    return int(value)


def text(row: Row, key: str) -> str | None:
    value = row.get(key)
    return None if value is None else str(value)


@dataclass(frozen=True)
class QueryCount:
    query: str
    executed: int
    verified: int
    written: int


@dataclass(frozen=True)
class Miss:
    step: str
    item: str
    cause: str
    what: str | None
    names: str | None


@dataclass(frozen=True)
class Check:
    """One check as the history has it."""

    label: str
    wall_s: float
    code: int
    build: int
    process: int | None
    tree: str | None
    blaze: str | None
    ran: int | None
    settled: int | None
    joined: int | None
    dur_ns: int | None
    counts: tuple[QueryCount, ...]
    misses: tuple[Miss, ...]
    heap_end: int | None
    heap_peak: int | None

    @property
    def executed(self) -> int:
        return sum(it.executed for it in self.counts)

    @property
    def written(self) -> int:
        return sum(it.written for it in self.counts)


class Gate:
    def __init__(self, it: Input, workspace: Path, home: Path, duckdb: str):
        self.it = it
        self.workspace = workspace
        self.home = home
        base = {k: v for k, v in os.environ.items() if k not in SCRUBBED}
        self.env = {**base, "BLAZE_HOME": str(home), "BLAZE_DUCKDB": duckdb, "RUST_BACKTRACE": "0"}

    async def sql(self, what: str, query: str) -> list[Row]:
        """`query` over the home's profile history, a row a dict."""
        ran = await run(
            f"reading {what} from the history",
            [self.it.blaze, "debug", "sql", "--json", query],
            self.workspace,
            self.env,
            120,
        )
        if ran.code != 0:
            raise Failed(
                f"reading {what}: `blaze debug sql --json` exited {ran.code}: {query}\n{ran.stderr[-3000:]}"
            )
        rows: list[Row] = []
        for line in ran.stdout.splitlines():
            if line.strip():
                value: object = json.loads(line)
                if not isinstance(value, dict):
                    raise Failed(f"reading {what}: a line of `blaze debug sql --json` is no object: {line}")
                rows.append(dict(cast(dict[str, object], value)))
        return rows

    async def last_build(self) -> int:
        rows = await self.sql(
            "the last build", "SELECT CAST(coalesce(max(build_id), 0) AS UBIGINT) AS last FROM builds"
        )
        return (number(rows[0], "last") or 0) if rows else 0

    async def ended_after(self, after: int, label: str) -> Row:
        """The `builds` row of the first check that ended after build `after`, waited for up to
        `history_s`: the history writes it on its own thread once the build ends."""
        query = (
            "SELECT build_id, CAST(command AS VARCHAR) AS command, CAST(outcome AS VARCHAR) AS outcome, "
            "hits, misses, joined, started_ns, dur_ns, tree, blaze, process FROM builds "
            f"WHERE build_id > {after} AND dur_ns IS NOT NULL ORDER BY build_id"
        )
        deadline = time.monotonic() + self.it.history_s
        print(f"{label}: waiting for the history's row of the check after build {after}", flush=True)
        while True:
            rows = [
                r
                for r in await self.sql(f"{label}'s build", query)
                if (text(r, "command") or "").startswith("check")
            ]
            if rows:
                return rows[-1]
            if time.monotonic() > deadline:
                raise Failed(
                    f"{label}: no check after build {after} ended in the history within {self.it.history_s:.0f} s"
                )
            await asyncio.sleep(0.5)

    async def check(self, label: str, bound_s: float, noisy: bool) -> Check:
        after = await self.last_build()
        env, cwd = dict(self.env), self.workspace
        if noisy:
            tmp = tempfile.mkdtemp(prefix="rj-noise-")
            cwd = self.workspace / self.it.noise_cwd
            env.update(
                {
                    "TMPDIR": tmp,
                    "OLDPWD": "/",
                    "SHLVL": str(int(env.get("SHLVL", "1") or "1") + 3),
                    "PWD": str(cwd),
                    "INCREMENTAL_GATE_NOISE": os.urandom(8).hex(),
                }
            )
        else:
            env.update({"OLDPWD": str(self.workspace), "PWD": str(cwd)})
        ran = await run(f"{label}: blaze check", [self.it.blaze, "check", *self.it.args], cwd, env, bound_s)
        print(f"{label}: `blaze check` exited {ran.code} after {ran.seconds:.1f} s", flush=True)
        row = await self.ended_after(after, label)
        build = number(row, "build_id") or 0
        started, dur = number(row, "started_ns"), number(row, "dur_ns")
        process = number(row, "process")
        counts_q = (
            "SELECT CAST(query AS VARCHAR) AS query, CAST(sum(executed) AS UBIGINT) AS executed, "
            "CAST(sum(verified) AS UBIGINT) AS verified, CAST(sum(coalesce(written, 0)) AS UBIGINT) AS written "
            f"FROM query_counts WHERE build_id = {build} GROUP BY 1 ORDER BY 2 DESC, 4 DESC, 1"
        )
        misses_q = (
            "SELECT CAST(step AS VARCHAR) AS step, item, CAST(cause AS VARCHAR) AS cause, what, names "
            f"FROM misses WHERE build_id = {build} ORDER BY 1, 2"
        )
        end = (started or 0) + (dur or 0)
        of = f" AND process = {process}" if process is not None else ""
        heap_q = (
            "SELECT CAST(arg_max(value, t_ns) AS BIGINT) AS heap_end, "
            f"CAST(max(value) FILTER (WHERE t_ns >= {started or 0}) AS BIGINT) AS heap_peak "
            f"FROM counters WHERE name = 'live bytes' AND t_ns <= {end}{of}"
        )
        async with asyncio.TaskGroup() as group:
            counts_t = group.create_task(self.sql(f"{label}'s query counts", counts_q))
            misses_t = group.create_task(self.sql(f"{label}'s misses", misses_q))
            heap_t = group.create_task(self.sql(f"{label}'s heap", heap_q))
        counts = tuple(
            QueryCount(
                text(r, "query") or "?",
                number(r, "executed") or 0,
                number(r, "verified") or 0,
                number(r, "written") or 0,
            )
            for r in counts_t.result()
        )
        misses = tuple(
            Miss(
                text(r, "step") or "?",
                text(r, "item") or "?",
                text(r, "cause") or "?",
                text(r, "what"),
                text(r, "names"),
            )
            for r in misses_t.result()
        )
        heap = heap_t.result()
        return Check(
            label=label,
            wall_s=ran.seconds,
            code=ran.code,
            build=build,
            process=process,
            tree=text(row, "tree"),
            blaze=text(row, "blaze"),
            ran=number(row, "misses"),
            settled=number(row, "hits"),
            joined=number(row, "joined"),
            dur_ns=dur,
            counts=counts,
            misses=misses,
            heap_end=number(heap[0], "heap_end") if heap else None,
            heap_peak=number(heap[0], "heap_peak") if heap else None,
        )

    def serve_log(self) -> str:
        path = Path(storelib.layout(str(self.home)).log)
        try:
            return path.read_text(errors="replace")
        except OSError as error:
            raise Failed(f"restart: reading {path}: {error}") from None

    async def kill(self, pid: int) -> None:
        """SIGKILL to the daemon, waited for (a bounded wait, named)."""
        try:
            os.kill(pid, signal.SIGKILL)
        except ProcessLookupError:
            raise Failed(f"restart: the daemon (pid {pid}) the last check ran in is not running") from None
        print(f"restart: waiting for the daemon (pid {pid}) to end after SIGKILL", flush=True)
        async with asyncio.timeout(60):
            while True:
                try:
                    os.kill(pid, 0)
                except ProcessLookupError:
                    return
                await asyncio.sleep(0.1)


def mib(value: int | None) -> str:
    return "?" if value is None else f"{value / (1 << 20):.0f} MiB"


def summary(check: Check) -> str:
    return (
        f"{check.label:8} build {check.build}: exit {check.code}, {check.wall_s:7.1f} s wall, "
        f"plane nodes ran {check.ran} settled {check.settled} joined {check.joined}, "
        f"queries executed {check.executed}, inputs written {check.written}, misses {len(check.misses)}, "
        f"heap end {mib(check.heap_end)} peak {mib(check.heap_peak)}"
    )


def detail(check: Check, top: int) -> str:
    """What a check re-executed and why: the query functions that ran most, the inputs written,
    every misses row."""
    lines = [f"{check.label} (build {check.build}, tree {check.tree}, blaze {check.blaze}):"]
    ran = [it for it in check.counts if it.executed]
    lines.append(f"  query functions that ran, most first ({len(ran)}, the top {top}):")
    lines += [f"    {it.executed:9} runs  {it.verified:9} verified  {it.query}" for it in ran[:top]]
    written = sorted((it for it in check.counts if it.written), key=lambda it: (-it.written, it.query))
    lines.append(f"  inputs written, what every run traces back to ({len(written)}):")
    lines += [f"    {it.written:9} writes  {it.query}" for it in written[:top]] or ["    none"]
    lines.append(f"  computed again, and why ({len(check.misses)} misses rows):")
    for it in check.misses:
        why = f": {it.what}" if it.what else ""
        of = f" (of {it.names})" if it.names else ""
        lines.append(f"    {it.step}: {it.item}: {it.cause}{why}{of}")
    return "\n".join(lines)


def zero_work(check: Check) -> list[str]:
    said: list[str] = []
    if check.ran is None:
        said.append(f"{check.label}: its builds row has no count of plane nodes that ran")
    elif check.ran:
        said.append(f"{check.label}: {check.ran} plane nodes ran, none expected")
    if check.executed:
        said.append(f"{check.label}: {check.executed} query runs, none expected")
    if check.misses:
        said.append(f"{check.label}: {len(check.misses)} things computed again, none expected")
    return said


def owned(item: str, package: str) -> bool:
    """Whether a misses row's item is of `package`: a unit (`pkg v0.1.0 (lib)`), its build
    script's run, or a path in its crate (`pkg_name::...`)."""
    krate = package.replace("-", "_")
    return item.startswith((f"{package} ", f"{krate}::", f"<{krate}::")) or item in (package, krate)


def bounded(check: Check, edit: Edit) -> list[str]:
    said: list[str] = []
    if check.ran is None:
        said.append(f"{check.label}: its builds row has no count of plane nodes that ran")
    elif check.ran > edit.max_ran:
        said.append(f"{check.label}: {check.ran} plane nodes ran, over the bound of {edit.max_ran}")
    if check.executed > edit.max_executed:
        said.append(f"{check.label}: {check.executed} query runs, over the bound of {edit.max_executed}")
    foreign = [it for it in check.misses if not owned(it.item, edit.package)]
    if foreign:
        names = ", ".join(sorted({f"{it.step}: {it.item}" for it in foreign})[:8])
        said.append(
            f"{check.label}: {len(foreign)} things outside {edit.package} computed again, none expected: {names}"
        )
    return said


def grew(before: Check, after: Check, bound: int) -> list[str]:
    if before.heap_end is None or after.heap_end is None:
        return [
            f"{after.label}: no `live bytes` sample of the daemon at the end of {before.label} or {after.label}"
        ]
    if after.heap_end - before.heap_end > bound:
        return [
            (
                f"{after.label}: the daemon's heap at its end is {mib(after.heap_end)}, "
                f"{mib(after.heap_end - before.heap_end)} over {before.label}'s {mib(before.heap_end)} "
                f"(the bound is {mib(bound)})"
            )
        ]
    return []


async def duckdb_of(it: Input) -> str:
    if it.duckdb is not None:
        return it.duckdb
    ran = await run(
        "the flake's DuckDB",
        ["nix", "build", "--no-link", "--print-out-paths", f"{ROOT}#duckdb"],
        ROOT,
        os.environ,
        1800,
    )
    if ran.code != 0 or not ran.stdout.strip():
        raise Failed(f"the flake's DuckDB: `nix build .#duckdb` exited {ran.code}:\n{ran.stderr[-3000:]}")
    return str(Path(ran.stdout.strip().splitlines()[-1]) / "bin" / "duckdb")


async def gate(it: Input, workspace: Path, home: Path) -> list[str]:
    duckdb = await duckdb_of(it)
    g = Gate(it, workspace, home, duckdb)
    edited = workspace / it.edit.file
    try:
        original = edited.read_bytes()
    except OSError as error:
        raise Failed(f"edit: reading {edited}: {error}") from None
    source = original.decode()
    if source.count(it.edit.find) != 1:
        raise Failed(
            f"edit: {edited} has {source.count(it.edit.find)} copies of edit.find, not one: {it.edit.find!r}"
        )

    failed: list[str] = []
    checks: list[Check] = []
    warm = await g.check("warm", it.cold_s, noisy=False)
    checks.append(warm)
    noop = await g.check("no-op", it.step_s, noisy=True)
    checks.append(noop)
    failed += zero_work(noop) + grew(warm, noop, it.heap_growth_bytes)

    if noop.process is None:
        raise Failed("restart: the no-op's builds row names no daemon process to kill")
    await g.kill(noop.process)
    log_before = len(g.serve_log())
    restart = await g.check("restart", it.step_s, noisy=False)
    checks.append(restart)
    said = g.serve_log()[log_before:].splitlines()
    if not any("restored the daemon image" in line for line in said):
        failed.append("restart: the daemon after SIGKILL did not log `restored the daemon image`")
    failed += [f"restart: {line}" for line in said if "starts empty" in line]
    failed += zero_work(restart)

    try:
        edited.write_text(source.replace(it.edit.find, it.edit.replace))
        edit = await g.check("edit", it.step_s, noisy=False)
    finally:
        edited.write_bytes(original)
    checks.append(edit)
    back = await g.check("back", it.step_s, noisy=False)
    checks.append(back)
    failed += bounded(edit, it.edit) + bounded(back, it.edit) + grew(noop, back, it.heap_growth_bytes)
    if edit.tree is not None and edit.tree == restart.tree:
        failed.append(f"edit: its tree digest is the unedited one ({edit.tree}): the edit was not seen")
    if back.tree is not None and restart.tree is not None and back.tree != restart.tree:
        failed.append(f"back: its tree digest {back.tree} is not the unedited {restart.tree}")
    failed += [
        f"{c.label}: `blaze check` exited {c.code}, the warm check {warm.code}"
        for c in checks[1:]
        if c.code != warm.code
    ]
    if warm.code != 0:
        print(f"note: the warm check exited {warm.code}; the others are held to the same exit", flush=True)

    print("\n".join(summary(c) for c in checks), flush=True)
    broke = {line.split(":", 1)[0] for line in failed}
    for c in checks:
        if c.label in broke:
            print(detail(c, it.top), flush=True)
    return failed


async def main_async(it: Input) -> None:
    workspace = Path(it.workspace).resolve() if it.workspace else ROOT
    home = Path(it.home) if it.home else Path(tempfile.mkdtemp(prefix="rj-inc-", dir="/tmp"))
    home.mkdir(parents=True, exist_ok=True)
    print(f"workspace {workspace}, home {home}, blaze {it.blaze}", flush=True)
    failed: list[str]
    try:
        failed = await gate(it, workspace, home)
    except (Failed, TimeoutError, OSError) as error:
        failed = [f"stopped: {error or type(error).__name__}"]
    finally:
        env = {k: v for k, v in os.environ.items() if k not in SCRUBBED} | {"BLAZE_HOME": str(home)}
        try:
            await run("stopping the daemon", [it.blaze, "stop"], workspace, env, 120)
        except Failed as error:
            print(f"warning: {error}", file=sys.stderr)
    if failed:
        fail(SCRIPT, f"home kept at {home}\n" + "\n".join(failed))
    if not it.keep_home and not it.home:
        shutil.rmtree(home, ignore_errors=True)
    print("ok")


def main() -> None:
    match sys.argv[1:]:
        case [value]:
            it = parse(Input, value, SCRIPT)
        case _:
            fail(SCRIPT, f"usage: {SCRIPT} JSON (see the top of the file)")
    if sys.platform not in ("linux", "darwin"):
        fail(SCRIPT, f"the daemon's socket is Unix's: not on {sys.platform}")
    if re.search(r"\s", it.noise_cwd) or Path(it.noise_cwd).is_absolute():
        fail(SCRIPT, f"noise_cwd is a directory inside the workspace, not {it.noise_cwd!r}")
    asyncio.run(main_async(it))


if __name__ == "__main__":
    main()
