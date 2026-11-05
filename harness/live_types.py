#!/usr/bin/env python3
"""Live edits that add, move to and delete types (macOS on AArch64, Linux on x86-64), as a game's
components change while it runs: a component is never changed in place, a new type takes its
place ([live.md](../docs/live.md#types)).

usage: uv run --frozen --no-dev python harness/live_types.py '<json>'   (or a file, or - for stdin)

    {"blaze": "/abs/path/to/blaze", "env": {}, "keep": false, "tmp": "/tmp"}

  blaze    the blaze executable under test (required)
  env      extra environment for every blaze command and the program (default none)
  keep     keep the work directories and the daemons' homes for a look after (default false)
  tmp      where the daemons' homes and the work directories go (default /tmp), as live_e2e.py's

harness/live-types keeps values of `V1` in a world that erases their type, each type with an id
in a static of its own handed out from a pool every type shares, as flecs_ecs registers its
components. It is copied with what it links into a git repository of its own as live_e2e.py
does, built, started with `blaze run` on the daemon's code memory, and given the texts of
harness/live-types/stages one after another, each of which has to be applied as a patch with no
restart:

  1  adds `struct V2` with a derive, its id's static and a trait impl, in the file of `V1`, and
     asks for its id: the id is the pool's next (1), and `V1` keeps its own (0)
  2  moves the code to `V2`, converting the world's values of `V1` (a `From` impl, generic
     instances of the world's methods and closures over `V2`), counted in a static new in the
     edit, which starts at zero
  3  deletes `V1`
  4  changes a field of `V2`, which the program has values of: the program is told to restart,
     and why, naming `V2`

Each stage's answer is compared with the first answer of the same text built by blaze from
nothing (a copy and a daemon home of its own) and started afresh; every edit's time from the
build's start to the program having applied it is printed. Python 3.13. Every step has a
timeout; a failure names the step, the command, its exit code and its output's tail.
"""

import asyncio
import os
import re
import shutil
import sys
import tempfile
import time
from dataclasses import dataclass, field
from pathlib import Path

from live_e2e import BUILD_TIMEOUT, HERE, Running, StepError, command, copy_tree, stop_daemon
from typed_input import parse

SCRIPT = "live_types.py"
PROGRAM = "live-types"
STAGE_MARK = "// The stage.\n"
STAGES = {1: "add V2", 2: "move to V2", 3: "delete V1"}


@dataclass(frozen=True)
class Args:
    blaze: str
    env: dict[str, str] = field(default_factory=dict[str, str])
    keep: bool = False
    tmp: str = "/tmp"


@dataclass(frozen=True)
class Daemon:
    """A daemon home and a copy of the program of its own, and the environment that reaches it."""

    blaze: str
    home: Path
    work: Path
    project: Path
    env: dict[str, str]

    @property
    def source(self) -> Path:
        return self.project / "src" / "main.rs"

    async def build(self, step: str) -> str:
        return await command(step, [self.blaze, "build"], self.project, self.env, BUILD_TIMEOUT)

    async def run(self, step: str) -> Running:
        """`blaze run` of the program, once it said it is ready."""
        proc = await asyncio.create_subprocess_exec(
            self.blaze,
            "run",
            cwd=self.project,
            env=self.env,
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.STDOUT,
        )
        running = Running(proc)
        await running.wait_for(f"{step}: the program starts", "ready")
        return running

    async def close(self, keep: bool) -> None:
        await stop_daemon(self.blaze, self.home, self.env)
        if keep:
            print(f"kept: work {self.work}, home {self.home}", file=sys.stderr)
        else:
            shutil.rmtree(self.work, ignore_errors=True)
            shutil.rmtree(self.home, ignore_errors=True)


async def daemon(args: Args, name: str) -> Daemon:
    """A fresh copy of the program with a daemon home of its own under `args.tmp`."""
    blaze = str(Path(args.blaze).resolve())
    # Short: the daemon's sockets are in it, and macOS allows 104 bytes.
    home = Path(tempfile.mkdtemp(prefix=f"rj-{name}-", dir=args.tmp))
    work = Path(tempfile.mkdtemp(prefix=f"blaze-live-types-{name}-", dir=args.tmp))
    env = {**os.environ, "BLAZE_HOME": str(home), "RUST_BACKTRACE": "full", **args.env}
    for unset in ("CARGO_TARGET_DIR", "CARGO_INCREMENTAL", "RUSTUP_TOOLCHAIN"):
        env.pop(unset, None)
    made = Daemon(blaze, home, work, work, env)
    try:
        project = await copy_tree(work, PROGRAM)
    except BaseException:
        await made.close(args.keep)
        raise
    return Daemon(blaze, home, work, project, env)


def text_of(stage: int) -> str:
    """The program's source at `stage`: what every stage shares, then the stage's own part."""
    main = (HERE / PROGRAM / "src" / "main.rs").read_text()
    if stage == 0:
        return main
    shared, _ = main.split(STAGE_MARK)
    return shared + STAGE_MARK + (HERE / PROGRAM / "stages" / f"{stage}.rs").read_text()


def applied(outcome: str) -> bool:
    return re.match(r"outcome: generation \d+ applied:", outcome) is not None


def answer_of(line: str) -> str:
    return re.sub(r" at_us=\d+$", "", line.removeprefix("answer "))


async def from_nothing(args: Args, stage: int) -> str:
    """The first answer of the stage's text built by blaze from nothing and started afresh."""
    step = f"from nothing {stage}"
    scratch = await daemon(args, f"s{stage}")
    try:
        scratch.source.write_text(text_of(stage))
        await scratch.build(step)
        running = await scratch.run(step)
        try:
            return answer_of(await running.wait_for(step, "answer "))
        finally:
            await running.stop()
    finally:
        await scratch.close(args.keep)


async def edit(step: str, live: Daemon, program: Running, stage: int) -> tuple[str, str, float]:
    """Writes the stage's text and builds it: what the program said of the patch, its first answer
    since, and the milliseconds from the build's start to the program having applied it."""
    live.source.write_text(text_of(stage))
    since = len(program.log)
    began = time.monotonic()
    out = await live.build(step)
    print(f"  build: {out.strip().splitlines()[-1] if out.strip() else ''}", flush=True)
    outcome = await program.wait_for(f"{step}: the program is told", "outcome:", since)
    took = (time.monotonic() - began) * 1000
    if not applied(outcome):
        return outcome, "", took
    answer = await program.wait_for(f"{step}: the new code runs", f"answer stage {stage}:", since)
    return outcome, answer_of(answer), took


async def main(args: Args) -> None:
    # Every stage's build from nothing is independent: each in a home of its own, all at once.
    async with asyncio.TaskGroup() as group:
        tasks = {stage: group.create_task(from_nothing(args, stage)) for stage in [0, *STAGES]}
    expected = {stage: task.result() for stage, task in tasks.items()}
    for stage, answer in expected.items():
        print(f"from nothing {stage}: {answer}", flush=True)

    live = await daemon(args, "live")
    program: Running | None = None
    try:
        live.source.write_text(text_of(0))
        await live.build("first build")
        program = await live.run("start")
        got = answer_of(await program.wait_for("start", "answer "))
        if got != expected[0]:
            raise StepError(f"start: the program says `{got}`, the build from nothing `{expected[0]}`")
        print(f"started on the daemon's code memory: {got}", flush=True)

        for stage, name in STAGES.items():
            step = f"stage {stage} ({name})"
            outcome, got, took = await edit(step, live, program, stage)
            print(
                f"{step}: {outcome.removeprefix('outcome: ')} [{took:.0f} ms from the build to applied]",
                flush=True,
            )
            if not applied(outcome):
                raise StepError(f"{step}: the edit was not applied as a patch: {outcome}")
            if got != expected[stage]:
                raise StepError(
                    f"{step}: the program says `{got}`, the build from nothing `{expected[stage]}`"
                )
            print(f"  {got}", flush=True)

        # A type the program has values of, changed in place, is the one edit that restarts.
        step = "stage 4 (change V2)"
        outcome, _, _ = await edit(step, live, program, 4)
        print(f"{step}: {outcome.removeprefix('outcome: ')}", flush=True)
        if "restart needed" not in outcome or "`struct V2`" not in outcome:
            raise StepError(f"{step}: the program was not told to restart because of `struct V2`: {outcome}")
        print("live types: V2 added, the code moved to it and V1 deleted in a running program, each a patch")
    finally:
        if program is not None:
            await program.stop()
        await live.close(args.keep)


if __name__ == "__main__":
    if len(sys.argv) != 2:
        raise SystemExit(__doc__)
    try:
        asyncio.run(main(parse(Args, sys.argv[1], SCRIPT)))
    except* StepError as group:
        for error in group.exceptions:
            print(f"error: {error}", file=sys.stderr)
        sys.exit(1)
