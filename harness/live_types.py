#!/usr/bin/env python3
"""Live edits that add, move to and delete types (macOS on AArch64, Linux on x86-64), as a game's
components change while it runs: a component is never changed in place, a new type takes its
place ([live.md](../docs/live.md#types)).

usage: live_types.py        (BLAZE_BIN names the blaze built; blaze is found next to it)

harness/live-types keeps values of `V1` in a world that erases their type, each type with an id
in a static of its own handed out from a pool every type shares, as flecs_ecs registers its
components. It is built by the daemon, started, and given the texts of harness/live-types/stages
one after another, each of which has to be applied as a patch with no restart:

  1  adds `struct V2` with a derive, its id's static and a trait impl, in the file of `V1`, and
     asks for its id: the id is the pool's next (1), and `V1` keeps its own (0)
  2  moves the code to `V2`, converting the world's values of `V1` (a `From` impl, generic
     instances of the world's methods and closures over `V2`), counted in a static new in the
     edit, which starts at zero
  3  deletes `V1`
  4  changes a field of `V2`, which the program has values of: the program is told to restart,
     and why, naming `V2`

Each stage's answer is compared with the first answer of the same text built by blaze from
nothing (a home, a store and a target directory of its own) and started afresh; every edit's time from the build's start to the program having applied it
is printed. Python 3.13, stdlib only. Every step has a timeout; a failure names the step.
"""

import asyncio
import os
import re
import shutil
import subprocess
import sys
import tempfile
import time
from pathlib import Path

from live_e2e import APPLY_TIMEOUT, Running, StepError, build, serving, show_daemon_log, start, own_home

HERE = Path(__file__).resolve().parent
ROOT = HERE.parent
PROGRAM = HERE / "live-types"
STAGE_MARK = "// The stage.\n"


def text_of(stage: int) -> str:
    """The program's source at `stage`: what every stage shares, then the stage's own part."""
    main = (PROGRAM / "src" / "main.rs").read_text()
    shared, own = main.split(STAGE_MARK)
    if stage == 0:
        return main
    return shared + STAGE_MARK + (PROGRAM / "stages" / f"{stage}.rs").read_text()


def applied(outcome: str) -> bool:
    return re.match(r"outcome: edit \d+ applied", outcome) is not None


def answer_of(line: str) -> str:
    return re.sub(r" at_us=\d+$", "", line.removeprefix("answer "))


async def from_nothing(stage: int, work: Path, bin_dir: Path) -> str:
    """The first answer of the stage's text built by blaze from nothing and started afresh."""
    step = f"from nothing {stage}"
    project = work / "scratch" / "harness" / "live-types"
    (project / "src" / "main.rs").write_text(text_of(stage))
    # Short, under /tmp: the daemon's socket is in it, and macOS allows 104 bytes.
    home = tempfile.mkdtemp(prefix="rj-scratch-", dir=None if os.name == "nt" else "/tmp")
    target = work / f"target-scratch-{stage}"
    env = dict(os.environ, PATH=f"{bin_dir}{os.pathsep}{os.environ['PATH']}", BLAZE_HOME=home,
               CARGO_TARGET_DIR=str(target))
    env.pop("CARGO_INCREMENTAL", None)
    try:
        await build(step, project, env)
    finally:
        subprocess.run(["blaze", "stop"], cwd=project, env=env, capture_output=True, timeout=120)
        shutil.rmtree(home, ignore_errors=True)
    running = Running(
        await asyncio.create_subprocess_exec(
            str(target / "debug" / "livetypes"),
            cwd=project,
            env=env,
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.STDOUT,
        )
    )
    try:
        return answer_of(await running.wait_for(step, "answer "))
    finally:
        await running.stop()


async def edit(step: str, program: Running, source: Path, stage: int, project: Path, env: dict[str, str]) -> tuple[str, str, float]:
    """Writes the stage's text and builds it: what the program said of the patch, its first answer
    since, and the milliseconds from the build's start to the program having applied it."""
    source.write_text(text_of(stage))
    since = len(program.log)
    began = time.monotonic()
    out = await build(step, project, env)
    print(f"  build: {out.strip().splitlines()[-1] if out.strip() else ''}")
    outcome = await program.wait_for(f"{step}: the program is told", "outcome:", since)
    took = (time.monotonic() - began) * 1000
    if not applied(outcome):
        return outcome, "", took
    answer = await program.wait_for(f"{step}: the new code runs", f"answer stage {stage}:", since)
    return outcome, answer_of(answer), took


async def main() -> None:
    own_home()
    bin_dir = Path(os.environ["BLAZE_BIN"]).resolve().parent
    work = Path(tempfile.mkdtemp(prefix="blaze-live-types-"))
    started: list[Running] = []
    try:
        for copy in [work, work / "scratch"]:
            shutil.copytree(ROOT / "crates" / "blaze-live", copy / "crates" / "blaze-live")
            shutil.copytree(PROGRAM, copy / "harness" / "live-types", ignore=shutil.ignore_patterns("target"))
        stages = [0, 1, 2, 3]
        expected = {}
        for stage in stages:
            expected[stage] = await from_nothing(stage, work, bin_dir)
            print(f"from nothing {stage}: {expected[stage]}")

        project = work / "harness" / "live-types"
        source = project / "src" / "main.rs"
        executable = work / "target" / "debug" / "livetypes"
        env = dict(
            os.environ,
            PATH=f"{bin_dir}{os.pathsep}{os.environ['PATH']}",
            CARGO_TARGET_DIR=str(work / "target"),
            RUST_BACKTRACE="full",
        )
        env.pop("CARGO_INCREMENTAL", None)
        # The first edit after the daemon starts writes the whole object again: it is made
        # before the program starts.
        source.write_text(text_of(0).replace('"stage 0:', '"stage 0 (first build):'))
        await build("first build", project, env)
        await serving(project, env)
        source.write_text(text_of(0))
        await build("edit before start", project, env)
        program = Running(
            await asyncio.create_subprocess_exec(
                str(executable), cwd=project, env=env, stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.STDOUT
            )
        )
        started.append(program)
        await program.wait_for("start", "ready")
        got = answer_of(await program.wait_for("start", "answer "))
        if got != expected[0]:
            raise StepError(f"start: the program says `{got}`, the build from nothing `{expected[0]}`")
        print(f"started: {got}")

        names = {1: "add V2", 2: "move to V2", 3: "delete V1"}
        for stage in stages[1:]:
            step = f"stage {stage} ({names[stage]})"
            outcome, got, took = await edit(step, program, source, stage, project, env)
            print(f"{step}: {outcome.removeprefix('outcome: ')} [{took:.0f} ms from the build to applied]")
            if not applied(outcome):
                raise StepError(f"{step}: the edit was not applied as a patch: {outcome}")
            if got != expected[stage]:
                raise StepError(f"{step}: the program says `{got}`, the build from nothing `{expected[stage]}`")
            print(f"  {got}")

        # A type the program has values of, changed in place, is the one edit that restarts.
        step = "stage 4 (change V2)"
        outcome, _, _ = await edit(step, program, source, 4, project, env)
        print(f"{step}: {outcome.removeprefix('outcome: ')}")
        if "restart needed" not in outcome or "`struct V2`" not in outcome:
            raise StepError(f"{step}: the program was not told to restart because of `struct V2`: {outcome}")
        print("live types: V2 added, the code moved to it and V1 deleted in a running program, each a patch")
    finally:
        for running in started:
            await running.stop()
        shutil.rmtree(work, ignore_errors=True)


if __name__ == "__main__":
    try:
        asyncio.run(main())
    except StepError as error:
        print(f"error: {error}", file=sys.stderr)
        show_daemon_log()
        sys.exit(1)
