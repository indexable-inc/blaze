#!/usr/bin/env python3
"""Save-to-running latency of live edits (macOS on AArch64, Windows on x86-64), step by step.

usage: live_latency.py [--program probe|life|bevy|egui] [cargo ...]
       (BLAZE_BIN names the blaze built; blaze is found next to it)

The program is harness/live (`probe`, the default: a few functions and blaze-live), the Game
of Life demo (`life`: demo/life with eframe, a 38 MB debug executable, run without its window
with BLAZE_LIFE_HEADLESS=1), a bevy app (`bevy`: harness/live-bevy, a simulation of ships,
rocks and shots on bevy's ECS with generic systems, without a window, one 600-line file, 21 MB)
or an egui app (`egui`: harness/live-egui, a ledger whose whole UI egui lays out and tessellates
every frame without a window, generic widgets, one 580-line file). Each applies each patch as it
arrives, as the demo's window does. It is built and started as live_e2e.py does, then takes
LATENCY_EDITS edits (default 10) in cargo mode: the file is written and `blaze build` asks
the daemon for the build, as the demo's launcher did on each save (its 200 ms mtime poll, 100 ms
on average, comes on top and is not in these numbers). One edit at a time, each after the last
is running: an isolated edit.

The modes socket, burst, replay and restart sent the demo's `edit` lines to daemon.sock, which
nothing reads since the demo's editor commits through blaze-api (Open, Commit [Op::Write],
Build). They are refused, by name, until they come back as clients of blaze_api::client.

Each edit is timed from just before the file is written (t0) to the program having the new code
in place, and split by the epoch stamps the worker (`request:`, `live: ... at_us=`) and the
program (`outcome: ... at_us=`) print. An edit whose worker printed neither stops the run,
naming the stamp:

  to-worker   t0 to the worker reading the request (cargo and the daemon's hop)
  worker      the request to the patch left for the program (`timeline-us:` splits it)
  delivery    the patch file written to the program's thread reading it
  wait        read to applied: the program's thread taking it up
  apply       mapping the patch and writing the jumps
  total       t0 to applied, what the demo shows as "edit to running" (the old in-process
              prototype timed the same span: from just before the edit left the editor to
              the new code in place, with no frame in between)
  reply       t0 to the client's answer

The tables (median, p90, p95, p99, min, max) go to stdout and to $GITHUB_STEP_SUMMARY when it is
set. $LATENCY_JSON, when set, gets every edit's numbers with what was measured where:
{"program", "date", "commit", "run_url", "host": {"name", "cpu", "cores", "memory_gib", "os",
"battery"}, "modes": {mode: [row, ...]}, "counts": {mode: {...}}}, a row having at least
"total" in ms; harness/benchhistory.py `live` reads it. A mode that fails leaves the modes
before it in the JSON, and its error under counts.failed. The host's name is $BLAZE_BENCH_MACHINE
or the Mac's model; "battery" is what `pmset -g batt` says ("AC" or "battery 87%").
LATENCY_TARGET_DIR keeps the programs' target directory across runs (bevy's dependencies take
minutes to build). Python 3.13, stdlib only.
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
from dataclasses import dataclass
from pathlib import Path

from live_e2e import ROOT, HERE, StepError, build, daemon_home, pump, serving, show_daemon_log, wait_for

EDITS = int(os.environ.get("LATENCY_EDITS", "10"))
# The modes that sent the demo's `edit` lines to daemon.sock, which nothing reads since the
# editor commits through blaze-api (Open, Commit [Op::Write], Build); they come back as
# clients of blaze_api::client.
EDIT_LINES = "nothing reads `edit` lines on daemon.sock any more; it needs a blaze_api::client (Open, Commit, Build) port"
GONE = {mode: EDIT_LINES for mode in ("socket", "burst", "replay", "restart")}
COLUMNS = ["to-worker", "worker", "delivery", "wait", "apply", "total", "reply"]
# What an executable's name ends with where cargo writes it.
EXE = ".exe" if os.name == "nt" else ""


@dataclass
class Program:
    """A program to edit while it runs: where its crate is, and the function edited."""

    name: str
    project: Path
    binary: str
    function: str
    value: int
    env: dict[str, str]

    def needle(self, value: int) -> str:
        return f"fn {self.function}() -> u64 {{\n    {value}\n}}"

    def edited(self, text: str, old: int, new: int) -> str:
        if self.needle(old) not in text:
            raise StepError(f"edit: `{self.needle(old)}` is not in {self.source}")
        return text.replace(self.needle(old), self.needle(new))

    @property
    def source(self) -> Path:
        return self.project / "src" / "main.rs"


def copy_program(name: str, work: Path) -> Program:
    shutil.copytree(ROOT / "crates" / "blaze-live", work / "crates" / "blaze-live")
    ignore = shutil.ignore_patterns("target")
    match name:
        case "probe":
            shutil.copytree(HERE / "live", work / "harness" / "live", ignore=ignore)
            return Program(name, work / "harness" / "live", "liveprobe", "answer", 1, {})
        case "life":
            shutil.copytree(ROOT / "demo" / "life", work / "demo" / "life", ignore=ignore)
            env = {"BLAZE_LIFE_HEADLESS": "1"}
            return Program(name, work / "demo" / "life", "life", "step_ms", 80, env)
        case "bevy" | "egui":
            shutil.copytree(HERE / f"live-{name}", work / "harness" / f"live-{name}", ignore=ignore)
            return Program(name, work / "harness" / f"live-{name}", f"live{name}", "answer", 1, {})
    raise StepError(f"unknown program {name!r}: probe, life, bevy or egui")


def now_us() -> int:
    return time.time_ns() // 1000


class Logs:
    """The worker logs, read from where the last edit left off."""

    def __init__(self) -> None:
        self.offsets: dict[Path, int] = {}
        self.skip()

    def files(self) -> list[Path]:
        # A worker that was a spare, started ahead of the checkout, keeps the spare's log.
        home = daemon_home()
        return sorted([*home.glob("workers/*/log"), *home.glob("spares/*/log")])

    def skip(self) -> None:
        for log in self.files():
            self.offsets[log] = log.stat().st_size

    def new(self) -> list[str]:
        lines = []
        for log in self.files():
            with log.open("rb") as f:
                f.seek(self.offsets.get(log, 0))
                data = f.read()
            self.offsets[log] = self.offsets.get(log, 0) + len(data)
            lines += data.decode(errors="replace").splitlines()
        return lines


def at_us(line: str) -> int | None:
    found = re.search(r"at_us=(\d+)", line)
    return int(found.group(1)) if found else None


def duration_ms(text: str) -> float:
    """A Rust `Duration` as Debug prints it: 1.2ms, 830.0µs, 2.1s."""
    found = re.match(r"([\d.]+)(ns|µs|us|ms|s)", text)
    if not found:
        return float("nan")
    scale = {"ns": 1e-6, "µs": 1e-3, "us": 1e-3, "ms": 1.0, "s": 1e3}[found.group(2)]
    return float(found.group(1)) * scale


def worker_steps(worker: list[str]) -> dict[str, list[float]]:
    """The steps the worker's log times, each with every value it had in `worker`."""
    steps: dict[str, list[float]] = {}
    for line in worker:
        if line.startswith(("timeline-us:", "apply-us:", "splice-us:")):
            prefix = {"t": "worker.", "a": "worker.apply.", "s": "worker.splice."}[line[0]]
            for step in line.split()[1:]:
                name, _, value = step.partition("=")
                if name != "bytes":
                    steps.setdefault(f"{prefix}{name}", []).append(int(value) / 1e3)
        elif line.startswith("live: edit") and (took_us := re.search(r"took_us=(\d+)", line)):
            # Laying the patch out for the program and leaving it there.
            steps.setdefault("worker.publish", []).append(int(took_us.group(1)) / 1e3)
        elif line.startswith("edit-to-binary:"):
            steps.setdefault("worker.edit-to-binary", []).append(duration_ms(line.split()[1]))
        elif line.startswith("link:") and "spliced" in line:
            steps.setdefault("worker.link", []).append(duration_ms(line.split()[1]))
    return steps


def breakdown(t0: int, reply: int, applied_line: str, worker: list[str]) -> dict[str, float]:
    requests = [it for line in worker if line.startswith("request:") and (it := at_us(line)) and it >= t0]
    published = [it for line in worker if line.startswith("live: edit") and (it := at_us(line)) and it >= t0]
    if not requests or not published:
        raise StepError(f"timing: no `request:` or `live: edit` stamp in the worker log since the edit:\n  " + "\n  ".join(worker[-30:]))
    found = re.search(r" in (\S+) after waiting (\S+) at_us=(\d+)", applied_line)
    if not found:
        raise StepError(f"timing: the program's outcome has no times: {applied_line}")
    took, waited, applied = duration_ms(found.group(1)), duration_ms(found.group(2)), int(found.group(3))
    request, publish = requests[0], published[-1]
    ms = lambda us: us / 1e3
    row = {
        "to-worker": ms(request - t0),
        "worker": ms(publish - request),
        "delivery": ms(applied - publish) - waited - took,
        "wait": waited,
        "apply": took,
        "total": ms(applied - t0),
        "reply": ms(reply - t0),
    }
    # One edit, one build: the last value of each step is this edit's.
    row.update({name: values[-1] for name, values in worker_steps(worker).items()})
    return row


def percentile(values: list[float], p: float) -> float:
    """The value `p` percent of the way up `values`, sorted (as benchhistory.py takes it)."""
    values = sorted(values)
    return values[min(len(values) - 1, int(p / 100 * len(values)))]


def summarize(title: str, rows: list[dict], columns: list[str] = COLUMNS) -> str:
    keys = columns + sorted({k for row in rows for k in row if k not in columns})
    out = [f"### {title}, ms", "", "| step | n | median | p90 | p95 | p99 | min | max |", "|---|---|---|---|---|---|---|---|"]
    for key in keys:
        values = sorted(row[key] for row in rows if isinstance(row.get(key), (int, float)))
        if not values:
            continue
        cells = [statistics.median(values), *(percentile(values, p) for p in (90, 95, 99)), values[0], values[-1]]
        out.append(f"| {key} | {len(values)} | " + " | ".join(f"{it:.2f}" for it in cells) + " |")
    return "\n".join(out)


class Running:
    """The program's output: each line in order, and when it ran each answer."""

    def __init__(self, lines: asyncio.Queue[str]) -> None:
        self.lines = lines
        self.log: list[str] = []

    async def wait_for(self, step: str, wanted: str) -> str:
        return await wait_for(step, self.lines, wanted, self.log)

    async def runs(self, step: str, value: int) -> int:
        """Waits for the program to run `value` (or newer) and says when it did."""
        pattern = re.compile(r"^answer (\d+) at_us=(\d+)")
        for line in self.log[self.seen :]:
            if (found := pattern.match(line)) and int(found.group(1)) >= value:
                return int(found.group(2))
        while True:
            line = await self.wait_for(step, "answer ")
            if (found := pattern.match(line)) and int(found.group(1)) >= value:
                return int(found.group(2))

    def drain(self) -> None:
        """Takes what the program said so far into the log, waiting for nothing."""
        while not self.lines.empty():
            self.log.append(self.lines.get_nowait())

    def mark(self) -> None:
        self.seen = len(self.log)

    seen = 0


class Launched:
    """A program started: the process, what it says, and when it first ran the answer."""

    def __init__(self, proc: asyncio.subprocess.Process, running: Running, pumper: asyncio.Task, started: int) -> None:
        self.proc, self.running, self.pumper, self.started = proc, running, pumper, started

    async def stop(self) -> None:
        self.pumper.cancel()
        if self.proc.returncode is None:
            self.proc.terminate()
            await self.proc.wait()


async def launch(program: Program, binary: Path, env: dict[str, str]) -> Launched:
    """Starts `binary` and waits for it to run the answer the source has now."""
    proc = await asyncio.create_subprocess_exec(
        str(binary), cwd=program.project, env=dict(env, **program.env),
        stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.STDOUT,
    )
    lines: asyncio.Queue[str] = asyncio.Queue()
    pumper = asyncio.create_task(pump(proc.stdout, lines))
    running = Running(lines)
    try:
        await running.wait_for("program start", "ready")
        started = await running.runs("program runs the edited source", program.value)
    except BaseException:
        pumper.cancel()
        proc.terminate()
        await proc.wait()
        raise
    return Launched(proc, running, pumper, started)


async def one_by_one(mode: str, program: Program, running: Running, env: dict[str, str], logs: Logs) -> list[dict[str, float]]:
    rows = []
    # The first edit of a mode is a warm-up: not counted.
    for nth in range(EDITS + 1):
        new = program.edited(program.source.read_text(), program.value, program.value + 1)
        logs.skip()
        running.mark()
        t0 = now_us()
        program.source.write_text(new)
        out = await build(f"{mode} edit {nth}", program.project, env)
        said = out.strip().splitlines()[-1] if out.strip() else ""
        reply = now_us()
        program.value += 1
        applied = await running.wait_for(f"{mode} edit {nth} applied", "outcome:")
        if "applied" not in applied:
            raise StepError(f"{mode} edit {nth}: the program did not take the patch: {applied}")
        # The new code can run before the apply thread says it was applied.
        await running.runs(f"{mode} edit {nth} runs", program.value)
        # Give the worker the moment it takes to log the rest of the build (the splice may
        # come after the answer).
        await asyncio.sleep(0.3)
        row = breakdown(t0, reply, applied, logs.new())
        print(f"{mode} {nth}: " + " ".join(f"{k}={row[k]:.2f}" for k in COLUMNS) + f"  ({said[-60:]})")
        if nth:
            rows.append(row)
    return rows


def run(*cmd: str) -> str:
    try:
        return subprocess.run(cmd, capture_output=True, text=True, timeout=30).stdout.strip()
    except (OSError, subprocess.SubprocessError):
        return ""


def host() -> dict:
    """What the numbers were measured on, and whether it ran on its battery."""
    info = {"name": os.environ.get("BLAZE_BENCH_MACHINE"), "cpu": None, "cores": os.cpu_count(), "memory_gib": None,
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


def metadata(program: str) -> dict:
    server, repo, run_id = (os.environ.get(k) for k in ("GITHUB_SERVER_URL", "GITHUB_REPOSITORY", "GITHUB_RUN_ID"))
    return {
        "program": program,
        "date": datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
        "commit": os.environ.get("GITHUB_SHA") or run("git", "-C", str(ROOT), "rev-parse", "HEAD") or None,
        "run_url": f"{server}/{repo}/actions/runs/{run_id}" if server and repo and run_id else None,
        "host": host(),
    }


async def main(program_name: str, modes: list[str]) -> None:
    # Lines as they happen, in order with the errors, also into a pipe.
    if isinstance(sys.stdout, io.TextIOWrapper):
        sys.stdout.reconfigure(line_buffering=True)
    bin_dir = Path(os.environ["BLAZE_BIN"]).resolve().parent
    work = Path(tempfile.mkdtemp(prefix="blaze-live-latency-"))
    target = Path(os.environ.get("LATENCY_TARGET_DIR") or work / "target").resolve()
    meta = metadata(program_name)
    print(f"{meta['host']}")
    live: list[Launched] = []
    results: dict[str, list[dict]] = {}
    counted: dict[str, dict] = {}
    tables = []
    mode, failed = "first build", None
    try:
        program = copy_program(program_name, work)
        env = dict(
            os.environ,
            PATH=f"{bin_dir}{os.pathsep}{os.environ['PATH']}",
            CARGO_TARGET_DIR=str(target),
            BLAZE_STATS="1",
        )
        env.pop("CARGO_INCREMENTAL", None)

        await build("first build", program.project, env)
        await serving(program.project, env)
        program.source.write_text(program.edited(program.source.read_text(), program.value, program.value + 1))
        program.value += 1
        await build("edit before start", program.project, env)
        binary = target / "debug" / (program.binary + EXE)
        print(f"{program.name}: {binary.stat().st_size / 1e6:.1f} MB executable, {len(program.source.read_text().splitlines())}-line {program.source.name}")
        live.append(await launch(program, binary, env))

        logs = Logs()
        for mode in modes:
            running = live[0].running
            rows = await one_by_one(mode, program, running, env, logs)
            tables.append(summarize(f"{program.name}, {mode}: {len(rows)} edits one at a time", rows))
            results[mode] = rows
    except StepError as error:
        # The modes before it keep their numbers; the JSON says which mode failed and how.
        failed = error
        counted["failed"] = {"mode": mode, "error": str(error)}
    finally:
        for it in live:
            await it.stop()
        shutil.rmtree(work, ignore_errors=True)

    text = "\n\n".join(tables)
    print(text)
    if summary := os.environ.get("GITHUB_STEP_SUMMARY"):
        with open(summary, "a") as f:
            f.write(f"{program_name} on {meta['host']['name']} ({meta['host']['cpu']}, {meta['host']['battery'] or 'no battery'})\n\n{text}\n")
    if out := os.environ.get("LATENCY_JSON"):
        Path(out).write_text(json.dumps(dict(meta, modes=results, counts=counted), indent=1))
    if failed:
        raise failed


if __name__ == "__main__":
    args = sys.argv[1:]
    program = "probe"
    if args[:1] == ["--program"]:
        program, args = args[1], args[2:]
    for mode in args:
        if mode in GONE:
            sys.exit(f"error: mode {mode!r} is gone: {GONE[mode]}")
        if mode != "cargo":
            sys.exit(f"error: unknown mode {mode!r}: cargo is the only one")
    try:
        asyncio.run(main(program, args or ["cargo"]))
    except StepError as error:
        print(f"error: {error}", file=sys.stderr)
        show_daemon_log()
        sys.exit(1)
