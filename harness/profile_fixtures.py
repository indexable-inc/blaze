#!/usr/bin/env python3
"""profile_fixtures.py: writes the profile gate's fixtures, one small history a check that it fails.

usage: uv run --frozen --no-dev python harness/profile_fixtures.py CONFIG

CONFIG is one JSON object, inline, the path of a file, or `-` for standard input
(AGENTS.md rule scripts-take-typed-input):

    {"duckdb": "<the pinned DuckDB CLI: $(nix build --print-out-paths .#duckdb)/bin/duckdb>",
     "out": "crates/blaze-profile/fixtures/gate"}

Each fixture is `<out>/<check>/`, a history of one build (build 1 of process 100, from 1 s to
2 s on the profile's clock) whose files break that check of `blaze_profile::gate` and only
as much else as it must. `gate.rs`'s `each_fixture_fails_its_own_check` and
`harness/profile_gate.py` hold the gate to failing each. The files are checked in; run this
again after changing a fixture or the history's schema, and commit what it writes.
"""

import asyncio
import shutil
import sys
from dataclasses import dataclass
from pathlib import Path

from typed_input import parse

SCRIPT = "profile_fixtures.py"
TIMEOUT_S = 60

T0 = 1_000_000_000
T1 = 2_000_000_000
BUILD = 1
PROCESS = 100
THREAD = 7


@dataclass(frozen=True)
class Config:
    duckdb: str
    out: str


def copy(select: str, file: str, metadata: str = "") -> str:
    """`COPY` of `select` to the fixture's `file`; `metadata`: its KV_METADATA's struct."""
    kv = f", KV_METADATA {metadata}" if metadata else ""
    return f"COPY ({select}) TO '{file}' (FORMAT parquet{kv});"


BUILDS = copy(
    f"SELECT CAST({BUILD} AS UBIGINT) AS build_id, 'f1' AS commit, '/w' AS workspace, 'build' AS command, "
    f"CAST({T0} AS BIGINT) AS started_ns, CAST({T1 - T0} AS BIGINT) AS dur_ns, "
    "CAST(1700000000000000000 AS BIGINT) AS started_unix_ns, 'ok' AS outcome, 't1' AS tree, 'b1' AS blaze, "
    f"CAST({PROCESS} AS UINTEGER) AS process",
    "1-00000001.builds.parquet",
)


def switches(*rows: tuple[int, str]) -> str:
    """`switches` of the one worker: each (time after t0, event)."""
    selects = " UNION ALL ".join(
        f"SELECT CAST({BUILD} AS UBIGINT) AS build_id, CAST({PROCESS} AS UINTEGER) AS process, "
        f"CAST({THREAD} AS UINTEGER) AS thread, 'blaze-work-0' AS thread_name, CAST({T0 + at} AS BIGINT) AS t_ns, "
        f"'{event}' AS event, CAST(1 AS UBIGINT) AS job, CAST(0 AS UINTEGER) AS fiber, CAST(0 AS UINTEGER) AS core, "
        f"CAST({T0 + at} AS BIGINT) AS since_ns"
        for at, event in rows
    )
    return copy(selects, "1-00000001.switches.parquet")


# A worker that ran one job through the whole build: covered, so only the check under test fails.
BUSY = switches((0, "started"), (T1 - T0, "ended"))


def span(span_id: int, start: int, dur: int, *, name: str = "job", parent: int | None = None,
         self_cpu: int | None = None, awaits: str | None = None) -> str:
    """One `spans` row of the worker, `start` after t0."""
    parent_sql = "NULL" if parent is None else str(parent)
    cpu_sql = "NULL" if self_cpu is None else str(self_cpu)
    awaits_sql = "NULL" if awaits is None else f"'{awaits}'"
    return (
        f"SELECT CAST({BUILD} AS UBIGINT) AS build_id, CAST({PROCESS} AS UINTEGER) AS process, "
        f"'blaze' AS process_name, CAST({THREAD} AS UINTEGER) AS thread, 'blaze-work-0' AS thread_name, "
        f"CAST({span_id} AS UBIGINT) AS span_id, CAST({parent_sql} AS UBIGINT) AS parent, "
        f"CAST({0 if parent is None else 1} AS USMALLINT) AS depth, '{name}' AS name, 'blaze' AS target, "
        f"CAST({T0 + start} AS BIGINT) AS start_ns, CAST({dur} AS BIGINT) AS dur_ns, "
        f"CAST({cpu_sql} AS BIGINT) AS self_cpu_ns, CAST({awaits_sql} AS VARCHAR) AS awaits"
    )


def spans(*rows: str) -> str:
    return copy(" UNION ALL ".join(rows), "1-00000001.spans.parquet")


def counter(name: str, at: int, value: float, *, build: int | None = None) -> str:
    build_sql = "NULL" if build is None else str(build)
    return (
        f"SELECT CAST({build_sql} AS UBIGINT) AS build_id, CAST({PROCESS} AS UINTEGER) AS process, '{name}' AS name, "
        f"CAST({at} AS BIGINT) AS t_ns, CAST({value} AS DOUBLE) AS value"
    )


def counters(*rows: str) -> str:
    return copy(" UNION ALL ".join(rows), "1-00000001.counters.parquet")


def samples(count: int, *, stack: str, cpu: str, span_id: int | None) -> str:
    """`count` samples of the worker, spread over the build."""
    span_sql = "NULL" if span_id is None else str(span_id)
    step = (T1 - T0) // (count + 1)
    return copy(
        f"SELECT CAST({BUILD} AS UBIGINT) AS build_id, CAST({PROCESS} AS UINTEGER) AS process, "
        f"CAST({THREAD} AS UINTEGER) AS thread, CAST({T0} + {step} * (i + 1) AS BIGINT) AS t_ns, "
        f"{stack} AS stack, CAST({span_sql} AS UBIGINT) AS span_id, '{cpu}' AS cpu FROM range({count}) r(i)",
        "1-00000001.samples.parquet",
    )


MS = 1_000_000
# The check each fixture fails, and the files beside its `builds` row that make it fail.
FIXTURES: dict[str, list[str]] = {
    # The worker ran a job for its first 0.1 s and then nothing said what it did: 90% uncovered.
    "workers_covered": [switches((0, "started"), (100 * MS, "parked"))],
    # The worker blocked on a condition variable with no wait span open.
    "blocks_in_waits": [
        BUSY,
        samples(1, stack="['start', 'std::sync::Condvar::wait', '__psynch_cvwait']", cpu="off", span_id=None),
    ],
    # A wait that says nothing of what it awaits.
    "waits_say_what": [BUSY, spans(span(1, 100 * MS, MS, name="wait"))],
    # The recorder could not open 5 spans during the build.
    "rows_lost": [
        BUSY,
        counters(counter("profile spans lost", T0 - 1, 0), counter("profile spans lost", T0 + 500 * MS, 5)),
    ],
    # Every sample of the worker names no span.
    "samples_in_spans": [BUSY, samples(10, stack="['start', 'work']", cpu="on", span_id=None)],
    # A child that closes 50 ms after its parent.
    "children_in_parents": [
        BUSY,
        spans(span(1, 100 * MS, 100 * MS), span(2, 150 * MS, 100 * MS, name="child", parent=1)),
    ],
    # A span of 1 ms that says it used 5 ms of CPU of its own.
    "self_cpu_in_duration": [BUSY, spans(span(1, 100 * MS, MS, self_cpu=5 * MS))],
    # The process used 1 s of CPU over the build; its spans say 0.5 s.
    "cpu_matches_process": [
        BUSY,
        spans(span(1, 0, 900 * MS, self_cpu=500 * MS)),
        counters(
            counter("process cpu ns", T0, 0, build=BUILD), counter("process cpu ns", T1, 1_000_000_000, build=BUILD)
        ),
    ],
    # A merged file whose metadata says its merge read 3 rows; it holds 2.
    "merged_rows_match": [
        BUSY,
        copy(
            f"SELECT CAST({BUILD} AS UBIGINT) AS build_id, 'src/' || i || '.rs' AS key, 'file' AS kind, "
            "CAST(NULL AS VARCHAR) AS detail, 'watcher' AS seen_by FROM range(2) r(i)",
            "1-00000001-00000002-1.inputs.parquet",
            "{'blaze.merged_rows': '3'}",
        ),
    ],
}


async def write(duckdb: str, dir: Path, statements: list[str]) -> None:
    """The fixture's files, written by one DuckDB run in `dir`."""
    if dir.exists():
        shutil.rmtree(dir)
    dir.mkdir(parents=True)
    script = "\n".join([BUILDS, *statements]) + "\n"
    command = [duckdb, "-batch", "-bail"]
    process = await asyncio.create_subprocess_exec(
        *command,
        cwd=dir,
        stdin=asyncio.subprocess.PIPE,
        stdout=asyncio.subprocess.PIPE,
        stderr=asyncio.subprocess.PIPE,
    )
    try:
        async with asyncio.timeout(TIMEOUT_S):
            _, stderr = await process.communicate(script.encode())
    except TimeoutError:
        process.kill()
        await process.wait()
        raise RuntimeError(f"{SCRIPT}: writing {dir}: `{' '.join(command)}` ran past {TIMEOUT_S} s") from None
    if process.returncode != 0:
        tail = "\n".join(stderr.decode(errors="replace").splitlines()[-20:])
        raise RuntimeError(f"{SCRIPT}: writing {dir}: `{' '.join(command)}` exited {process.returncode}:\n{tail}")


async def main() -> None:
    if len(sys.argv) != 2:
        sys.exit(f"usage: {SCRIPT} CONFIG (one JSON object: duckdb, out)")
    config = parse(Config, sys.argv[1], SCRIPT)
    out = Path(config.out)
    try:
        async with asyncio.TaskGroup() as group:
            for check, statements in FIXTURES.items():
                group.create_task(write(config.duckdb, out / check, statements))
    except* RuntimeError as errors:
        sys.exit("\n".join(str(it) for it in errors.exceptions))
    for check in FIXTURES:
        files = sorted(it.name for it in (out / check).iterdir())
        print(f"{check}: {', '.join(files)}")


if __name__ == "__main__":
    asyncio.run(main())
