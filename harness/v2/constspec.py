"""constspec.py: the benchmark of `hir-ty-mir`'s `Specialize`, which calls a copy of a callee
compiled knowing a constant argument it branches on (docs/v2/plan/design/constspec.md).
The program `constspec/modes.rs` runs mode-switched hot loops; two blazes build it, the one
without the pass (`before`) and the one with it (`after`), and their programs are timed.

    uv run --frozen python harness/v2/constspec.py '{"before": {"kind": "v2", "blaze": "/tmp/before/blaze"},
        "after": {"kind": "v2", "blaze": "target/release/blaze"}, "duckdb": "/nix/store/...-duckdb/bin/duckdb",
        "out": "/tmp/constspec.json", "before_tree": "<digest>", "after_tree": "<digest>"}'
    uv run --frozen python harness/v2/constspec.py selftest

The input is one JSON object (inline, a file's path, or `-` for stdin):

    before, after  the two blazes (engine.EngineSpec)
    duckdb         the DuckDB `blaze debug sql` runs (`nix build --print-out-paths .#duckdb`/bin/duckdb)
    out            the path the result JSON is written to (report.py)
    before_tree    what each blaze was built from, kept in the result as given: the exact tree
    after_tree     (rule 22), as a content digest of its sources; the binaries' hashes are added
    rounds         rounds of each hot loop in a timed run (20000: 82M elements a loop)
    runs           timed runs of each program, the two alternating (15)
    work           the directory everything is built in; empty: a new one under /tmp
    keep           keep the work directory after the run

Each blaze builds the program, `src/main.rs` of a one-file package, with `--release` in a home
of its own; the two build at once. Then, one outcome each:

    meaning/<which>  run with 3 rounds, the program prints what `expected` here computes of
                     the same loops: the program's meaning, not either blaze's
    same-output      every timed run of both prints the same checksums
    copies           the after build's profile history (`blaze debug sql`) has a `mir
                     specialized copy` span for each copy of `run` main calls (`REQUIRED`),
                     and the before build's has none: otherwise the timing would not compare
                     the pass with its absence. Its numbers: the copies of `modes` and the time
                     in the pass's spans (`mir specialize`, `mir specialized copy`)
    time/<loop>      each loop's median and fastest nanoseconds under both, and the
                     before/after ratio of the medians: a measurement, which passes when the
                     runs did

The timed runs alternate (before, after, then after, before) so drift on the machine falls on
both alike. A loop's time is the program's own `Instant` around it, so neither process start
nor page faults of the binary count.
"""

import asyncio
import json
import os
import shutil
import statistics
import subprocess
import sys
import time
from collections.abc import Sequence
from dataclasses import dataclass
from pathlib import Path
from typing import cast

HERE = Path(__file__).resolve().parent
sys.path[:0] = [str(HERE), str(HERE.parent)]
import engine
import report
from typed_input import fail, parse

SCRIPT = "constspec.py"
SOURCE = HERE / "constspec" / "modes.rs"
PACKAGE = "modes"
EDITION = "2024"
BUILD_SECS = 3600
RUN_SECS = 600
STOP_SECS = 300
QUERY_SECS = 300
CHECK_ROUNDS = 3
WHICH = ("before", "after")
LOOPS = ("run_sum", "run_mix", "run_checked", "step_mix", "step_checked", "filter_odd", "filter_all",
         "scale_0", "scale_3")
# The copies of `run` main calls, by discriminant (`Mode::Sum`, `Mix`, `Checked`), as the span
# `mir specialized copy` names each after the base's path (`specialize.rs` `write_facts`).
REQUIRED = tuple(f"::run knowing #0 = {d}" for d in range(3))
SPANS: tuple[str, ...] = ("mir specialize", "mir specialized copy")
M = (1 << 64) - 1
GOLDEN = 0x9E37_79B9_7F4A_7C15
LEN = 4096


@dataclass(frozen=True)
class Input:
    before: engine.EngineSpec
    after: engine.EngineSpec
    duckdb: str
    out: str
    before_tree: str = ""
    after_tree: str = ""
    rounds: int = 20_000
    runs: int = 15
    work: str = ""
    keep: bool = False


# The program's loops, in Python's integers: what Rust says `modes.rs` computes.

def data() -> list[int]:
    state, out = 0x2545_F491_4F6C_DD1D, list[int]()
    for _ in range(LEN):
        state ^= (state << 13) & M
        state ^= state >> 7
        state ^= (state << 17) & M
        out.append(state)
    return out


def rotl(v: int, n: int) -> int:
    n %= 64
    return ((v << n) | (v >> (64 - n))) & M if n else v


def rotr(v: int, n: int) -> int:
    return rotl(v, (64 - n % 64) % 64)


def checked(acc: int, v: int) -> int:
    """`match acc.checked_add(v) { Some(sum) => sum, None => acc >> 1 }`."""
    return acc + v if acc + v <= M else acc >> 1


def step(mode: int, acc: int, x: int) -> int:
    if mode == 0:
        return (acc + x) & M
    if mode == 1:
        return ((rotl(acc, 5) ^ x) * GOLDEN) & M
    return checked(acc, x >> 1)


def run(mode: int, xs: list[int], rounds: int) -> int:
    acc = 0
    for r in range(rounds):
        for x in xs:
            acc = checked(acc, (x >> 1) ^ r) if mode == 2 else step(mode, acc, x)
    return acc


def stepped(mode: int, xs: list[int], rounds: int) -> int:
    acc = 0
    for _ in range(rounds):
        for x in xs:
            acc = step(mode, acc, x)
    return acc


def filtered(odd: bool, xs: list[int], rounds: int) -> int:
    acc = 0
    for r in range(rounds):
        for x in xs:
            if odd:
                if x & 1:
                    acc = (acc + (x >> 3)) & M
            else:
                acc ^= rotr(x, r)
    return acc


def scaled(k: int, xs: list[int], rounds: int) -> int:
    acc = 0
    for _ in range(rounds):
        for x in xs:
            acc = acc ^ x if k == 0 else (acc + x * k) & M
    return acc


def expected(rounds: int) -> dict[str, int]:
    xs = data()
    return {"run_sum": run(0, xs, rounds), "run_mix": run(1, xs, rounds), "run_checked": run(2, xs, rounds),
            "step_mix": stepped(1, xs, rounds), "step_checked": stepped(2, xs, rounds),
            "filter_odd": filtered(True, xs, rounds), "filter_all": filtered(False, xs, rounds),
            "scale_0": scaled(0, xs, rounds), "scale_3": scaled(3, xs, rounds)}


# What a run of the program printed.

@dataclass(frozen=True)
class Printed:
    sums: dict[str, int]
    ns: dict[str, int]


def printed(label: str, ran: engine.Ran) -> Printed:
    """The checksums and times `ran` printed, each loop once, all of `LOOPS`; anything else
    is a `Failed` naming the line."""
    if ran.returncode != 0:
        raise engine.Failed(f"{label}: `{' '.join(ran.argv)}` exited {ran.returncode}:\n{ran.said()}")
    sums: dict[str, int] = {}
    for line in ran.stdout.splitlines():
        name, _, value = line.partition(" ")
        if name not in LOOPS or name in sums or len(value) != 16:
            raise engine.Failed(f"{label}: stdout line {line!r} is not `LOOP HEX` of a loop not yet printed")
        sums[name] = int(value, 16)
    ns: dict[str, int] = {}
    for line in ran.stderr.splitlines():
        words = line.split()
        if len(words) != 3 or words[0] != "time" or words[1] not in LOOPS or words[1] in ns or not words[2].isdigit():
            raise engine.Failed(f"{label}: stderr line {line!r} is not `time LOOP NANOSECONDS`")
        ns[words[1]] = int(words[2])
    for what, got in (("checksum", sums), ("time", ns)):
        if missing := [it for it in LOOPS if it not in got]:
            raise engine.Failed(f"{label}: no {what} of {', '.join(missing)}:\n{ran.said()}")
    return Printed(sums, ns)


def differences(want: dict[str, int], got: dict[str, int]) -> list[str]:
    return [f"{name}: {got[name]:016x}, and Rust says {want[name]:016x}" for name in LOOPS if got[name] != want[name]]


# Commands.

async def command(step: str, argv: Sequence[str], cwd: Path, env: dict[str, str], secs: float) -> engine.Ran:
    """`argv` run to its end in `cwd`; one that does not start or outlives `secs` is a
    `Failed` naming the step and the command."""
    shown = " ".join(argv)
    began = time.perf_counter()
    try:
        proc = await asyncio.create_subprocess_exec(*argv, cwd=cwd, env=env, stdin=subprocess.DEVNULL,
                                                    stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    except OSError as e:
        raise engine.Failed(f"{step}: `{shown}` in {cwd} did not start: {e}") from None
    try:
        async with asyncio.timeout(secs):
            out, err = await proc.communicate()
    except TimeoutError:
        proc.kill()
        await proc.wait()
        raise engine.Failed(f"{step}: `{shown}` in {cwd} took more than {secs:.0f} s") from None
    took = time.perf_counter() - began
    return engine.Ran(tuple(argv), took, proc.returncode or 0, engine.text(out), engine.text(err))


@dataclass(frozen=True)
class Build:
    which: str
    eng: engine.Engine
    ws: Path
    env: dict[str, str]

    @property
    def program(self) -> Path:
        # In the build directory under the daemon's home: blaze writes no `target` into the checkout.
        return self.eng.build_dir(self.ws, self.env) / "release" / (PACKAGE + engine.EXE)


def manifest() -> str:
    return (f'[package]\nname = "{PACKAGE}"\nversion = "0.0.0"\nedition = "{EDITION}"\npublish = false\n\n'
            f'[[bin]]\nname = "{PACKAGE}"\npath = "src/main.rs"\n\n[workspace]\n')


def prepared(which: str, eng: engine.Engine, work: Path, duckdb: str) -> Build:
    root = work / which
    shutil.rmtree(root, ignore_errors=True)
    (root / "ws" / "src").mkdir(parents=True)
    (root / "ws" / "Cargo.toml").write_text(manifest())
    shutil.copyfile(SOURCE, root / "ws" / "src" / "main.rs")
    return Build(which, eng, root / "ws", eng.env(root / "home", extra={"BLAZE_DUCKDB": duckdb}))


async def built(build: Build) -> engine.Ran:
    step = f"{build.which}: blaze build --release"
    ran = await command(step, [str(build.eng.blaze), "build", "--release"], build.ws, build.env, BUILD_SECS)
    ran.checked(step)
    if not build.program.is_file():
        raise engine.Failed(f"{step} exited 0 and made no {build.program}:\n{ran.said()}")
    return ran


async def ran_program(build: Build, rounds: int) -> Printed:
    label = f"{build.which}'s program, {rounds} rounds"
    ran = await command(label, [str(build.program), str(rounds)], build.ws, engine.scrubbed(), RUN_SECS)
    return printed(label, ran)


async def stopped(build: Build) -> None:
    step = f"{build.which}: blaze stop"
    (await command(step, [str(build.eng.blaze), "stop"], build.ws, build.env, STOP_SECS)).checked(step)


@dataclass(frozen=True)
class Spans:
    """What a build's history says of the pass: each `mir specialized copy` of the program's
    crate by its `function` (the copy's name), and the time in each of `SPANS`."""

    copies: tuple[str, ...]
    ns: dict[str, int]


QUERY = (
    "SELECT name, \"crate\" AS krate, function, count(*) AS spans, sum(dur_ns) AS ns FROM spans "
    f"WHERE name IN ({', '.join(repr(it) for it in SPANS)}) GROUP BY ALL ORDER BY ALL"
)


def spans_of(label: str, stdout: str) -> Spans:
    """`QUERY`'s answer as `blaze debug sql --json` prints it: an object a row, a line each."""
    copies: list[str] = []
    ns: dict[str, int] = dict.fromkeys(SPANS, 0)
    for line in stdout.splitlines():
        try:
            row = json.loads(line)
        except json.JSONDecodeError as e:
            raise engine.Failed(f"{label}: `blaze debug sql --json` printed {line!r}, not a row: {e}") from None
        if not isinstance(row, dict):
            raise engine.Failed(f"{label}: `blaze debug sql --json` printed {line!r}, not an object")
        row = cast(dict[str, object], row)
        name, krate, function, spent = row.get("name"), row.get("krate"), row.get("function"), row.get("ns")
        if not isinstance(name, str) or name not in SPANS or not isinstance(spent, int):
            raise engine.Failed(f"{label}: row {line!r} has no span name of {SPANS} and nanoseconds")
        ns[name] += spent
        if name == "mir specialized copy" and krate == PACKAGE and isinstance(function, str):
            copies.append(function)
    return Spans(tuple(sorted(set(copies))), ns)


async def history(build: Build) -> Spans:
    step = f"{build.which}: blaze debug sql"
    ran = await command(step, [str(build.eng.blaze), "debug", "sql", "--json", QUERY], build.ws, build.env,
                        QUERY_SECS)
    return spans_of(step, ran.checked(step).stdout)


def copies_verdict(before: Spans, after: Spans) -> tuple[report.Verdict, str]:
    problems: list[str] = []
    if before.copies or before.ns["mir specialized copy"]:
        problems.append(f"the before build compiled copies ({', '.join(before.copies) or 'of other crates'}): "
                        "its blaze has the pass, so the timing compares nothing")
    if missing := [it for it in REQUIRED if not any(copy.endswith(it) for copy in after.copies)]:
        problems.append(f"the after build compiled no copy {', '.join(f'`{PACKAGE}{it}`' for it in missing)}; "
                        f"its copies of {PACKAGE}: {', '.join(after.copies) or 'none'}")
    return ("fail", "\n".join(problems)) if problems else ("pass", "")


# The run.

async def measured(args: Input, rep: report.Report, work: Path) -> None:
    try:
        builds = {which: prepared(which, engine.Engine.of(spec), work, args.duckdb)
                  for which, spec in zip(WHICH, (args.before, args.after))}
    except engine.Failed as e:
        rep.add("constspec: the engines", "error", str(e))
        return
    for build in builds.values():
        rep.extra[f"{build.which}_blaze"] = {"path": str(build.eng.blaze), "blake3": engine.digest(build.eng.blaze)}
    failed = False
    builds_ran: dict[str, asyncio.Task[engine.Ran]] = {}
    try:
        async with asyncio.TaskGroup() as group:
            for which, build in builds.items():
                builds_ran[which] = group.create_task(built(build))
    except* engine.Failed as e:
        failed = True
        for each in e.exceptions:
            rep.add("constspec: the builds", "error", str(each))
    if failed:
        await stop_all(builds, rep)
        return
    for which, task in builds_ran.items():
        rep.extra[f"{which}_build_s"] = round(task.result().seconds, 1)
        rep.extra[f"{which}_program_blake3"] = engine.digest(builds[which].program)
    try:
        await timed(args, rep, builds)
    finally:
        await stop_all(builds, rep)
    spans: dict[str, Spans] = {}
    for which, build in builds.items():
        try:
            spans[which] = await history(build)
        except engine.Failed as e:
            rep.add(f"constspec/copies: {which}'s history", "error", str(e))
    if len(spans) == len(WHICH):
        verdict, detail = copies_verdict(spans["before"], spans["after"])
        after = spans["after"]
        rep.extra["copies"] = list(after.copies)
        rep.add("constspec/copies", verdict, detail, copies=len(after.copies),
                specialize_ms=round(after.ns["mir specialize"] / 1e6, 2),
                copy_bodies_ms=round(after.ns["mir specialized copy"] / 1e6, 2))


async def timed(args: Input, rep: report.Report, builds: dict[str, Build]) -> None:
    want = expected(CHECK_ROUNDS)
    for build in builds.values():
        name = f"constspec/meaning/{build.which}"
        try:
            got = await ran_program(build, CHECK_ROUNDS)
        except engine.Failed as e:
            rep.add(name, "error", str(e))
            return
        wrong = differences(want, got.sums)
        rep.add(name, "fail" if wrong else "pass", "\n".join(wrong))
    times: dict[str, dict[str, list[int]]] = {which: {loop: [] for loop in LOOPS} for which in WHICH}
    first: tuple[str, dict[str, int]] | None = None
    for run_index in range(args.runs):
        for which in (WHICH if run_index % 2 == 0 else tuple(reversed(WHICH))):
            try:
                got = await ran_program(builds[which], args.rounds)
            except engine.Failed as e:
                rep.add(f"constspec/time: run {run_index} of {which}", "error", str(e))
                return
            first = first or (which, got.sums)
            if got.sums != first[1]:
                changed = [f"{loop}: {got.sums[loop]:016x}, not {first[1][loop]:016x}"
                           for loop in LOOPS if got.sums[loop] != first[1][loop]]
                rep.add("constspec/same-output", "fail", f"run {run_index} of {which} printed other checksums "
                        f"than the first run, of {first[0]}: {'; '.join(changed)}")
                return
            for loop in LOOPS:
                times[which][loop].append(got.ns[loop])
    rep.add("constspec/same-output", "pass", runs=args.runs, rounds=args.rounds)
    ratios: list[float] = []
    for loop in LOOPS:
        before, after = times["before"][loop], times["after"][loop]
        ratio = statistics.median(before) / max(statistics.median(after), 1)
        ratios.append(ratio)
        rep.add(f"constspec/time/{loop}", "pass", before_median_ms=round(statistics.median(before) / 1e6, 3),
                after_median_ms=round(statistics.median(after) / 1e6, 3), before_min_ms=round(min(before) / 1e6, 3),
                after_min_ms=round(min(after) / 1e6, 3), speedup=round(ratio, 3))
    rep.extra["speedup_geomean"] = round(statistics.geometric_mean(ratios), 3)
    rep.extra["times_ns"] = times


async def stop_all(builds: dict[str, Build], rep: report.Report) -> None:
    """Stops the daemon of each build that has a home, all at once; the history is flushed."""
    try:
        async with asyncio.TaskGroup() as group:
            for build in builds.values():
                if Path(build.env["BLAZE_HOME"]).is_dir():
                    group.create_task(stopped(build))
    except* engine.Failed as e:
        for each in e.exceptions:
            rep.add("constspec: stopping a daemon", "error", str(each))


def main(args: Input) -> None:
    if args.rounds < 1 or args.runs < 1:
        fail(SCRIPT, f"rounds and runs: at least 1, not {args.rounds} and {args.runs}")
    if not os.path.isabs(args.duckdb) or not os.path.isfile(args.duckdb):
        fail(SCRIPT, f"duckdb: an absolute path to the DuckDB binary, not {args.duckdb!r}")
    work = engine.work_dir(args.work, "bz-constspec-")
    work.mkdir(parents=True, exist_ok=True)
    rep = report.Report(SCRIPT, args.after, args.out)
    rep.extra.update(work=str(work), before=args.before.blaze, before_tree=args.before_tree,
                     after_tree=args.after_tree, source_blake3=engine.digest(SOURCE), rounds=args.rounds,
                     runs=args.runs, toolchain=engine.TOOLCHAIN)
    print(f"work: {work}", flush=True)
    try:
        asyncio.run(measured(args, rep, work))
    finally:
        if not args.keep:
            shutil.rmtree(work, ignore_errors=True)
    rep.finish()


# Self-tests: the script's own logic, no blaze.

def ran(returncode: int, stdout: str = "", stderr: str = "") -> engine.Ran:
    return engine.Ran(("modes",), 0.5, returncode, stdout, stderr)


def test_expected() -> None:
    zero = expected(0)
    assert set(zero) == set(LOOPS) and not any(zero.values()), zero
    xs = data()
    one = expected(1)
    acc = 0
    for x in xs:
        acc ^= x
    assert one["scale_0"] == acc and one["filter_all"] == acc, one
    assert one["run_sum"] == sum(xs) & M and one["run_mix"] == one["step_mix"], one
    assert all(0 <= v <= M for v in expected(CHECK_ROUNDS).values())
    assert rotr(rotl(0x8000_0000_0000_0001, 7), 7) == 0x8000_0000_0000_0001 and rotr(5, 64) == 5


def test_printed() -> None:
    out = "".join(f"{name} {i:016x}\n" for i, name in enumerate(LOOPS))
    err = "".join(f"time {name} {100 + i}\n" for i, name in enumerate(LOOPS))
    got = printed("t", ran(0, out, err))
    assert got.sums["run_mix"] == 1 and got.ns["scale_3"] == 108, got
    for bad in (ran(0, out + "run_sum 0000000000000000\n", err), ran(0, out, err.replace("time ", "tim ", 1)),
                ran(0, out.split("\n", 1)[1], err), ran(101, out, err)):
        try:
            printed("t", bad)
        except engine.Failed:
            continue
        raise AssertionError(f"taken: {bad}")


def test_spans() -> None:
    rows = [{"name": "mir specialize", "krate": "modes", "function": "modes::main", "spans": 1, "ns": 5},
            {"name": "mir specialized copy", "krate": "modes", "function": "modes::run knowing #0 = 1", "spans": 2,
             "ns": 7},
            {"name": "mir specialized copy", "krate": "core", "function": "core::x knowing #0 = 0", "spans": 1,
             "ns": 1}]
    got = spans_of("t", "\n".join(json.dumps(it) for it in rows))
    assert got.copies == ("modes::run knowing #0 = 1",) and got.ns == {"mir specialize": 5,
                                                                      "mir specialized copy": 8}, got
    none = Spans((), dict.fromkeys(SPANS, 0))
    verdict, detail = copies_verdict(none, got)
    assert verdict == "fail" and "`modes::run knowing #0 = 0`" in detail and "#0 = 1`" not in detail, detail
    full = Spans(tuple(f"modes{it}" for it in REQUIRED), dict.fromkeys(SPANS, 1))
    assert copies_verdict(none, full) == ("pass", "")
    verdict, detail = copies_verdict(full, full)
    assert verdict == "fail" and "before build compiled copies" in detail, detail


def test_input() -> None:
    args = parse(Input, '{"before": {"kind": "v2", "blaze": "a"}, "after": {"kind": "v2", "blaze": "b"}, '
                        '"duckdb": "/d", "out": ""}', SCRIPT)
    assert args.rounds == 20_000 and args.runs == 15 and args.after.blaze == "b"
    try:
        parse(Input, '{"before": {"kind": "v2", "blaze": "a"}, "after": {"kind": "v2", "blaze": "b"}, '
                     '"duckdb": "/d", "out": "", "round": 3}', SCRIPT)
    except SystemExit as e:
        assert "no field round" in str(e.code), e.code
    else:
        raise AssertionError("an unknown field was taken")


SELFTESTS = (test_expected, test_printed, test_spans, test_input)


def selftest() -> int:
    failed = 0
    for test in SELFTESTS:
        try:
            test()
            print(f"ok    {test.__name__}", flush=True)
        except (AssertionError, engine.Failed, SystemExit) as e:
            failed += 1
            print(f"FAIL  {test.__name__}: {type(e).__name__}: {e}", flush=True)
    print(f"{SCRIPT} selftest: {len(SELFTESTS) - failed} of {len(SELFTESTS)} pass", flush=True)
    return 1 if failed else 0


if __name__ == "__main__":
    if sys.argv[1:] == ["selftest"]:
        sys.exit(selftest())
    if len(sys.argv) != 2:
        sys.exit(f"usage: {SCRIPT} '<json>' | selftest\n\n{__doc__}")
    main(parse(Input, sys.argv[1], SCRIPT))
