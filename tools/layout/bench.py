"""Profile-guided code layout's benchmark (docs/v2/plan/design/code-layout.md): one tree of
blaze built three ways, each binary timed on one fixed workload.

  uv run --frozen python tools/layout/bench.py tools/layout/bench.json

One typed input (AGENTS.md rule scripts-take-typed-input), a `Bench`:

1. `before`: `tree` built by `old_builder`, a blaze of this tree with the layout change
   reverted (so `before` against `cold` is that change alone, not every other lane's):
   functions by content.
2. `cold`: `tree` built by `new_builder` (a blaze of this change) with an empty
   `<new_target>/layout`: the functions codegen inferred cold last, no profile. The link writes
   the image's address map there.
3. Training: the `cold` binary runs the workload once with `IX_PROF=<new_target>/layout` and its
   daemon is stopped, which writes the daemon's profile there.
4. `profiled`: `tree` built again by `new_builder`: the profile is in the unit's record key, so the
   image is linked again, laid out by it. A binary equal to `cold`'s but for its build id
   (which names the profile) is a profile that moved nothing, and stops the bench.
5. `rounds` rounds: each binary in turn, rotated a place a round, runs the workload's turns in
   `workload` with a BLAZE_HOME and target directory of its own made fresh, then its daemon is
   stopped. The time is
   the turns' wall time. Each binary's first run is a warm-up, not counted: it builds the
   standard library every daemon of that binary shares on the machine (`cold`'s before its
   training, so the profile is of the workload the rounds time).

Writes `out`: the tree's digest before and after (measure-the-exact-tree: the same, or the bench
fails), each builder's and binary's sha256, every round's times, and each binary's median and
least, with `cold`'s and `profiled`'s speedups over `before`.

Every step is bounded and a failure names the step, the command, its exit code and the tail of
its output. Nothing falls back: a build that fails, a profile that is not written, a binary the
profile did not change all stop the bench.
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

SCRIPT = "tools/layout/bench.py"
# The variables a run's environment loses: a target directory or profiler of the caller's.
DROPPED = ("CARGO_TARGET_DIR", "BLAZE_HOME", "IX_PROF", "IX_PROF_HTTP", "IX_PROF_HZ", "IX_PROF_ALLOC")
TAIL = 4000


@dataclass(frozen=True)
class Turn:
    """One command of the workload: `blaze <args>` in the workload's directory."""

    args: tuple[str, ...]


@dataclass(frozen=True)
class Bench:
    old_builder: str
    new_builder: str
    tree: str
    old_target: str
    new_target: str
    workload: str
    turns: tuple[Turn, ...]
    scratch: str
    out: str
    rounds: int = 10
    build: tuple[str, ...] = ("build", "--release", "-p", "blaze-engine", "--bin", "blaze")
    binary: str = "release/blaze"
    build_secs: float = 7200.0
    turn_secs: float = 1800.0
    stop_secs: float = 300.0


class StepError(Exception):
    pass


def env_of(home: Path, **more: str) -> dict[str, str]:
    env = {k: v for k, v in os.environ.items() if k not in DROPPED}
    env["BLAZE_HOME"] = str(home)
    env.update(more)
    return env


async def run(step: str, argv: list[str], cwd: Path, env: dict[str, str], secs: float, log: Path) -> bytes:
    """Runs `argv`, its output to `log`; its stdout. A failure names the step and the command."""
    log.parent.mkdir(parents=True, exist_ok=True)
    with log.open("wb") as out:
        proc = await asyncio.create_subprocess_exec(
            *argv, cwd=cwd, env=env, stdout=asyncio.subprocess.PIPE, stderr=out
        )
        try:
            async with asyncio.timeout(secs):
                stdout, _ = await proc.communicate()
        except BaseException as error:
            # Timed out, or cancelled by a sibling step's failure: the child goes too, so it
            # loads no later measurement.
            if proc.returncode is None:
                proc.kill()
                await proc.wait()
            if isinstance(error, TimeoutError):
                raise StepError(f"{step}: `{' '.join(argv)}` ran past {secs:.0f} s (log {log})") from None
            raise
        out.write(stdout)
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


async def build(step: str, bench: Bench, builder: Path, target: Path, home: Path, logs: Path) -> Path:
    """`tree` built by `builder` into `target`: the binary."""
    env = env_of(home, CARGO_TARGET_DIR=str(target))
    tree = Path(bench.tree)
    try:
        await run(step, [str(builder), *bench.build], tree, env, bench.build_secs, logs / f"{step}.log")
    finally:
        # The daemon goes, whether the build passed, failed or was cancelled: so the next
        # build of this builder's starts its own, and nothing of it runs while measuring.
        await run(
            f"{step} stop", [str(builder), "stop"], tree, env, bench.stop_secs, logs / f"{step}-stop.log"
        )
    binary = target / bench.binary
    if not binary.is_file():
        raise StepError(f"{step}: the build made no {binary}")
    return binary


async def workload(name: str, bench: Bench, binary: Path, home: Path, logs: Path, **more: str) -> float:
    """The workload's turns with `binary` and a fresh BLAZE_HOME `home`, then its daemon
    stopped: the turns' wall time in seconds."""
    shutil.rmtree(home, ignore_errors=True)
    home.mkdir(parents=True)
    # A target directory of its own too, inside `home`: the workload's own `target` would hold
    # the last run's build, and the next run do nothing.
    env = env_of(home, CARGO_TARGET_DIR=str(home / "target"), **more)
    cwd = Path(bench.workload)
    took = 0.0
    try:
        for at, turn in enumerate(bench.turns):
            start = time.perf_counter_ns()
            await run(
                f"{name} turn {at}", [str(binary), *turn.args], cwd, env, bench.turn_secs, logs / f"{at}.log"
            )
            took += (time.perf_counter_ns() - start) / 1e9
    finally:
        await run(f"{name} stop", [str(binary), "stop"], cwd, env, bench.stop_secs, logs / "stop.log")
    shutil.rmtree(home, ignore_errors=True)
    return took


async def train(bench: Bench, binary: Path, layout: Path, scratch: Path) -> int:
    """The workload once with `binary` under ix-prof into `layout`: the profiles written."""
    before = set(layout.glob("*.ixprof"))
    await workload(
        "train", bench, binary, scratch / "home-train", scratch / "logs" / "train", IX_PROF=str(layout)
    )
    # Each client run writes its profile as it ends, and the daemon as `stop` ends it: one a
    # turn, the stop's, and the daemon's.
    want = len(bench.turns) + 2
    try:
        async with asyncio.timeout(bench.stop_secs):
            while len(set(layout.glob("*.ixprof")) - before) < want:
                await asyncio.sleep(0.2)
    except TimeoutError:
        written = sorted(str(it) for it in set(layout.glob("*.ixprof")) - before)
        raise StepError(
            f"train: {want} profiles were not written to {layout} in {bench.stop_secs:.0f} s: {written}"
        ) from None
    return len(set(layout.glob("*.ixprof")) - before)


def build_id(binary: bytes, layout: Path, name: str) -> bytes:
    """The build id of `binary`: of the maps in `layout`, the one id it holds."""
    ids = {bytes.fromhex(it.name) for it in layout.glob("maps/*/*") if len(it.name) == 32}
    found = sorted(it for it in ids if it in binary)
    if len(found) != 1:
        raise StepError(
            f"{name}: {len(found)} of the {len(ids)} build ids mapped in {layout} are in the binary, not 1"
        )
    return found[0]


async def moved(cold: Path, profiled: Path, layout: Path) -> bool:
    """Whether the profile moved code: the two binaries differ with their build ids masked
    (the image's lineage names its profile, so any profile changes the id)."""

    def masked(path: Path, name: str) -> bytes:
        binary = path.read_bytes()
        return binary.replace(build_id(binary, layout, name), bytes(16))

    a, b = await asyncio.gather(
        asyncio.to_thread(masked, cold, "cold"), asyncio.to_thread(masked, profiled, "profiled")
    )
    return a != b


async def main(bench: Bench) -> None:
    scratch = Path(bench.scratch)
    logs = scratch / "logs"
    bins = scratch / "bin"
    bins.mkdir(parents=True, exist_ok=True)
    old_builder, new_builder = Path(bench.old_builder), Path(bench.new_builder)
    layout = Path(bench.new_target) / "layout"
    async with asyncio.TaskGroup() as group:
        tree_before = group.create_task(tree_digest(Path(bench.tree), logs / "tree-before"))
        old_hash = group.create_task(sha256(old_builder))
        new_hash = group.create_task(sha256(new_builder))
    # An empty `layout`: the cold build writes its map there and reads no profile. It is
    # `IX_PROF` too, which refuses a directory anyone but its owner can enter (0700).
    layout.mkdir(parents=True, exist_ok=True)
    layout.chmod(0o700)
    for stale in layout.glob("*.ixprof"):
        stale.unlink()
    homes = scratch / "builders"
    async with asyncio.TaskGroup() as group:
        before = group.create_task(
            build("before", bench, old_builder, Path(bench.old_target), homes / "old", logs / "build")
        )
        cold = group.create_task(
            build("cold", bench, new_builder, Path(bench.new_target), homes / "new", logs / "build")
        )
    binaries: dict[str, Path] = {}
    for name, made in (("before", before.result()), ("cold", cold.result())):
        binaries[name] = bins / name
        shutil.copy2(made, binaries[name])
    # `cold`'s first run builds the standard library its daemons share; training after it
    # profiles the workload the rounds time, not that.
    await workload("warm-up cold", bench, binaries["cold"], scratch / "home", logs / "warm-up" / "cold")
    profiles = await train(bench, binaries["cold"], layout, scratch)
    made = await build("profiled", bench, new_builder, Path(bench.new_target), homes / "new", logs / "build")
    binaries["profiled"] = bins / "profiled"
    shutil.copy2(made, binaries["profiled"])
    async with asyncio.TaskGroup() as group:
        hashing = {name: group.create_task(sha256(path)) for name, path in binaries.items()}
    hashes = {name: task.result() for name, task in hashing.items()}
    if not await moved(binaries["cold"], binaries["profiled"], layout):
        raise StepError(
            f"profiled: the binary is `cold`'s but for its build id: the {profiles} profiles in {layout} moved nothing"
        )

    names = list(binaries)
    times: dict[str, list[float]] = {name: [] for name in names}
    for name in ("before", "profiled"):
        await workload(f"warm-up {name}", bench, binaries[name], scratch / "home", logs / "warm-up" / name)
    for at in range(bench.rounds):
        for name in names[at % len(names) :] + names[: at % len(names)]:
            took = await workload(
                f"round {at} {name}", bench, binaries[name], scratch / "home", logs / f"round-{at}" / name
            )
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
        "profiles": profiles,
        "workload": {"dir": bench.workload, "turns": [list(it.args) for it in bench.turns]},
        "times": times,
        "median": median,
        "least": {name: min(it) for name, it in times.items()},
        "speedup": {name: median["before"] / median[name] for name in ("cold", "profiled")},
    }
    Path(bench.out).write_text(json.dumps(result, indent=2) + "\n")
    print(json.dumps({"median": median, "speedup": result["speedup"]}, indent=2))


if __name__ == "__main__":
    if len(sys.argv) != 2:
        typed_input.fail(SCRIPT, "one argument: the input, inline JSON, a file or `-`")
    bench = typed_input.parse(Bench, sys.argv[1], SCRIPT)
    if bench.rounds < 1 or not bench.turns:
        typed_input.fail(SCRIPT, "rounds is at least 1 and turns has one at least")
    paths = ("old_builder", "new_builder", "tree", "old_target", "new_target", "workload", "scratch", "out")
    relative = [name for name in paths if not Path(getattr(bench, name)).is_absolute()]
    if relative:
        # Each command runs in another directory: a relative path would name another place.
        typed_input.fail(SCRIPT, f"these are paths from /: {', '.join(relative)}")
    try:
        asyncio.run(main(bench))
    except* StepError as group:
        typed_input.fail(SCRIPT, "\n".join(str(it) for it in group.exceptions))
