#!/usr/bin/env python3
"""Tiering end to end (macOS on AArch64, blaze built with `--features llvm`): a program that
takes edits runs under the daemon, which samples it and compiles its hot functions again with
LLVM, swapping them in as patches while it runs (docs/tiers.md).

usage: tiers.py          (BLAZE_BIN names the blaze built; blaze is found next
                          to it)

A copy of demo/lifebench (the Game of Life on a large grid, run flat out) is built by blaze
from nothing (a home, store and target directory of its own, BLAZE_TIER=off), then by a
daemon. Three checks:

1. promotion: the running program's hot functions reach level 2 (LLVM -O3) and level 3
   (LLVM -O3 + PGO), and what it prints is what the build from nothing prints, generation for
   generation;
2. an edit demotes: an edit of a promoted function that keeps what it does (`next_age`) is
   applied as a patch at once, the function is back at level 1, and the output stays the
   same;
3. BLAZE_TIER=off changes nothing: under a daemon started with it, nothing is sampled, no
   function leaves level 1, and the output is the build from nothing's.

Python 3.13, stdlib only. Every step has a timeout; a failure names the step.
"""

import asyncio
import os
import re
import shutil
import sys
import tempfile
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
ROOT = HERE.parent
BUILD_TIMEOUT = 900
# How long the program runs, and how long a level may take to be reached in it.
RUN_SECONDS = int(os.environ.get("TIERS_SECONDS", "30"))
LEVEL_TIMEOUT = 20
SIZE = os.environ.get("TIERS_SIZE", "160")
PIN = (HERE / "toolchain").read_text().strip()


class StepError(Exception):
    pass


async def run(step: str, cmd: list[str], cwd: Path, env: dict[str, str]) -> str:
    try:
        async with asyncio.timeout(BUILD_TIMEOUT):
            proc = await asyncio.create_subprocess_exec(
                *cmd, cwd=cwd, env=env, stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE
            )
            out, err = await proc.communicate()
    except TimeoutError:
        raise StepError(f"{step}: timed out after {BUILD_TIMEOUT}s\n  command: {' '.join(cmd)}") from None
    text = (out + err).decode(errors="replace")
    if proc.returncode != 0:
        tail = "\n".join(text.strip().splitlines()[-30:])
        raise StepError(f"{step}: exit code {proc.returncode}\n  command: {' '.join(cmd)}\n  output tail:\n{tail}")
    return text


async def build(step: str, cwd: Path, env: dict[str, str]) -> str:
    return await run(step, ["blaze", "build", "--locked"], cwd, env)


async def serve(project: Path, env: dict[str, str]) -> None:
    """Builds until the daemon serves the build, then once more after an edit: the first edit
    after a daemon starts writes the whole object again."""
    await build("first build", project, env)
    for _ in range(120):
        if "[blaze]" in await build("wait for the daemon", project, env):
            break
        await asyncio.sleep(0.5)
    else:
        raise StepError("wait for the daemon: no build said `[blaze]` in 120 tries")
    source = project / "src" / "main.rs"
    source.write_text(source.read_text() + "\n// the daemon's first edit\n")
    await build("first edit", project, env)


class Program:
    """The running program: its checksums, and the levels its rate lines report."""

    def __init__(self, proc: asyncio.subprocess.Process):
        self.proc = proc
        self.checksums: list[str] = []
        self.levels: dict[str, int] = {}
        self.log: list[str] = []
        self.applied: list[tuple[float, str]] = []
        self.events: list[str] = []

    async def pump_out(self) -> None:
        while line := await self.proc.stdout.readline():
            self.checksums.append(line.decode().rstrip())

    async def pump_err(self) -> None:
        while line := await self.proc.stderr.readline():
            text = line.decode(errors="replace").rstrip()
            self.log.append(text)
            if text.startswith("tier: "):
                self.events.append(re.sub(r"^t=\S+ ", "", text.removeprefix("tier: ")))
            if text.startswith("live:") and "applied" in text:
                self.applied.append((time.monotonic(), text))
            if text.startswith("rate "):
                skip = ("t", "gens_per_s", "sampler_permille")
                self.levels = {k: int(v) for k, v in re.findall(r"(\S+)=(\d+)", text) if k not in skip}

    async def wait_for(self, step: str, check, timeout: float = LEVEL_TIMEOUT) -> None:
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            if check(self.levels):
                return
            if self.proc.returncode is not None:
                break
            await asyncio.sleep(0.2)
        raise StepError(f"{step}: not seen in {timeout}s; levels {self.levels}; it said:\n  " + "\n  ".join(self.log[-15:]))


async def start(binary: Path, cwd: Path, env: dict[str, str], seconds: int) -> tuple[Program, list[asyncio.Task]]:
    proc = await asyncio.create_subprocess_exec(
        str(binary), cwd=cwd, env=dict(env, LIFE_SECONDS=str(seconds), LIFE_SIZE=SIZE),
        stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE,
    )
    program = Program(proc)
    return program, [asyncio.create_task(program.pump_out()), asyncio.create_task(program.pump_err())]


async def finish(program: Program, tasks: list[asyncio.Task]) -> None:
    await asyncio.wait_for(program.proc.wait(), RUN_SECONDS + 60)
    await asyncio.gather(*tasks)


def same_as_scratch(step: str, ours: list[str], scratch: list[str]) -> None:
    if not ours:
        raise StepError(f"{step}: the program printed no checksum")
    if ours != scratch[: len(ours)]:
        first = next(i for i, (a, b) in enumerate(zip(ours, scratch)) if a != b)
        raise StepError(f"{step}: generation {100 * (first + 1)} differs from the build from nothing:\n"
                        f"  ours          {ours[first]}\n  from nothing  {scratch[first]}")


def live_dir(binary: Path) -> Path | None:
    """The directory blaze-live keeps for the executable, named after its LC_UUID."""
    data = binary.read_bytes()
    at = data.find(b"\x1b\x00\x00\x00\x18\x00\x00\x00")  # LC_UUID, 24 bytes
    if at < 0:
        return None
    return Path.home() / ".cache" / "blaze" / "live" / data[at + 8 : at + 24].hex()


async def main() -> None:
    bin_dir = Path(os.environ["BLAZE_BIN"]).resolve().parent
    # Short: a worker's socket is in the daemon's home, and macOS takes 104 bytes of path.
    work = Path(tempfile.mkdtemp(prefix="rjt-", dir="/tmp" if Path("/tmp").is_dir() else None))
    try:
        shutil.copytree(ROOT / "crates" / "blaze-live", work / "crates" / "blaze-live")
        shutil.copytree(ROOT / "demo" / "lifebench", work / "demo" / "lifebench", ignore=shutil.ignore_patterns("target"))
        project = work / "demo" / "lifebench"
        base = dict(os.environ, PATH=f"{bin_dir}{os.pathsep}{os.environ['PATH']}", RUSTUP_TOOLCHAIN=PIN)
        for name in ("CARGO_INCREMENTAL", "BLAZE_TIER"):
            base.pop(name, None)

        # What blaze's build from nothing prints, with nothing tiered: it runs a fixed number of
        # generations, as many as a tiered run got to.
        scratch_env = dict(base, CARGO_TARGET_DIR=str(work / "scratch"), BLAZE_HOME=str(work / "home-scratch"),
                           BLAZE_TIER="off")
        await build("build from nothing", project, scratch_env)

        async def scratch(count: int) -> list[str]:
            proc = await asyncio.create_subprocess_exec(
                str(work / "scratch" / "debug" / "lifebench"), cwd=project,
                env=dict(scratch_env, LIFE_GENERATIONS=str(100 * count), LIFE_SIZE=SIZE),
                stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.DEVNULL,
            )
            out, _ = await proc.communicate()
            return out.decode().splitlines()

        # 1 and 2: a daemon that tiers.
        env = dict(base, CARGO_TARGET_DIR=str(work / "tiered"), BLAZE_HOME=str(work / "home-tiered"))
        await serve(project, env)
        binary = work / "tiered" / "debug" / "lifebench"
        program, tasks = await start(binary, project, env, RUN_SECONDS)
        await program.wait_for("level 2 (LLVM -O3)", lambda levels: any(v >= 2 for v in levels.values()))
        print(f"promoted to level 2: {program.levels}")
        await program.wait_for("level 3 (LLVM -O3 + PGO)", lambda levels: levels.get("neighbours", 0) == 3 or levels.get("step", 0) == 3)
        await program.wait_for("next_age at level 3", lambda levels: levels.get("next_age", 0) == 3)
        print(f"promoted to level 3: {program.levels}")

        source = project / "src" / "main.rs"
        text = source.read_text()
        needle = "if n == 2 || n == 3 { age + 1 } else { 0 }"
        if needle not in text:
            raise StepError(f"edit: `{needle}` is not in {source}")
        events_before = len(program.events)
        source.write_text(text.replace(needle, "if n == 3 || n == 2 { age + 1 } else { 0 }"))
        await build("edit of next_age while it runs", project, env)
        deadline = time.monotonic() + 10
        # A function is shown by its path: `lifebench::next_age`.
        demoted = lambda: [it for it in program.events[events_before:] if re.match(r"(?:\S*::)?next_age: back to Cranelift", it)]
        while not demoted() and time.monotonic() < deadline:
            await asyncio.sleep(0.05)
        if not demoted():
            raise StepError("edit of next_age: the daemon did not send it back to level 1; events:\n  " + "\n  ".join(program.events[-10:]))
        log = "".join(p.read_text(errors="replace") for p in (work / "home-tiered").rglob("log"))
        took = re.findall(r"change: 1 files\n(?:.*\n)*?edit-to-binary: (\S+)", log)
        print(f"edit demoted next_age: {demoted()[0]} (daemon edit-to-binary {took[-1] if took else '?'})")
        await program.wait_for("next_age hot again and promoted again", lambda levels: levels.get("next_age", 0) >= 2)
        print(f"promoted again once hot: {program.levels}")
        await finish(program, tasks)
        same_as_scratch("output of the tiered program", program.checksums, await scratch(len(program.checksums)))
        print(f"output: {len(program.checksums)} checksums, all the build from nothing's")

        # 3: BLAZE_TIER=off.
        source.write_text(text)
        env = dict(base, CARGO_TARGET_DIR=str(work / "off"), BLAZE_HOME=str(work / "home-off"), BLAZE_TIER="off")
        await serve(project, env)
        binary = work / "off" / "debug" / "lifebench"
        program, tasks = await start(binary, project, env, min(RUN_SECONDS, 12))
        await finish(program, tasks)
        if any(v > 1 for v in program.levels.values()):
            raise StepError(f"BLAZE_TIER=off: functions left level 1: {program.levels}")
        live = live_dir(binary)
        if live is not None and (live / "sample").exists():
            raise StepError(f"BLAZE_TIER=off: the daemon asked the program to sample ({live / 'sample'})")
        logs = "".join(p.read_text(errors="replace") for p in (work / "home-off").rglob("log"))
        if "tier: off" not in logs:
            raise StepError("BLAZE_TIER=off: the worker did not say tiering is off")
        same_as_scratch("output with BLAZE_TIER=off", program.checksums, await scratch(len(program.checksums)))
        print(f"BLAZE_TIER=off: nothing sampled, nothing tiered, {len(program.checksums)} checksums, all the build from nothing's")
    finally:
        for home in ("home-scratch", "home-tiered", "home-off"):
            stop = dict(os.environ, BLAZE_HOME=str(work / home), PATH=f"{bin_dir}{os.pathsep}{os.environ['PATH']}")
            try:
                proc = await asyncio.create_subprocess_exec("blaze", "stop", env=stop, stdout=asyncio.subprocess.DEVNULL, stderr=asyncio.subprocess.DEVNULL)
                await asyncio.wait_for(proc.wait(), 30)
            except (OSError, TimeoutError):
                pass
        shutil.rmtree(work, ignore_errors=True)


if __name__ == "__main__":
    if sys.platform != "darwin" or os.uname().machine != "arm64":
        print("tiers.py: live edits run on macOS on AArch64 only; nothing to check here")
        sys.exit(0)
    try:
        asyncio.run(main())
    except StepError as error:
        print(f"error: {error}", file=sys.stderr)
        sys.exit(1)
