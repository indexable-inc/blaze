#!/usr/bin/env python3
"""Save-to-running latency of live edits under blaze v2 (macOS on AArch64), edit by edit.

usage: uv run --frozen --no-dev python harness/live_latency.py '<json>'   (or a file, or - for stdin)

    {"blaze": "/abs/path/to/blaze", "program": "probe", "edits": 10, "json": null, "machine": null, "tmp": "/tmp"}

  blaze    the blaze executable under test (required)
  program  what is edited (default `probe`): harness/live (`probe`: a few functions and
           blaze-live), the Game of Life demo (`life`: demo/life with eframe, run without its
           window with BLAZE_LIFE_HEADLESS=1), a bevy app (`bevy`: harness/live-bevy, a
           simulation of ships, rocks and shots on bevy's ECS with generic systems, without a
           window) or an egui app (`egui`: harness/live-egui, a ledger whose whole UI egui lays
           out and tessellates every frame without a window). Each applies each patch as it
           arrives, as the demo's window does
  edits    how many edits are timed (default 10); one more before them is a warm-up
  json     where every edit's numbers go, with what was measured where (default none)
  machine  the host's name in the JSON (default the Mac's model)
  tmp      where the daemon's home and the work directory go (default /tmp), as live_e2e.py's

The program and the crates it links are copied into a git repository of their own as
live_e2e.py does, built, and started with `blaze run`, on the daemon's code memory. Then each
edit changes the number one function returns, written to the file, and `blaze build` asks the
daemon for the build, which patches the running program: one edit at a time, each after the last
is running. Each is timed from just before the file is written (t0):

  total    t0 to the program saying the patch applied (blaze-live's `outcome: generation N
           applied` and its at_us stamp), what the demo shows as "edit to running"
  running  t0 to the program first running the new number
  reply    t0 to `blaze build`'s answer

The daemon's spans then give each build that committed a patch its time from the build's first
span to the patch committed into the code memory (`dev run commit`), as live_e2e.py prints them.

The tables (median, p90, p95, p99, min, max) go to stdout and to $GITHUB_STEP_SUMMARY when it is
set. `json` gets {"program", "date", "commit", "run_url", "host": {"name", "cpu", "cores",
"memory_gib", "os", "battery"}, "modes": {"build": [row, ...]}, "spans": [...], "counts": {...}},
a row having at least "total" in ms; harness/benchhistory.py `live` reads it. A run that fails
writes the edits before the failure, and its error under counts.failed. "battery" is what
`pmset -g batt` says ("AC" or "battery 87%").

Python 3.13; the input is read with the workspace's typed_input. Every step has a timeout; a
failure names the step, the command, its exit code and its output's tail.
"""

import asyncio
import datetime
import io
import json
import os
import platform
import re
import shutil
import statistics
import subprocess
import sys
import tempfile
import time
from dataclasses import dataclass, field
from pathlib import Path
from typing import Literal

from live_e2e import BUILD_TIMEOUT, COPY_TIMEOUT, ROOT, Running, StepError, command, spans, stop_daemon
from typed_input import parse

SCRIPT = "live_latency.py"
MODE = "build"
COLUMNS = ["total", "running", "reply"]


@dataclass(frozen=True)
class Args:
    blaze: str
    program: Literal["probe", "life", "bevy", "egui"] = "probe"
    edits: int = 10
    json: str | None = None
    machine: str | None = None
    tmp: str = "/tmp"


@dataclass
class Program:
    """A program to edit while it runs: where its crate is, and the function edited."""

    name: str
    project: Path
    function: str
    value: int
    env: dict[str, str] = field(default_factory=dict[str, str])

    def needle(self, value: int) -> str:
        return f"fn {self.function}() -> u64 {{\n    {value}\n}}"

    def edited(self, text: str, old: int, new: int) -> str:
        if self.needle(old) not in text:
            raise StepError(f"edit: `{self.needle(old)}` is not in {self.source}")
        return text.replace(self.needle(old), self.needle(new))

    @property
    def source(self) -> Path:
        return self.project / "src" / "main.rs"


# Where each program's crate is in the repository, the function edited, and its value now.
PROGRAMS: dict[str, tuple[Path, str, int, dict[str, str]]] = {
    "probe": (Path("harness/live"), "answer", 1, {}),
    "life": (Path("demo/life"), "step_ms", 80, {"BLAZE_LIFE_HEADLESS": "1"}),
    "bevy": (Path("harness/live-bevy"), "answer", 1, {}),
    "egui": (Path("harness/live-egui"), "answer", 1, {}),
}


async def copy_program(name: str, work: Path) -> Program:
    """The program and what it links (`crates/`, `vendor/`, the root `Cargo.toml` its
    `workspace = true` lines read), in a git repository of its own at `work`."""
    at, function, value, env = PROGRAMS[name]
    async with asyncio.TaskGroup() as group:
        for part in ("crates", "vendor"):
            group.create_task(
                command(f"copy {part}", ["rsync", "-a", "--exclude", "target", "--exclude", ".git", f"{ROOT / part}/", f"{work / part}/"],
                        ROOT, dict(os.environ), COPY_TIMEOUT)
            )
    shutil.copy2(ROOT / "Cargo.toml", work / "Cargo.toml")
    shutil.copytree(ROOT / at, work / at, ignore=shutil.ignore_patterns("target"))
    git = ["git", "-c", "user.name=live-latency", "-c", "user.email=live-latency@blaze.invalid"]
    await command("git init", [*git, "init", "-q"], work, dict(os.environ), COPY_TIMEOUT)
    await command("git add", [*git, "add", "-A"], work, dict(os.environ), COPY_TIMEOUT)
    await command("git commit", [*git, "commit", "-qm", "live_latency"], work, dict(os.environ), COPY_TIMEOUT)
    return Program(name, work / at, function, value, dict(env))


def now_us() -> int:
    return time.time_ns() // 1000


def at_us(line: str, step: str) -> int:
    found = re.search(r"at_us=(\d+)", line)
    if not found:
        raise StepError(f"{step}: the program's line has no at_us stamp: {line}")
    return int(found.group(1))


def row_of(t0: int, reply: int, applied: str, running: str) -> dict[str, float]:
    return {
        "total": (at_us(applied, "timing") - t0) / 1e3,
        "running": (at_us(running, "timing") - t0) / 1e3,
        "reply": (reply - t0) / 1e3,
    }


def percentile(values: list[float], p: float) -> float:
    """The value `p` percent of the way up `values`, sorted (as benchhistory.py takes it)."""
    values = sorted(values)
    return values[min(len(values) - 1, int(p / 100 * len(values)))]


def summarize(title: str, rows: list[dict[str, float]], columns: list[str]) -> str:
    out = [f"### {title}, ms", "", "| step | n | median | p90 | p95 | p99 | min | max |", "|---|---|---|---|---|---|---|---|"]
    for key in columns:
        values = sorted(row[key] for row in rows if key in row)
        if not values:
            continue
        cells = [statistics.median(values), *(percentile(values, p) for p in (90, 95, 99)), values[0], values[-1]]
        out.append(f"| {key} | {len(values)} | " + " | ".join(f"{it:.2f}" for it in cells) + " |")
    return "\n".join(out)


async def start(blaze: str, program: Program, env: dict[str, str]) -> Running:
    """`blaze run` of the program, once it runs the number its source has now."""
    proc = await asyncio.create_subprocess_exec(
        blaze, "run", cwd=program.project, env=dict(env, **program.env),
        stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.STDOUT,
    )
    running = Running(proc)
    await running.wait_for("program start", "ready")
    await running.wait_for("program runs the source", f"answer {program.value} ")
    return running


async def one_by_one(args: Args, blaze: str, program: Program, running: Running, env: dict[str, str],
                     rows: list[dict[str, float]]) -> None:
    """Times `args.edits` edits into `rows`, each as it is done, so a failure keeps the ones
    before it. The first edit is a warm-up: not counted."""
    for nth in range(args.edits + 1):
        new = program.edited(program.source.read_text(), program.value, program.value + 1)
        since = len(running.log)
        t0 = now_us()
        program.source.write_text(new)
        out = await command(f"edit {nth}", [blaze, "build"], program.project, env, BUILD_TIMEOUT)
        reply = now_us()
        program.value += 1
        applied = await running.wait_for(f"edit {nth} applied", "outcome:", since)
        if "applied" not in applied:
            said = "\n".join(out.strip().splitlines()[-10:])
            raise StepError(f"edit {nth}: the program did not take the patch: {applied}\n  build said:\n{said}")
        # The new code can run before the apply thread says it was applied.
        ran = await running.wait_for(f"edit {nth} runs", f"answer {program.value} ", since)
        row = row_of(t0, reply, applied, ran)
        print(f"{MODE} {nth}: " + " ".join(f"{k}={row[k]:.2f}" for k in COLUMNS), flush=True)
        if nth:
            rows.append(row)


def run(*cmd: str) -> str:
    """What a command of the host's description said, or nothing when it is not there."""
    try:
        return subprocess.run(cmd, capture_output=True, text=True, timeout=30, check=False).stdout.strip()
    except (OSError, subprocess.SubprocessError):
        return ""


def host(machine: str | None) -> dict[str, object]:
    """What the numbers were measured on, and whether it ran on its battery."""
    info: dict[str, object] = {"name": machine, "cpu": None, "cores": os.cpu_count(), "memory_gib": None,
                               "os": f"{platform.system()} {platform.release()}", "battery": None}
    if sys.platform == "darwin":
        model = run("sysctl", "-n", "hw.model")
        named = re.search(r"Model Name: (.+)", run("system_profiler", "SPHardwareDataType"))
        info["name"] = info["name"] or (f"{named.group(1)} ({model})" if named else model)
        info["cpu"] = run("sysctl", "-n", "machdep.cpu.brand_string") or None
        memory = run("sysctl", "-n", "hw.memsize")
        info["memory_gib"] = round(int(memory) / 2**30) if memory.isdigit() else None
        info["os"] = f"macOS {run('sw_vers', '-productVersion')}"
        batt = run("pmset", "-g", "batt")
        if "AC Power" in batt:
            info["battery"] = "AC"
        elif found := re.search(r"(\d+)%", batt):
            info["battery"] = f"battery {found.group(1)}%"
    else:
        info["name"] = info["name"] or platform.node()
        info["cpu"] = platform.processor() or None
    return info


def metadata(args: Args) -> dict[str, object]:
    server, repo, run_id = (os.environ.get(k) for k in ("GITHUB_SERVER_URL", "GITHUB_REPOSITORY", "GITHUB_RUN_ID"))
    return {
        "program": args.program,
        "date": datetime.datetime.now(datetime.UTC).strftime("%Y-%m-%dT%H:%M:%SZ"),
        "commit": os.environ.get("GITHUB_SHA") or run("git", "-C", str(ROOT), "rev-parse", "HEAD") or None,
        "run_url": f"{server}/{repo}/actions/runs/{run_id}" if server and repo and run_id else None,
        "host": host(args.machine),
    }


def append(path: Path, text: str) -> None:
    with path.open("a") as f:
        f.write(text)


async def main(args: Args) -> None:
    # Lines as they happen, in order with the errors, also into a pipe.
    if isinstance(sys.stdout, io.TextIOWrapper):
        sys.stdout.reconfigure(line_buffering=True)  # pyright: ignore[reportUnknownMemberType]
    blaze = str(Path(args.blaze).resolve())
    # Short: the daemon's sockets are in it, and macOS allows 104 bytes.
    home = Path(tempfile.mkdtemp(prefix="rj-lat-", dir=args.tmp))
    work = Path(tempfile.mkdtemp(prefix="blaze-live-latency-", dir=args.tmp))
    # BLAZE_LOCK: the programs are copies, some without their lock, throwaway workspaces with no committed lock: their builds may write lock files under CI (docs/cargo.md, "Lock files"); a typed --locked still wins.
    env = {**os.environ, "BLAZE_HOME": str(home), "RUST_BACKTRACE": "full", "BLAZE_LOCK": "write"}
    for name in ("CARGO_TARGET_DIR", "CARGO_INCREMENTAL", "RUSTUP_TOOLCHAIN"):
        env.pop(name, None)
    meta = metadata(args)
    print(f"{meta['host']}")
    running: Running | None = None
    rows: list[dict[str, float]] = []
    built: list[dict[str, object]] = []
    counted: dict[str, object] = {}
    tables: list[str] = []
    failed: StepError | None = None
    try:
        program = await copy_program(args.program, work)
        await command("first build", [blaze, "build"], program.project, env, BUILD_TIMEOUT)
        running = await start(blaze, program, env)
        print(f"{program.name}: running on the daemon's code memory, {len(program.source.read_text().splitlines())}-line {program.source.name}")
        await one_by_one(args, blaze, program, running, env, rows)
        tables.append(summarize(f"{program.name}: {len(rows)} edits one at a time", rows, COLUMNS))
        built = await spans(blaze, program.project, env)
        committed = [{"start-to-committed": (int(str(it["committed_ns"])) - int(str(it["first_ns"]))) / 1e6,
                      "commit": int(str(it["commit_ns"])) / 1e6} for it in built]
        tables.append(summarize(f"{program.name}: the daemon's spans, per build that committed a patch", committed,
                                ["start-to-committed", "commit"]))
    except StepError as error:
        # The edits before it keep their numbers; the JSON says how the run failed.
        failed = error
        counted["failed"] = {"mode": MODE, "error": str(error)}
    finally:
        if running is not None:
            await running.stop()
        await stop_daemon(blaze, home, env)
        shutil.rmtree(work, ignore_errors=True)
        shutil.rmtree(home, ignore_errors=True)

    text = "\n\n".join(tables)
    print(text)
    host_info = meta["host"]
    assert isinstance(host_info, dict)
    if summary := os.environ.get("GITHUB_STEP_SUMMARY"):
        said = f"{args.program} on {host_info['name']} ({host_info['cpu']}, {host_info['battery'] or 'no battery'})\n\n{text}\n"
        await asyncio.to_thread(append, Path(summary), said)
    if args.json:
        modes = {MODE: rows} if rows else {}
        Path(args.json).write_text(json.dumps(dict(meta, modes=modes, spans=built, counts=counted), indent=1))
    if failed:
        raise failed


if __name__ == "__main__":
    if len(sys.argv) != 2:
        raise SystemExit(__doc__)
    try:
        asyncio.run(main(parse(Args, sys.argv[1], SCRIPT)))
    except StepError as error:
        print(f"error: {error}", file=sys.stderr)
        sys.exit(1)
