#!/usr/bin/env python3
"""Live edits end to end under blaze v2: a running dev program takes each edit of a function as a
patch applied in place, from the daemon's code memory (docs/v2/plan/design/compile-time.md).

usage: live_e2e.py '<json>'   (or a path to a JSON file, or - for stdin)

    {"blaze": "/abs/path/to/blaze", "edits": 5, "program": "live", "env": {}, "keep": false, "tmp": "/tmp"}

  blaze    the blaze executable under test (required)
  edits    how many edits the running program takes one after another (default 5)
  program  the program under harness/ that is edited: `live`, or another with the same
           `answer()` (default `live`)
  env      extra environment for every blaze command and the program (default none), e.g.
           {"BLAZE_SANDBOX": "off"} where no build-script sandbox can be made
  keep     keep the work directory and the daemon's home for a look after (default false)
  tmp      where the daemon's home and the work directory go (default /tmp): on the file
           system of the machine's blaze cache (`~/.cache/blaze`), which blaze clones from and
           never copies across (a tmpfs /tmp on Linux is not); short, as the home holds sockets

The program and the crates it links (`crates/`, `vendor/` and the root `Cargo.toml`, which
blaze-live's `workspace = true` lines read) are copied into a git repository of their own,
built, and started with `blaze run`, which starts it on the daemon's code memory. Then:
  edits    `answer()` is edited `edits` times while it runs: each has to arrive as a patch the
           program applies (`outcome: ... applied`) and the new answer has to be printed
  layout   a field is added to `Tally`, which the running loop holds: no patch can take that,
           so the program is told to start again, naming the type, and the build says so too
  restart  the program is stopped and `blaze run` again: it starts with the last edit, from
           the code memory every patch went into
  panic    an edit has `answer()` panic, and the panic unwinds through the patched code: the
           program says where it panicked and exits with a failure

The daemon runs in a home of its own; its profile's spans give each edit's time from the
build's start to the patch committed into the code memory, printed at the end with the
program's own time from the build's start to the new answer.

Python 3.13, stdlib only. Every step has a timeout; a failure names the step, the command,
its exit code and its output's tail.
"""

import asyncio
import json
import os
import shutil
import signal
import sys
import tempfile
import time
from dataclasses import dataclass, field
from pathlib import Path

HERE = Path(__file__).resolve().parent
ROOT = HERE.parent
BUILD_TIMEOUT = 900
APPLY_TIMEOUT = 60
COPY_TIMEOUT = 300
STOP_TIMEOUT = 30


@dataclass(frozen=True)
class Args:
    blaze: str
    edits: int = 5
    program: str = "live"
    env: dict[str, str] = field(default_factory=dict)
    keep: bool = False
    tmp: str = "/tmp"


def parse(raw: str) -> Args:
    text = sys.stdin.read() if raw == "-" else Path(raw).read_text() if not raw.lstrip().startswith("{") else raw
    value = json.loads(text)
    if not isinstance(value, dict):
        raise SystemExit(f"live_e2e.py takes one JSON object, not {type(value).__name__}")
    known = set(Args.__dataclass_fields__)
    unknown = sorted(set(value) - known)
    if unknown:
        raise SystemExit(f"live_e2e.py: unknown fields {unknown}; it takes {sorted(known)}")
    return Args(**value)


class StepError(Exception):
    pass


def tail(text: str, lines: int = 30) -> str:
    return "\n".join(text.strip().splitlines()[-lines:])


async def command(step: str, argv: list[str], cwd: Path, env: dict[str, str], timeout: float) -> str:
    """`argv` run to its end: its output, or a StepError naming the step, the command, the exit
    code and the output's tail."""
    try:
        async with asyncio.timeout(timeout):
            proc = await asyncio.create_subprocess_exec(
                *argv, cwd=cwd, env=env, stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.STDOUT
            )
            out, _ = await proc.communicate()
    except TimeoutError:
        raise StepError(f"{step}: timed out after {timeout}s\n  command: {' '.join(argv)}") from None
    text = out.decode(errors="replace")
    if proc.returncode != 0:
        raise StepError(f"{step}: exit code {proc.returncode}\n  command: {' '.join(argv)}\n  output tail:\n{tail(text)}")
    return text


def replace(source: Path, old: str, new: str) -> None:
    text = source.read_text()
    if old not in text:
        raise StepError(f"edit: `{old}` is not in {source}")
    source.write_text(text.replace(old, new))


def edit(source: Path, old: str, new: str) -> None:
    text = source.read_text()
    needle = f"fn answer() -> u64 {{\n    {old}\n}}"
    if needle not in text:
        raise StepError(f"edit: `{needle}` is not in {source}")
    source.write_text(text.replace(needle, f"fn answer() -> u64 {{\n    {new}\n}}"))


class Running:
    """A `blaze run` of the program, with what it said so far."""

    def __init__(self, proc: asyncio.subprocess.Process):
        self.proc = proc
        self.lines: asyncio.Queue[str] = asyncio.Queue()
        self.log: list[str] = []
        assert proc.stdout is not None
        self.pumper = asyncio.create_task(self.pump(proc.stdout))

    async def pump(self, stream: asyncio.StreamReader) -> None:
        while line := await stream.readline():
            await self.lines.put(line.decode(errors="replace").rstrip())

    async def wait_for(self, step: str, wanted: str, since: int | None = None) -> str:
        """The first line with `wanted`; with `since`, one already read from that index on counts
        too (the program may print the new answer before it says the patch applied)."""
        if since is not None:
            for line in self.log[since:]:
                if wanted in line:
                    return line
        try:
            async with asyncio.timeout(APPLY_TIMEOUT):
                while True:
                    line = await self.lines.get()
                    self.log.append(line)
                    if "no live edits:" in line:
                        raise StepError(f"{step}: the program runs without live edits: {line}\n  it said:\n  " + "\n  ".join(self.log[-20:]))
                    if wanted in line:
                        return line
        except TimeoutError:
            code = self.proc.returncode
            raise StepError(
                f"{step}: no `{wanted}` on the program's output in {APPLY_TIMEOUT}s (exit code {code}); it said:\n  "
                + "\n  ".join(self.log[-30:])
            ) from None

    async def stop(self) -> None:
        if self.proc.returncode is None:
            self.proc.send_signal(signal.SIGTERM)
            try:
                async with asyncio.timeout(STOP_TIMEOUT):
                    await self.proc.wait()
            except TimeoutError:
                self.proc.kill()
                await self.proc.wait()
        self.pumper.cancel()


async def start(blaze: str, project: Path, env: dict[str, str], step: str, answer: int) -> Running:
    proc = await asyncio.create_subprocess_exec(
        blaze, "run", cwd=project, env=env, stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.STDOUT
    )
    running = Running(proc)
    await running.wait_for(f"{step}: the program starts", "ready")
    await running.wait_for(f"{step}: the program runs the last edit", f"answer {answer}")
    return running


async def copy_tree(work: Path, program: str) -> Path:
    """The program and what it links, in a git repository of its own at `work`."""
    async with asyncio.TaskGroup() as group:
        for part in ("crates", "vendor"):
            group.create_task(
                command(f"copy {part}", ["rsync", "-a", "--exclude", "target", "--exclude", ".git", f"{ROOT / part}/", f"{work / part}/"],
                        ROOT, dict(os.environ), COPY_TIMEOUT)
            )
    shutil.copy2(ROOT / "Cargo.toml", work / "Cargo.toml")
    project = work / "harness" / program
    shutil.copytree(HERE / program, project, ignore=shutil.ignore_patterns("target", "Cargo.lock"))
    git = ["git", "-c", "user.name=live-e2e", "-c", "user.email=live-e2e@blaze.invalid"]
    await command("git init", [*git, "init", "-q"], work, dict(os.environ), COPY_TIMEOUT)
    await command("git add", [*git, "add", "-A"], work, dict(os.environ), COPY_TIMEOUT)
    await command("git commit", [*git, "commit", "-qm", "live_e2e"], work, dict(os.environ), COPY_TIMEOUT)
    return project


async def spans(blaze: str, project: Path, env: dict[str, str]) -> list[dict[str, object]]:
    """Each build of this run that committed into a dev executable's code memory, from the
    daemon's live recorder: when the build's first span started, when its last `dev run commit`
    ended (the patch is in the memory and announced), and the commits' own time (one per dev
    executable the build links)."""
    query = (
        "select c.build_id, b.first_ns, max(c.start_ns + c.dur_ns) as committed_ns, "
        "sum(c.dur_ns) as commit_ns, sum(c.self_cpu_ns) as commit_cpu_ns, count(*) as commits "
        "from spans c join (select build_id, min(start_ns) as first_ns from spans group by build_id) b "
        "using (build_id) where c.name = 'dev run commit' group by c.build_id, b.first_ns order by c.build_id"
    )
    out = await command("spans", [blaze, "debug", "sql", "--json", query], project, env, 120)
    rows = []
    for line in out.splitlines():
        line = line.strip()
        if line.startswith("{"):
            rows.append(json.loads(line))
    return rows


async def stop_daemon(blaze: str, home: Path, env: dict[str, str]) -> None:
    """The daemon of this run's home alone: `blaze stop`, and the process its lock names killed
    by pid when the stop does not end in time."""
    try:
        await command("blaze stop", [blaze, "stop"], home, env, STOP_TIMEOUT)
        return
    except StepError as error:
        print(f"warning: {error}", file=sys.stderr)
    lock = home / "v2" / "serve.lock"
    try:
        pid = int(lock.read_text().split()[0])
    except (OSError, ValueError, IndexError) as error:
        print(f"warning: no daemon pid in {lock}: {error}", file=sys.stderr)
        return
    try:
        os.kill(pid, signal.SIGKILL)
        print(f"warning: killed the daemon, pid {pid}, after `blaze stop` did not end", file=sys.stderr)
    except ProcessLookupError:
        pass


async def main(args: Args) -> None:
    blaze = str(Path(args.blaze).resolve())
    # Short: the daemon's sockets are in it, and macOS allows 104 bytes.
    home = Path(tempfile.mkdtemp(prefix="rj-live-", dir=args.tmp))
    work = Path(tempfile.mkdtemp(prefix="blaze-live-e2e-", dir=args.tmp))
    # BLAZE_LOCK: the copy leaves Cargo.lock out, throwaway workspaces with no committed lock: their builds may write lock files under CI (docs/cargo.md, "Lock files"); a typed --locked still wins.
    env = {**os.environ, "BLAZE_HOME": str(home), "RUST_BACKTRACE": "full", "BLAZE_LOCK": "write", **args.env}
    for name in ("CARGO_TARGET_DIR", "CARGO_INCREMENTAL", "RUSTUP_TOOLCHAIN"):
        env.pop(name, None)
    running: Running | None = None
    timings: list[tuple[int, float, float]] = []
    try:
        project = await copy_tree(work, args.program)
        source = project / "src" / "main.rs"
        await command("first build", [blaze, "build"], project, env, BUILD_TIMEOUT)
        running = await start(blaze, project, env, "first start", 1)
        print("program started on the daemon's code memory with answer 1", flush=True)

        last = 1 + args.edits
        for new in range(2, last + 1):
            edit(source, str(new - 1), str(new))
            since = len(running.log)
            began = time.time()
            out = await command(f"edit {new - 1} while running", [blaze, "build"], project, env, BUILD_TIMEOUT)
            built = time.time()
            applied = await running.wait_for(f"patch {new - 1} applied", "outcome:", since)
            if "applied" not in applied:
                raise StepError(f"patch {new - 1}: the program did not apply it: {applied}\n  build said:\n{tail(out, 10)}")
            line = await running.wait_for(f"new code of edit {new - 1} runs", f"answer {new}", since)
            at_us = int(line.rsplit("at_us=", 1)[1]) if "at_us=" in line else int(time.time() * 1e6)
            timings.append((new - 1, (built - began) * 1e3, (at_us / 1e6 - began) * 1e3))
            print(f"edit {new - 1}: {applied}", flush=True)
        print(f"live edits: answer 1 -> {last} in a running program, each a patch blaze-live applied", flush=True)

        replace(source, "struct Tally {\n    turns: u64,\n}", "struct Tally {\n    turns: u64,\n    edits: u64,\n}")
        replace(source, "Tally { turns: 0 }", "Tally { turns: 0, edits: 0 }")
        since = len(running.log)
        out = await command("layout edit while running", [blaze, "build"], project, env, BUILD_TIMEOUT)
        told = await running.wait_for("layout edit told", "outcome:", since)
        if "restart needed" not in told or "Tally" not in told:
            raise StepError(f"layout edit: the program was not told to start again for `Tally`: {told}\n  build said:\n{tail(out, 10)}")
        if "can't take this build as a patch" not in out:
            raise StepError(f"layout edit: the build did not say the running program has to start again:\n{tail(out, 20)}")
        print(f"layout edit: {told}", flush=True)

        await running.stop()
        running = await start(blaze, project, env, "restart", last)
        print(f"program started again with answer {last}: its executable is the code memory's, every patch in it", flush=True)

        edit(source, str(last), 'panic!("live panic")')
        await command("panic edit while running", [blaze, "build"], project, env, BUILD_TIMEOUT)
        await running.wait_for("patched panic", "panicked at")
        try:
            async with asyncio.timeout(APPLY_TIMEOUT):
                await running.proc.wait()
        except TimeoutError:
            raise StepError(f"patched panic: the program still runs {APPLY_TIMEOUT}s after it panicked") from None
        if running.proc.returncode == 0:
            raise StepError("patched panic: the program exited successfully")
        print("live unwind: the panic unwound through the patched answer", flush=True)

        rows = await spans(blaze, project, env)
        print("edit  build ms  edit-to-new-answer ms (client clocks)")
        for edit_no, build_ms, answer_ms in timings:
            print(f"{edit_no:>4}  {build_ms:>8.1f}  {answer_ms:>8.1f}")
        print("daemon spans, per build that committed: build start to patch committed, and the commits")
        print("build  start-to-committed ms  commits  commit ms  commit cpu ms")
        for row in rows:
            first, committed = int(row["first_ns"]), int(row["committed_ns"])
            print(
                f"{row['build_id']:>5}  {(committed - first) / 1e6:>21.1f}  {row['commits']:>7}  "
                f"{int(row['commit_ns']) / 1e6:>9.1f}  {int(row['commit_cpu_ns']) / 1e6:>13.1f}"
            )
    finally:
        if running is not None:
            await running.stop()
        await stop_daemon(blaze, home, env)
        if args.keep:
            print(f"kept: work {work}, home {home}", file=sys.stderr)
        else:
            shutil.rmtree(work, ignore_errors=True)


if __name__ == "__main__":
    if len(sys.argv) != 2:
        raise SystemExit(__doc__)
    try:
        asyncio.run(main(parse(sys.argv[1])))
    except StepError as error:
        print(f"error: {error}", file=sys.stderr)
        sys.exit(1)
