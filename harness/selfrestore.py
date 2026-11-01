#!/usr/bin/env python3
"""Restore on the real self-build (docs/v2/plan/design/restore.md): a daemon killed with
SIGKILL restores from its image, and what it builds from there is what a build from nothing
builds.

1. `blaze build ARGS` in `worktree` with `BLAZE_HOME=home`, so the home has a daemon and an
   image.
2. The home's daemon (the last `listening ... (pid N)` of its `v2/serve.log`) is killed with
   SIGKILL, and waited for.
3. `blaze build ARGS` again, nothing changed. It passes when the daemon it starts logs
   `restored the daemon image` and no `starts empty`, no unit is compiled again (no
   `Compiling` line), and it answers within `budget_s`.
4. With `fresh_home` set: `ARGS` built again from nothing, in an emptied `fresh_home` and
   `fresh_target`, and each of `outputs` (paths under the target directories) compared byte
   for byte with the restored build's in `target`.

A build may fail where blaze cannot build a unit yet: each build's failed units (its
`could not compile `NAME` (KIND)` lines) are the same set in all three builds, and that set is
`expect_failed` when it is given; a build that exits nonzero with no such line fails the run.

Input, one JSON value (inline, a file or `-`):

    {"blaze": "/path/blaze", "worktree": "/path/src", "home": "/path/home",
     "args": ["-p", "blaze-engine", "--bin", "blaze"], "target": "/path/src/target",
     "budget_s": 10.0, "fresh_home": null, "fresh_target": null, "outputs": ["debug/blaze"],
     "expect_failed": ["zstd-sys (build script)"]}
"""

import hashlib
import os
import re
import shutil
import signal
import subprocess
import sys
import time
from dataclasses import dataclass

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.join(HERE, "..", "python", "typed-input", "src"))
from typed_input import fail, parse

SCRIPT = "selfrestore.py"
LISTENING = re.compile(r"^blaze serve: listening on .* \(pid (\d+)\)$", re.M)
COMPILING = re.compile(r"^\s*Compiling ", re.M)
FAILED = re.compile(r"^error: could not compile `([^`]+)` \(([^)]+)\)", re.M)


@dataclass(frozen=True)
class Input:
    blaze: str
    worktree: str
    home: str
    args: tuple[str, ...]
    target: str
    budget_s: float = 10.0
    fresh_home: str | None = None
    fresh_target: str | None = None
    outputs: tuple[str, ...] = ()
    expect_failed: tuple[str, ...] | None = None


def build(it: Input, home: str, target: str, what: str) -> tuple[float, str, frozenset[str]]:
    """`blaze build ARGS` in the worktree with `home` and `target`: its wall time, output and
    failed units (`NAME (KIND)`)."""
    env = {**os.environ, "BLAZE_HOME": home, "CARGO_TARGET_DIR": target}
    began = time.monotonic()
    try:
        ran = subprocess.run([it.blaze, "build", *it.args], cwd=it.worktree, env=env, capture_output=True, text=True)
    except OSError as error:
        fail(SCRIPT, f"{what}: running {it.blaze} in {it.worktree}: {error}")
    took = time.monotonic() - began
    said = ran.stdout + ran.stderr
    failed = frozenset(f"{name} ({kind})" for name, kind in FAILED.findall(said))
    if ran.returncode != 0 and not failed:
        fail(SCRIPT, f"{what}: `blaze build` exited {ran.returncode} after {took:.1f} s naming no unit:\n{said[-4000:]}")
    return took, said, failed


def serve_log(home: str) -> str:
    path = os.path.join(home, "v2", "serve.log")
    with open(path, errors="replace") as f:
        return f.read()


def kill_daemon(home: str) -> int:
    """SIGKILL to the home's daemon, waited for: its pid."""
    started = LISTENING.findall(serve_log(home))
    if not started:
        fail(SCRIPT, f"{home}: its serve.log names no daemon listening")
    pid = int(started[-1])
    try:
        os.kill(pid, signal.SIGKILL)
    except ProcessLookupError:
        fail(SCRIPT, f"{home}: its daemon (pid {pid}) is not running after the first build")
    for _ in range(600):
        try:
            os.kill(pid, 0)
        except ProcessLookupError:
            return pid
        time.sleep(0.1)
    fail(SCRIPT, f"{home}: its daemon (pid {pid}) still runs 60 s after SIGKILL")


def digest(path: str) -> str:
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for block in iter(lambda: f.read(1 << 20), b""):
            h.update(block)
    return h.hexdigest()


def main():
    match sys.argv[1:]:
        case [text]:
            it = parse(Input, text, SCRIPT)
        case _:
            fail(SCRIPT, f"usage: {SCRIPT} JSON (see the top of the file)")
    if (it.fresh_home is None) != (it.fresh_target is None):
        fail(SCRIPT, "fresh_home and fresh_target are set together")
    if it.fresh_home is not None and not it.outputs:
        fail(SCRIPT, "a build from nothing is compared on `outputs`, and none is named")

    first, _, first_failed = build(it, it.home, it.target, "the first build")
    print(f"first build: {first:.1f} s, {len(first_failed)} units failed")
    failed = []
    if it.expect_failed is not None and first_failed != frozenset(it.expect_failed):
        failed.append(
            f"the first build's failed units are not the expected ones: "
            f"also {sorted(first_failed - set(it.expect_failed))}, not {sorted(set(it.expect_failed) - first_failed)}"
        )
    killed = kill_daemon(it.home)
    before = len(serve_log(it.home))
    took, said, restored_failed = build(it, it.home, it.target, "the build after SIGKILL")
    after = serve_log(it.home)[before:]
    restored = [line for line in after.splitlines() if "restored the daemon image" in line]
    empty = [line for line in after.splitlines() if "starts empty" in line]
    compiled = COMPILING.findall(said)
    print(f"after SIGKILL of pid {killed}: {took:.1f} s, {len(compiled)} units compiled again")
    print(*(restored or ["no `restored the daemon image` line"]), sep="\n")
    if restored_failed != first_failed:
        failed.append(
            f"the build after SIGKILL failed other units than the first: "
            f"also {sorted(restored_failed - first_failed)}, not {sorted(first_failed - restored_failed)}"
        )
    if not restored or empty:
        failed.append(f"the daemon did not restore its image: {empty or 'no restored line'}")
    if compiled:
        failed.append(f"{len(compiled)} units compiled again, none expected:\n{''.join(compiled)}")
    if took > it.budget_s:
        failed.append(f"the build after SIGKILL took {took:.1f} s, over the budget of {it.budget_s} s")

    if it.fresh_home is not None:
        shutil.rmtree(it.fresh_home, ignore_errors=True)
        shutil.rmtree(it.fresh_target, ignore_errors=True)
        os.makedirs(it.fresh_home)
        fresh, _, fresh_failed = build(it, it.fresh_home, it.fresh_target, "the build from nothing")
        print(f"build from nothing: {fresh:.1f} s, {len(fresh_failed)} units failed")
        if fresh_failed != restored_failed:
            failed.append(
                f"the build from nothing failed other units than the restored one: "
                f"also {sorted(fresh_failed - restored_failed)}, not {sorted(restored_failed - fresh_failed)}"
            )
        for output in it.outputs:
            restored_at, fresh_at = os.path.join(it.target, output), os.path.join(it.fresh_target, output)
            mine, theirs = digest(restored_at), digest(fresh_at)
            print(f"{output}: restored {mine[:16]}, from nothing {theirs[:16]}")
            if mine != theirs:
                failed.append(f"{output} differs: restored {restored_at} {mine}, from nothing {fresh_at} {theirs}")

    if failed:
        fail(SCRIPT, "\n".join(failed))
    print("ok")


if __name__ == "__main__":
    main()
