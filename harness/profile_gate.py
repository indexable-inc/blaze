#!/usr/bin/env python3
"""profile_gate.py: the profile's gate over CI's self-build, held to harness/profile-gate-known.toml.

usage: uv run --frozen --no-dev python harness/profile_gate.py CONFIG

CONFIG is one JSON object, inline, the path of a file, or `-` for standard input
(AGENTS.md rule scripts-take-typed-input):

    {"blaze": "<the blaze this tree built>", "duckdb": "<the pinned DuckDB CLI>",
     "history": "<the self-build's profile history files>", "platform": "linux-x64",
     "runner": "<the build's runner class>", "commit": "<the commit built>", "out": "<dir>",
     "known": "harness/profile-gate-known.toml", "fixtures": "crates/blaze-profile/fixtures/gate",
     "build": null, "timeout_s": 900}

In parallel it runs `blaze debug gate --json --summary` over the history (its last build that
ended unless `build` says) and over each fixture of `fixtures` (`<check>/`, written by
harness/profile_fixtures.py), each of which must fail its own check. The run fails on:
- a check of the self-build that failed or could not run and is not listed with that outcome
  for this platform in `known` (a new failure, or one that went from could-not-run to failed);
- a listed check that passed (the list only shrinks: take it out);
- a fixture whose check did not fail (the check no longer catches what it is for);
- a self-build whose `builds` row names no tree or no blaze (rule measure-the-exact-tree);
- a known list that does not hold together (an entry with no owner or cause, a check that is
  not the gate's).
Every check's outcome is listed, and every one that could not run is named, so a partial
verdict never reads as clean.

It writes into `out`: `verdict.json` (the gate's), `summary.parquet` (the build's
`build_summary`), `identity.json` (commit, tree, blaze, platform, runner, build), `report.md`
(for the step summary) and `artifact` (the name the summary is uploaded under:
`profile-summary-<platform>-<runner>-<commit12>-<tree12>-<blaze12>`, what
harness/profile_trend.py lists).
"""

import asyncio
import dataclasses
import json
import os
import re
import shutil
import sys
import tempfile
import tomllib
from dataclasses import dataclass
from pathlib import Path
from typing import Literal

from typed_input import parse

SCRIPT = "profile_gate.py"
VERDICT_VERSION = 1
ARTIFACT_PREFIX = "profile-summary-"
# blaze's code for a failure of its own: a verdict that did not pass (blaze_core::exit::Exit::Failed).
EXIT_FAILED = 101
# What an artifact's name may not hold (actions/upload-artifact), and `-` between its fields.
UNSAFE = re.compile(r"[^A-Za-z0-9._]+")

type Outcome = Literal["passed", "failed", "unchecked"]


@dataclass(frozen=True)
class Config:
    blaze: str
    duckdb: str
    history: str
    platform: str
    runner: str
    commit: str
    out: str
    known: str = "harness/profile-gate-known.toml"
    fixtures: str = "crates/blaze-profile/fixtures/gate"
    build: int | None = None
    timeout_s: int = 900


@dataclass(frozen=True)
class Check:
    name: str
    outcome: Outcome
    said: str
    detail: tuple[str, ...]


@dataclass(frozen=True)
class Verdict:
    version: int
    build: int
    tree: str | None
    blaze: str | None
    process: int | None
    checks: tuple[Check, ...]


@dataclass(frozen=True)
class Known:
    check: str
    outcome: Literal["failed", "unchecked"]
    owner: str
    cause: str
    platforms: tuple[str, ...] = ()


@dataclass(frozen=True)
class KnownList:
    known: tuple[Known, ...] = ()


class Refused(Exception):
    """A step that failed, naming the step, the command, its exit code and its stderr's tail."""


def tail(text: bytes, lines: int = 30) -> str:
    return "\n".join(text.decode(errors="replace").splitlines()[-lines:])


async def gate(config: Config, step: str, history: Path, summary: Path | None, build: int | None) -> Verdict:
    """`blaze debug gate --json` over `history`: its verdict, whatever the exit code says."""
    command = [config.blaze, "debug", "gate", "--history", str(history), "--json"]
    if build is not None:
        command += ["--build", str(build)]
    if summary is not None:
        command += ["--summary", str(summary)]
    process = await asyncio.create_subprocess_exec(
        *command,
        env={**os.environ, "BLAZE_DUCKDB": config.duckdb},
        stdout=asyncio.subprocess.PIPE,
        stderr=asyncio.subprocess.PIPE,
    )
    try:
        async with asyncio.timeout(config.timeout_s):
            stdout, stderr = await process.communicate()
    except TimeoutError:
        process.kill()
        await process.wait()
        raise Refused(f"{step}: `{' '.join(command)}` ran past {config.timeout_s} s") from None
    try:
        verdict = parse(Verdict, stdout.decode(), f"{SCRIPT}: {step}'s verdict")
    except SystemExit:
        raise Refused(
            f"{step}: `{' '.join(command)}` exited {process.returncode} with no verdict:\n{tail(stderr)}"
        ) from None
    if verdict.version != VERDICT_VERSION:
        raise Refused(f"{step}: the verdict is version {verdict.version}; this script reads {VERDICT_VERSION}")
    # A verdict that passed exits 0; one that did not, EXIT_FAILED with its summary. Anything
    # else, or a summary asked for and not written, is the command failing.
    passed = all(it.outcome == "passed" for it in verdict.checks)
    if process.returncode != (0 if passed else EXIT_FAILED) or (summary is not None and not summary.is_file()):
        raise Refused(f"{step}: `{' '.join(command)}` exited {process.returncode}:\n{tail(stderr)}")
    return verdict


async def fixture(config: Config, dir: Path) -> str | None:
    """The gate over the fixture `dir`, in a copy (the reader writes schema files where it reads);
    why its check did not fail, or None."""
    check = dir.name
    with tempfile.TemporaryDirectory(prefix=f"profile-gate-{check}-") as scratch:
        for file in dir.glob("*.parquet"):
            shutil.copy(file, scratch)
        verdict = await gate(config, f"the fixture {check}", Path(scratch), None, None)
    outcome = next((it.outcome for it in verdict.checks if it.name == check), None)
    if outcome is None:
        return f"the fixture {check} names no check of the gate (its checks: {', '.join(it.name for it in verdict.checks)})"
    if outcome != "failed":
        said = next(it.said for it in verdict.checks if it.name == check)
        return f"the fixture {check} does not fail its check: {outcome}, {said}"
    return None


def read_known(path: Path) -> KnownList:
    try:
        text = path.read_text()
        data = tomllib.loads(text)
    except (OSError, tomllib.TOMLDecodeError) as e:
        raise Refused(f"reading {path}: {e}") from e
    return parse(KnownList, json.dumps(data), f"{SCRIPT}: {path}")


def held(verdict: Verdict, known: KnownList, platform: str) -> tuple[list[str], list[str]]:
    """Each check against the known list: (what fails the run, what is listed and held)."""
    names = {it.name for it in verdict.checks}
    problems: list[str] = []
    listed: list[str] = []
    mine: dict[str, Known] = {}
    for entry in known.known:
        if entry.check not in names:
            problems.append(f"the known list names `{entry.check}`, which is no check of the gate")
        if not entry.owner.strip() or not entry.cause.strip():
            problems.append(f"the known list's `{entry.check}` has no owner or no cause")
        if entry.platforms and platform not in entry.platforms:
            continue
        if entry.check in mine:
            problems.append(f"the known list has `{entry.check}` twice for {platform}")
        mine[entry.check] = entry
    for check in verdict.checks:
        entry = mine.get(check.name)
        if check.outcome == "passed":
            if entry is not None:
                problems.append(f"{check.name} passed and is listed as {entry.outcome} (owner {entry.owner}): take it out")
            continue
        if entry is None:
            problems.append(f"{check.name} {check.outcome} and is not listed: {check.said}")
        elif entry.outcome != check.outcome:
            problems.append(f"{check.name} {check.outcome}, listed as {entry.outcome} (owner {entry.owner}): {check.said}")
        else:
            listed.append(f"{check.name} {check.outcome}, listed (owner {entry.owner}: {entry.cause})")
    return problems, listed


def report(verdict: Verdict, problems: list[str], listed: list[str], fixtures: list[str]) -> str:
    lines = [
        f"### profile gate: build {verdict.build}",
        "",
        f"tree `{verdict.tree}`, blaze `{verdict.blaze}`, process {verdict.process}",
        "",
        "| check | outcome | said |",
        "| --- | --- | --- |",
    ]
    for check in verdict.checks:
        lines.append(f"| {check.name} | {check.outcome} | {check.said.replace('|', '/')} |")
    unchecked = [it.name for it in verdict.checks if it.outcome == "unchecked"]
    lines += ["", f"could not run: {', '.join(unchecked) or 'none'}", ""]
    for check in verdict.checks:
        if check.detail:
            lines += [f"{check.name}:", *(f"- {it}" for it in check.detail), ""]
    lines += [*(f"- known: {it}" for it in listed), *(f"- **fails the run**: {it}" for it in problems)]
    lines += [f"- fixtures: {len(fixtures)} checked, each failing its own check"]
    return "\n".join(lines) + "\n"


def artifact(config: Config, verdict: Verdict) -> str:
    def field(text: str) -> str:
        return UNSAFE.sub("_", text)

    tree = (verdict.tree or "none")[:12]
    blaze = (verdict.blaze or "none")[:12]
    return ARTIFACT_PREFIX + "-".join(
        field(it) for it in (config.platform, config.runner, config.commit[:12], tree, blaze)
    )


async def main() -> None:
    if len(sys.argv) != 2:
        sys.exit(f"usage: {SCRIPT} CONFIG (one JSON object; see the docstring)")
    config = parse(Config, sys.argv[1], SCRIPT)
    out = Path(config.out)
    out.mkdir(parents=True, exist_ok=True)
    summary = out / "summary.parquet"
    try:
        known = read_known(Path(config.known))
        fixtures = sorted(it for it in Path(config.fixtures).iterdir() if it.is_dir())
        if not fixtures:
            raise Refused(f"{config.fixtures} has no fixture: run harness/profile_fixtures.py")
        async with asyncio.TaskGroup() as group:
            build = group.create_task(gate(config, "the self-build", Path(config.history), summary, config.build))
            missed = [group.create_task(fixture(config, it)) for it in fixtures]
    except* Refused as errors:
        sys.exit("\n".join(f"{SCRIPT}: {it}" for it in errors.exceptions))
    verdict = build.result()
    problems, listed = held(verdict, known, config.platform)
    problems += [why for it in missed if (why := it.result()) is not None]
    if verdict.tree is None or verdict.blaze is None:
        problems.append(f"build {verdict.build}'s row names no tree or no blaze: a measurement names its exact tree")
    (out / "verdict.json").write_text(json.dumps(dataclasses.asdict(verdict), indent=2) + "\n")
    identity = {
        "commit": config.commit,
        "tree": verdict.tree,
        "blaze": verdict.blaze,
        "platform": config.platform,
        "runner": config.runner,
        "build": verdict.build,
    }
    (out / "identity.json").write_text(json.dumps(identity, indent=2) + "\n")
    (out / "artifact").write_text(artifact(config, verdict) + "\n")
    text = report(verdict, problems, listed, [it.name for it in fixtures])
    (out / "report.md").write_text(text)
    print(text, end="")
    if problems:
        sys.exit(f"{SCRIPT}: {len(problems)} problems:\n" + "\n".join(f"- {it}" for it in problems))


if __name__ == "__main__":
    asyncio.run(main())
