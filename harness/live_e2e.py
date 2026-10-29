#!/usr/bin/env python3
"""Live edits end to end (macOS on AArch64, Linux and Windows on x86-64): edit a function of a running
program, and the running program must run the new code, applied by blaze-live from the
daemon's patch.

usage: live_e2e.py          (BLAZE_BIN names the blaze built; blaze and blaze-rustc
                             are found next to it)
       LIVE_PROGRAM=live-alloc live_e2e.py
                            the same steps on harness/live-alloc, the program on the nightly's
                            `allocator_api` (a bump arena through `Vec<T, A>` and `Box<T, A>`)

A copy of harness/live (a program that polls blaze-live and prints `answer()`) is built by the
daemon until the daemon serves the build, edited once (the first edit after a daemon starts
writes the whole object again), started, and edited again LIVE_EDITS times (default 5). Each
edit has to arrive as a patch the program applies: `outcome: edit N applied` and the new answer
on its output. Then, while it runs:
  other     a second copy, a program of its own, is built, edited twice and started: its
            executable has its last edit, which a program that runs elsewhere changes nothing of
  restart   the first program is stopped and started again from its executable, which has
            the last edit
  panic     an edit has the answer panic, and the panic unwinds through the patched code: the
            program says where it panicked and exits with a failure

Python 3.13, stdlib only. Every step has a timeout; a failure names the step.
"""

import asyncio
import atexit
import os
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

HERE = Path(__file__).resolve().parent
ROOT = HERE.parent
os.environ.setdefault("RUSTUP_TOOLCHAIN", (HERE / "toolchain").read_text().strip())
BUILD_TIMEOUT = 900
APPLY_TIMEOUT = 60
# How many edits the running program takes one after another.
LIVE_EDITS = int(os.environ.get("LIVE_EDITS", "5"))
# The program under harness/ that is edited: `live`, or another with the same `answer()`.
PROGRAM = os.environ.get("LIVE_PROGRAM", "live")


class StepError(Exception):
    pass


async def build(step: str, cwd: Path, env: dict[str, str]) -> str:
    cmd = ["blaze", "build", "--locked"]
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


# What `blaze` says when the toolchain is one the daemon never compiles for, which no
# wait changes: another release than blaze's, or one without the standard library's source.
OTHER_TOOLCHAIN = ("compiles for", "source of the standard library")


async def serving(project: Path, env: dict[str, str]) -> None:
    """Builds until the daemon serves one. Until the daemon has loaded the workspace a build is
    cargo's, which `blaze` says why of; one it served says `[blaze]`."""
    why = []
    for _ in range(120):
        said = await build("wait for the daemon", project, env)
        if "[blaze]" in said:
            return
        why = [it for it in said.splitlines() if "blaze:" in it] or why
        if any(reason in line for line in why for reason in OTHER_TOOLCHAIN):
            raise StepError("wait for the daemon: the daemon builds nothing with this toolchain:\n  " + "\n  ".join(why))
        await asyncio.sleep(0.5)
    raise StepError("wait for the daemon: no build said `[blaze]` in 120 tries; the last said:\n  " + "\n  ".join(why))


def edit(source: Path, old: int, new: int) -> None:
    text = source.read_text()
    needle = f"fn answer() -> u64 {{\n    {old}\n}}"
    if needle not in text:
        raise StepError(f"edit: `{needle}` is not in {source}")
    source.write_text(text.replace(needle, f"fn answer() -> u64 {{\n    {new}\n}}"))


async def wait_for(step: str, lines: asyncio.Queue[str], wanted: str, log: list[str], since: int | None = None) -> str:
    """The first line with `wanted`. With `since`, a line already read into `log` from that
    index on counts too: the program may print the new answer before it says the patch applied."""
    if since is not None:
        for line in log[since:]:
            if wanted in line:
                return line
    try:
        async with asyncio.timeout(APPLY_TIMEOUT):
            while True:
                line = await lines.get()
                log.append(line)
                if wanted in line:
                    return line
    except TimeoutError:
        raise StepError(f"{step}: no `{wanted}` on the program's output in {APPLY_TIMEOUT}s; it said:\n  " + "\n  ".join(log[-20:])) from None


async def pump(stream: asyncio.StreamReader, lines: asyncio.Queue[str]) -> None:
    while line := await stream.readline():
        await lines.put(line.decode(errors="replace").rstrip())


class Running:
    """A program started from its executable, with what it said so far."""

    def __init__(self, proc: asyncio.subprocess.Process):
        self.proc = proc
        self.lines: asyncio.Queue[str] = asyncio.Queue()
        self.log: list[str] = []
        self.pumper = asyncio.create_task(pump(proc.stdout, self.lines))

    async def wait_for(self, step: str, wanted: str, since: int | None = None) -> str:
        return await wait_for(step, self.lines, wanted, self.log, since)

    async def stop(self) -> None:
        if self.proc.returncode is None:
            self.proc.terminate()
            await self.proc.wait()
        self.pumper.cancel()


async def start(executable: Path, cwd: Path, env: dict[str, str], step: str, answer: int) -> Running:
    proc = await asyncio.create_subprocess_exec(
        str(executable), cwd=cwd, env=env, stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.STDOUT,
    )
    running = Running(proc)
    await running.wait_for(f"{step}: program start", "ready")
    await running.wait_for(f"{step}: the program runs the last edit", f"answer {answer}")
    return running


async def main() -> None:
    own_home()
    bin_dir = Path(os.environ["BLAZE_BIN"]).resolve().parent
    work = Path(tempfile.mkdtemp(prefix="blaze-live-e2e-"))
    started: list[Running] = []
    try:
        shutil.copytree(ROOT / "crates" / "blaze-live", work / "crates" / "blaze-live")
        shutil.copytree(HERE / PROGRAM, work / "harness" / "live", ignore=shutil.ignore_patterns("target"))
        project = work / "harness" / "live"
        source = project / "src" / "main.rs"
        executable = work / "target" / "debug" / "liveprobe"
        env = dict(
            os.environ,
            PATH=f"{bin_dir}{os.pathsep}{os.environ['PATH']}",
            CARGO_TARGET_DIR=str(work / "target"),
            RUST_BACKTRACE="full",
        )
        env.pop("CARGO_INCREMENTAL", None)

        await build("first build", project, env)
        await serving(project, env)
        print("daemon is serving builds")

        edit(source, 1, 2)
        await build("edit before start", project, env)
        probe = await start(executable, project, env, "first start", 2)
        started.append(probe)
        print("program started with answer 2")

        # Edit after edit, as the demo's editor does: each has to arrive as a patch the running
        # program applies, none may link the program again.
        last = 2 + LIVE_EDITS
        for new in range(3, last + 1):
            edit(source, new - 1, new)
            since = len(probe.log)
            out = await build(f"edit {new - 2} while running", project, env)
            print("build:", out.strip().splitlines()[-1] if out.strip() else "")
            applied = await probe.wait_for(f"patch {new - 2} applied", "outcome:")
            print(applied)
            if "applied" not in applied:
                raise StepError(f"patch {new - 2} applied: the program did not take the patch: {applied}")
            await probe.wait_for(f"new code of edit {new - 2} runs", f"answer {new}", since)
        chain = " -> ".join(str(it) for it in range(1, last + 1))
        print(f"live edits: answer {chain} in a running program, each applied by blaze-live")

        # Another program, built and edited while this one runs, has its edits on disk.
        shutil.copytree(HERE / PROGRAM, work / "harness" / "other", ignore=shutil.ignore_patterns("target"))
        other = work / "harness" / "other"
        other_env = dict(env, CARGO_TARGET_DIR=str(work / "target-other"))
        await build("other: first build", other, other_env)
        await serving(other, other_env)
        edit(other / "src" / "main.rs", 1, 10)
        await build("other: edit", other, other_env)
        edit(other / "src" / "main.rs", 10, 11)
        await build("other: second edit", other, other_env)
        started.append(await start(work / "target-other" / "debug" / "liveprobe", other, other_env, "other", 11))
        await started.pop().stop()
        print("another program built while this one runs has its last edit")

        # The executable is for the next start, and has every edit the program took.
        await probe.stop()
        await build("restart: build", project, env)
        probe = await start(executable, project, env, "restart", last)
        started.append(probe)
        print(f"program started again with answer {last}")

        text = source.read_text()
        needle = f"fn answer() -> u64 {{\n    {last}\n}}"
        if needle not in text:
            raise StepError(f"panic edit: `{needle}` is not in {source}")
        source.write_text(text.replace(needle, 'fn answer() -> u64 {\n    panic!("live panic")\n}'))
        await build("panic edit while running", project, env)
        await probe.wait_for("patched panic", "panicked at")
        try:
            async with asyncio.timeout(APPLY_TIMEOUT):
                await probe.proc.wait()
        except TimeoutError:
            raise StepError(f"patched panic: the program still runs {APPLY_TIMEOUT}s after it panicked") from None
        if probe.proc.returncode == 0:
            raise StepError("patched panic: the process exited successfully")
        print("live unwind: the panic unwound through the patched answer")
    finally:
        for running in started:
            await running.stop()
        shutil.rmtree(work, ignore_errors=True)


def own_home() -> None:
    """Gives this run a daemon of its own when BLAZE_HOME names none: the default home
    (XDG_RUNTIME_DIR/blaze) is one daemon and socket for every harness and lane of the user,
    and a run that talks to another's daemon tests that daemon's binary and state, not its own."""
    if os.environ.get("BLAZE_HOME"):
        return
    # Short, under /tmp: the daemon's socket is in it, and macOS allows 104 bytes.
    home = tempfile.mkdtemp(prefix="rj-live-", dir=None if os.name == "nt" else "/tmp")
    os.environ["BLAZE_HOME"] = home

    def stop() -> None:
        # The daemon of this home alone, which nothing else talks to; its logs stay for a failure.
        blaze_cli = Path(os.environ["BLAZE_BIN"]).resolve().parent / "blaze"
        subprocess.run([str(blaze_cli), "blaze", "stop"], env=dict(os.environ, BLAZE_HOME=home),
                       capture_output=True, timeout=120)

    atexit.register(stop)


def daemon_home() -> Path:
    """Where the daemon keeps its socket and log, as `cargo::daemon::home` finds it."""
    if os.environ.get("BLAZE_HOME"):
        return Path(os.environ["BLAZE_HOME"])
    if os.environ.get("XDG_RUNTIME_DIR"):
        return Path(os.environ["XDG_RUNTIME_DIR"]) / "blaze"
    return Path.home() / ".cache" / "blaze"


def show_daemon_log(lines: int = 80) -> None:
    """The daemon's log, and those of its workers: a worker that died says why in its own."""
    home = daemon_home()
    logs = [home / "daemon.log", *sorted(home.glob("workers/*/log")), *sorted(home.glob("spares/*/log"))]
    for log in logs:
        try:
            tail = log.read_text(errors="replace").splitlines()[-lines:]
        except OSError as why:
            print(f"no log at {log}: {why}", file=sys.stderr)
            continue
        print(f"--- the last {len(tail)} lines of {log}", file=sys.stderr)
        print("\n".join(tail), file=sys.stderr)


if __name__ == "__main__":
    try:
        asyncio.run(main())
    except StepError as error:
        print(f"error: {error}", file=sys.stderr)
        show_daemon_log()
        sys.exit(1)
