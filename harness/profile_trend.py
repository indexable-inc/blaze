#!/usr/bin/env python3
"""profile_trend.py: the spans the newest self-build on main got slower in, against the builds before it.

usage: uv run --frozen --no-dev python harness/profile_trend.py CONFIG

CONFIG is one JSON object, inline, the path of a file, or `-` for standard input
(AGENTS.md rule scripts-take-typed-input):

    {"repo": "indexable-inc/blaze", "platform": "linux-x64", "duckdb": "<the pinned DuckDB CLI>",
     "out": "<dir>", "count": 10, "min_ratio": 1.25, "min_slower_ms": 50.0, "limit": 20,
     "branch": "main", "timeout_s": 900}

It lists the per-build summaries CI's profile gate uploads (harness/profile_gate.py, artifacts
`profile-summary-<platform>-<runner>-<commit12>-<tree12>-<blaze12>`) of runs on `branch`, takes
the newest one's runner class, and of that class the newest summary of each of the last
`count` commits (a summary is only comparable to one taken on the same class of machine). It
downloads them in parallel and runs `slower_between_commits` (crates/blaze-profile/sql/
examples.sql, the one definition) with the newest commit as `later` and the others as
`earlier`. The run fails naming each span whose median grew to at least `min_ratio` times and
by at least `min_slower_ms`; with fewer than two commits' summaries it fails saying so, since
nothing was compared. It writes `slower.json` (every row the view gave) and `report.md` into
`out`.
"""

import asyncio
import json
import re
import sys
import zipfile
from dataclasses import dataclass
from pathlib import Path
from typing import Any, cast

from typed_input import parse

SCRIPT = "profile_trend.py"
ROOT = Path(__file__).resolve().parent.parent
EXAMPLES = ROOT / "crates" / "blaze-profile" / "sql" / "examples.sql"
VIEW = "slower_between_commits"
PREFIX = "profile-summary-"
UNSAFE = re.compile(r"[^A-Za-z0-9._]+")
DOWNLOADS = 4


@dataclass(frozen=True)
class Config:
    repo: str
    platform: str
    duckdb: str
    out: str
    count: int = 10
    min_ratio: float = 1.25
    min_slower_ms: float = 50.0
    limit: int = 20
    branch: str = "main"
    timeout_s: int = 900


@dataclass(frozen=True)
class Summary:
    id: int
    name: str
    created_at: str
    runner: str
    commit: str


class Refused(Exception):
    """A step that failed, naming the step, the command, its exit code and its stderr's tail."""


async def run(step: str, command: list[str], timeout_s: int, stdin: bytes | None = None) -> bytes:
    process = await asyncio.create_subprocess_exec(
        *command,
        stdin=asyncio.subprocess.PIPE if stdin is not None else asyncio.subprocess.DEVNULL,
        stdout=asyncio.subprocess.PIPE,
        stderr=asyncio.subprocess.PIPE,
    )
    try:
        async with asyncio.timeout(timeout_s):
            stdout, stderr = await process.communicate(stdin)
    except TimeoutError:
        process.kill()
        await process.wait()
        raise Refused(f"{step}: `{' '.join(command)}` ran past {timeout_s} s") from None
    if process.returncode != 0:
        tail = "\n".join(stderr.decode(errors="replace").splitlines()[-20:])
        raise Refused(f"{step}: `{' '.join(command)}` exited {process.returncode}:\n{tail}")
    return stdout


async def summaries(config: Config) -> list[Summary]:
    """The summaries of `config.platform` on `config.branch`, newest first."""
    jq = (
        f'.artifacts[] | select(.expired | not) | select(.name | startswith("{PREFIX}")) '
        f'| select(.workflow_run.head_branch == "{config.branch}") | {{id, name, created_at}}'
    )
    command = ["gh", "api", "--paginate", f"repos/{config.repo}/actions/artifacts?per_page=100", "--jq", jq]
    stdout = await run("listing the summaries", command, config.timeout_s)
    platform = UNSAFE.sub("_", config.platform)
    found: list[Summary] = []
    for line in stdout.decode().splitlines():
        if not line.strip():
            continue
        it = cast(dict[str, Any], json.loads(line))
        fields = str(it["name"]).removeprefix(PREFIX).split("-")
        # <platform>-<runner>-<commit12>-<tree12>-<blaze12>, each field free of `-`.
        if len(fields) != 5 or fields[0] != platform:
            continue
        found.append(Summary(int(it["id"]), str(it["name"]), str(it["created_at"]), fields[1], fields[2]))
    found.sort(key=lambda it: it.created_at, reverse=True)
    return found


def chosen(found: list[Summary], count: int) -> list[Summary]:
    """Of the newest summary's runner class, the newest summary of each of the last `count` commits."""
    if not found:
        return []
    runner = found[0].runner
    picked: dict[str, Summary] = {}
    for it in found:
        if it.runner == runner and it.commit not in picked and len(picked) < count:
            picked[it.commit] = it
    return list(picked.values())


async def download(config: Config, it: Summary, into: Path, gate: asyncio.Semaphore) -> Path:
    """The summary's `summary.parquet`, unzipped under `into`."""
    async with gate:
        command = ["gh", "api", f"repos/{config.repo}/actions/artifacts/{it.id}/zip"]
        data = await run(f"downloading {it.name}", command, config.timeout_s)
    zipped = into / f"{it.id}.zip"
    zipped.write_bytes(data)
    dir = into / str(it.id)
    with zipfile.ZipFile(zipped) as archive:
        names = archive.namelist()
        if "summary.parquet" not in names:
            raise Refused(f"downloading {it.name}: it holds no summary.parquet (it holds {', '.join(names)})")
        archive.extract("summary.parquet", dir)
    return dir / "summary.parquet"


def view_sql(earlier: str, later: str, limit: int) -> str:
    """`slower_between_commits` from examples.sql, its arguments bound as literals."""
    text = EXAMPLES.read_text()
    start = text.index(f"-- name: {VIEW}\n")
    body = text[start:].split("\n", 2)[2]
    end = body.find("\n-- name: ")
    sql = body if end < 0 else body[:end]
    return sql.replace("$earlier", f"'{earlier}'").replace("$later", f"'{later}'").replace("$limit", str(limit))


async def main() -> None:
    if len(sys.argv) != 2:
        sys.exit(f"usage: {SCRIPT} CONFIG (one JSON object; see the docstring)")
    config = parse(Config, sys.argv[1], SCRIPT)
    out = Path(config.out)
    out.mkdir(parents=True, exist_ok=True)
    try:
        picked = chosen(await summaries(config), config.count)
        if len(picked) < 2:
            raise Refused(
                f"{len(picked)} commits on {config.branch} have a {config.platform} summary of one runner class: "
                "nothing to compare, so no span is known not to have slowed"
            )
        gate = asyncio.Semaphore(DOWNLOADS)
        async with asyncio.TaskGroup() as group:
            tasks = [group.create_task(download(config, it, out, gate)) for it in picked]
        files = [it.result() for it in tasks]
        # The newest commit is `later`, every other `earlier`, whatever commit the rows say.
        parts = [
            f"SELECT * REPLACE ('{'later' if i == 0 else 'earlier'}' AS commit) FROM read_parquet('{str(file).replace("'", "''")}')"
            for i, file in enumerate(files)
        ]
        script = (
            f"CREATE VIEW build_summary AS {' UNION ALL BY NAME '.join(parts)};\n"
            f"{view_sql('earlier', 'later', config.limit)};\n"
        )
        stdout = await run("comparing the summaries", [config.duckdb, "-json", "-bail"], config.timeout_s, script.encode())
    except* Refused as errors:
        sys.exit("\n".join(f"{SCRIPT}: {it}" for it in errors.exceptions))
    text = stdout.decode().strip()
    rows = cast(list[dict[str, Any]], json.loads(text)) if text else []
    (out / "slower.json").write_text(json.dumps(rows, indent=2) + "\n")
    newest = picked[0]
    slowed = [
        it
        for it in rows
        if it["earlier_ms"] and it["later_ms"] is not None
        and float(it["later_ms"]) >= config.min_ratio * float(it["earlier_ms"])
        and float(it["slower_ms"]) >= config.min_slower_ms
    ]
    lines = [
        f"### profile trend: {config.platform} on {newest.runner}",
        "",
        f"later: {newest.commit} ({newest.name}); earlier: {', '.join(it.commit for it in picked[1:])}",
        "",
        "| span | earlier ms | later ms | slower ms | runs (earlier, later) |",
        "| --- | --- | --- | --- | --- |",
    ]
    for it in rows:
        lines.append(
            f"| {it['name']} | {float(it['earlier_ms']):.1f} | {float(it['later_ms']):.1f} | "
            f"{float(it['slower_ms']):.1f} | {it['earlier_runs']}, {it['later_runs']} |"
        )
    report = "\n".join(lines) + "\n"
    (out / "report.md").write_text(report)
    print(report, end="")
    if slowed:
        named = "\n".join(
            f"- `{it['name']}`: {float(it['earlier_ms']):.1f} ms to {float(it['later_ms']):.1f} ms "
            f"(+{float(it['slower_ms']):.1f} ms) at {newest.commit}"
            for it in slowed
        )
        sys.exit(
            f"{SCRIPT}: {len(slowed)} spans slowed by {config.min_ratio}x and {config.min_slower_ms} ms or more:\n{named}"
        )


if __name__ == "__main__":
    asyncio.run(main())
