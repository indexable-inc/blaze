"""Devirtualization's benchmark (docs/v2/plan/design/devirt.md): the dyn-heavy loop of
`tools/devirt/dyn_loop` built `--release` by a blaze of the tree before devirtualization and by a
blaze of this change, each binary timed on the same iterations.

  uv run --frozen python tools/devirt/bench.py tools/devirt/bench.json

One typed input (AGENTS.md rule scripts-take-typed-input), a `Bench`:

1. `before`: `program` built by `old_builder` (the merge base), every call through a `dyn` left
   indirect. `after`: built by `new_builder`, each builder with a BLAZE_HOME and target
   directory of its own, the two at once.
2. Each binary runs once with `check_iterations`: their outputs (a checksum a loop) must be the
   same, and the binaries not (a binary equal to `before`'s is a pass that did nothing, and
   stops the bench).
3. `rounds` rounds: each binary in turn, rotated a place a round, runs `iterations`; the time is
   its wall time. The first run of each is a warm-up, not counted.

Writes `out`: the blaze tree's digest before and after (measure-the-exact-tree: the same, or
the bench fails), each builder's and binary's sha256, every round's times, each binary's median
and least, and `after`'s speedup over `before`. Every step is bounded and a failure names the
step, the command, its exit code and the tail of its output.
"""

import asyncio
import hashlib
import json
import os
import shutil
import statistics
import sys
import time
from dataclasses import dataclass
from pathlib import Path

import typed_input

SCRIPT = "tools/devirt/bench.py"
DROPPED = ("CARGO_TARGET_DIR", "BLAZE_HOME", "IX_PROF", "IX_PROF_HTTP", "IX_PROF_HZ", "IX_PROF_ALLOC")
TAIL = 4000


@dataclass(frozen=True)
class Bench:
    old_builder: str
    new_builder: str
    tree: str
    program: str
    scratch: str
    out: str
    rounds: int = 10
    iterations: int = 300_000_000
    check_iterations: int = 1_000_003
    build: tuple[str, ...] = ("build", "--release")
    binary: str = "release/dyn-loop"
    build_secs: float = 3600.0
    run_secs: float = 600.0
    stop_secs: float = 300.0


class StepError(Exception):
    pass


async def run(step: str, argv: list[str], cwd: Path, env: dict[str, str], secs: float, log: Path) -> bytes:
    """Runs `argv`, its stderr to `log`; its stdout. A failure names the step and the command."""
    log.parent.mkdir(parents=True, exist_ok=True)
    with log.open("wb") as err:
        try:
            async with asyncio.timeout(secs):
                proc = await asyncio.create_subprocess_exec(
                    *argv, cwd=cwd, env=env, stdout=asyncio.subprocess.PIPE, stderr=err
                )
                stdout, _ = await proc.communicate()
        except TimeoutError:
            raise StepError(f"{step}: `{' '.join(argv)}` ran past {secs:.0f} s (log {log})") from None
        err.write(stdout)
    if proc.returncode != 0:
        tail = log.read_bytes()[-TAIL:].decode(errors="replace")
        raise StepError(f"{step}: `{' '.join(argv)}` exited {proc.returncode} in {cwd} (log {log}):\n{tail}")
    return stdout


async def sha256(path: Path) -> str:
    return await asyncio.to_thread(lambda: hashlib.sha256(path.read_bytes()).hexdigest())


async def tree_digest(tree: Path, logs: Path) -> str:
    """The tree's sources: its commit, the diff of its tracked files and its untracked files."""
    env = dict(os.environ)
    git = ["git", "-C", str(tree)]
    async with asyncio.TaskGroup() as group:
        head = group.create_task(
            run("tree digest", [*git, "rev-parse", "HEAD"], tree, env, 60, logs / "head.log")
        )
        diff = group.create_task(
            run("tree digest", [*git, "diff", "--binary", "HEAD"], tree, env, 300, logs / "diff.log")
        )
        untracked = group.create_task(
            run(
                "tree digest",
                [*git, "ls-files", "-z", "--others", "--exclude-standard"],
                tree,
                env,
                300,
                logs / "untracked.log",
            )
        )
    digest = hashlib.sha256(head.result() + b"\0" + diff.result())
    for name in sorted(it for it in untracked.result().split(b"\0") if it):
        digest.update(name + b"\0")
        digest.update(await asyncio.to_thread((tree / name.decode()).read_bytes))
    return digest.hexdigest()


async def build(step: str, bench: Bench, builder: Path, home: Path, logs: Path) -> Path:
    """`program` built by `builder` with a fresh BLAZE_HOME `home`: the binary, copied out."""
    shutil.rmtree(home, ignore_errors=True)
    home.mkdir(parents=True)
    target = home / "target"
    env = {k: v for k, v in os.environ.items() if k not in DROPPED}
    env |= {"BLAZE_HOME": str(home), "CARGO_TARGET_DIR": str(target)}
    program = Path(bench.program)
    try:
        await run(step, [str(builder), *bench.build], program, env, bench.build_secs, logs / f"{step}.log")
    finally:
        await run(
            f"{step} stop", [str(builder), "stop"], program, env, bench.stop_secs, logs / f"{step}-stop.log"
        )
    binary = target / bench.binary
    if not binary.is_file():
        raise StepError(f"{step}: the build made no {binary}")
    kept = Path(bench.scratch) / "bin" / step
    kept.parent.mkdir(parents=True, exist_ok=True)
    shutil.copy2(binary, kept)
    return kept


async def timed(step: str, binary: Path, iterations: int, bench: Bench, log: Path) -> tuple[float, bytes]:
    start = time.perf_counter_ns()
    out = await run(
        step, [str(binary), str(iterations)], Path(bench.scratch), dict(os.environ), bench.run_secs, log
    )
    return (time.perf_counter_ns() - start) / 1e9, out


async def main(bench: Bench) -> None:
    scratch = Path(bench.scratch)
    logs = scratch / "logs"
    old_builder, new_builder = Path(bench.old_builder), Path(bench.new_builder)
    async with asyncio.TaskGroup() as group:
        tree_before = group.create_task(tree_digest(Path(bench.tree), logs / "tree-before"))
        old_hash = group.create_task(sha256(old_builder))
        new_hash = group.create_task(sha256(new_builder))
        before = group.create_task(
            build("before", bench, old_builder, scratch / "home-before", logs / "build")
        )
        after = group.create_task(build("after", bench, new_builder, scratch / "home-after", logs / "build"))
    binaries = {"before": before.result(), "after": after.result()}
    async with asyncio.TaskGroup() as group:
        hashing = {name: group.create_task(sha256(path)) for name, path in binaries.items()}
    hashes = {name: task.result() for name, task in hashing.items()}
    if hashes["after"] == hashes["before"]:
        raise StepError("after: the binary is `before`'s byte for byte: no call was devirtualized")
    outputs: dict[str, bytes] = {}
    for name, binary in binaries.items():
        _, outputs[name] = await timed(
            f"check {name}", binary, bench.check_iterations, bench, logs / f"check-{name}.log"
        )
    if outputs["after"] != outputs["before"]:
        raise StepError(f"the outputs differ: before {outputs['before']!r}, after {outputs['after']!r}")

    names = list(binaries)
    times: dict[str, list[float]] = {name: [] for name in names}
    for name in names:
        await timed(f"warm-up {name}", binaries[name], bench.iterations, bench, logs / f"warm-up-{name}.log")
    for at in range(bench.rounds):
        for name in names[at % len(names) :] + names[: at % len(names)]:
            took, out = await timed(
                f"round {at} {name}", binaries[name], bench.iterations, bench, logs / f"round-{at}-{name}.log"
            )
            if out != outputs.get(f"{name} full", out):
                raise StepError(f"round {at} {name}: the output changed between runs: {out!r}")
            outputs[f"{name} full"] = out
            times[name].append(took)
            print(f"{SCRIPT}: round {at} {name}: {took:.3f} s", file=sys.stderr)

    tree_after = await tree_digest(Path(bench.tree), logs / "tree-after")
    if tree_after != tree_before.result():
        raise StepError(f"the tree changed while it was measured: {tree_before.result()} then {tree_after}")
    median = {name: statistics.median(it) for name, it in times.items()}
    result = {
        "tree": tree_after,
        "builders": {"old": old_hash.result(), "new": new_hash.result()},
        "binaries": hashes,
        "output": outputs["before"].decode(errors="replace"),
        "iterations": bench.iterations,
        "times": times,
        "median": median,
        "least": {name: min(it) for name, it in times.items()},
        "speedup": median["before"] / median["after"],
    }
    Path(bench.out).write_text(json.dumps(result, indent=2) + "\n")
    print(json.dumps({"median": median, "speedup": result["speedup"]}, indent=2))


if __name__ == "__main__":
    if len(sys.argv) != 2:
        typed_input.fail(SCRIPT, "one argument: the input, inline JSON, a file or `-`")
    bench = typed_input.parse(Bench, sys.argv[1], SCRIPT)
    if bench.rounds < 1 or bench.iterations < 1 or bench.check_iterations < 1:
        typed_input.fail(SCRIPT, "rounds, iterations and check_iterations are at least 1")
    paths = ("old_builder", "new_builder", "tree", "program", "scratch", "out")
    relative = [name for name in paths if not Path(getattr(bench, name)).is_absolute()]
    if relative:
        # Each command runs in another directory: a relative path would name another place.
        typed_input.fail(SCRIPT, f"these are paths from /: {', '.join(relative)}")
    try:
        asyncio.run(main(bench))
    except* StepError as group:
        typed_input.fail(SCRIPT, "\n".join(str(it) for it in group.exceptions))
