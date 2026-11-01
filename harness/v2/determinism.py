"""determinism.py: every corpus build twice, on different core counts, byte for byte the same
(docs/v2/plan/goals/principles.md, "Deterministic", and docs/v2/plan/lanes/proof.md). Linux only: a build is
pinned to its cores with the scheduler's affinity, as `taskset -c` pins one.

    uv run --frozen python harness/v2/determinism.py '{"engine": {"kind": "v1", "blaze": "target/release/blaze"}, "out": "/tmp/determinism.json"}'
    uv run --frozen python harness/v2/determinism.py '{"engine": {...}, "out": "...", "projects": ["ripgrep", "just", "fd"], "cores_a": 4}'
    uv run --frozen python harness/v2/determinism.py selftest

The input is one JSON object (inline, a file's path, or `-`):

    engine        the blaze measured (engine.EngineSpec)
    out           the result's path (report.py's JSON)
    work          where the checkout, its home, store and target directories go; a new
                  directory under /tmp when empty
    projects      the projects of bench.PROJECTS built, at their pinned revisions (ripgrep, just;
                  name more to build more)
    fixture       also the oracle's fixture (harness/v2/fixture.py), built with the environment
                  its state asks for (true)
    fixture_seed  its state: "base" for the default one, any other seed for a random one ("base")
    profiles      dev, release or both (both)
    cores_a       the cores of the first build (2)
    cores_b       the cores of the second (0: every core this script may run on); not cores_a
    keep          keep the work directory (false)

For each project and profile: two cold builds of the same source at the same path, each with an
empty home, store and target directory and its own daemon, the first pinned to the first
`cores_a` of the cores this script may run on and the second to the first `cores_b`; then
`engine.outputs` of both (every program and library a build delivers, by its BLAKE3 digest). One
outcome per (project, profile): pass when both builds delivered the same files with the same
bytes; fail naming each file that differs and where (`engine.first_difference_by`, the
`blaze-v2-proof` beside the blaze measured), or that is in
one build only, or when a build failed or was not blaze's. `selftest` checks the comparison, the
choice of cores and the fixture's workspace and environment without blaze, cargo or the network.
"""

import os
import shutil
import sys
import tempfile
import time
from collections.abc import Callable
from dataclasses import dataclass, field
from pathlib import Path
from typing import Literal, get_args

HERE = Path(__file__).resolve().parent
sys.path[:0] = [str(HERE), str(HERE.parent)]
from typed_input import fail, parse
import engine
import report
import budgets
import fixture

SCRIPT = "determinism.py"
Profile = Literal["dev", "release"]
SHOWN = 10


@dataclass(frozen=True)
class Input:
    engine: engine.EngineSpec
    out: str
    work: str = ""
    projects: tuple[str, ...] = ("ripgrep", "just")
    fixture: bool = True
    fixture_seed: str = "base"
    profiles: tuple[Profile, ...] = get_args(Profile)
    cores_a: int = 2
    cores_b: int = 0
    keep: bool = False


@dataclass(frozen=True)
class Subject:
    """What is built: a tree copied afresh for each build, its workspace (`sub`, a directory of
    it or ""), the arguments of `blaze build`, the environment it needs and a directory put in
    front of PATH (or none)."""

    label: str
    source: Path
    sub: str = ""
    build: tuple[str, ...] = ()
    env: dict[str, str] = field(default_factory=dict[str, str])
    path_front: Path | None = None


def cpu_set(available: list[int], n: int) -> set[int]:
    """The first `n` of the cores this script may run on."""
    if not 1 <= n <= len(available):
        raise ValueError(f"{n} cores asked for, of the {len(available)} this script may run on")
    return set(sorted(available)[:n])


def profile_dir(profile: Profile) -> str:
    return "release" if profile == "release" else "debug"


def differences(a: dict[str, str], b: dict[str, str]) -> list[tuple[str, str]]:
    """Each file of two builds' outputs that is not the same in both: its path and how."""
    found: list[tuple[str, str]] = []
    for name in sorted(a.keys() | b.keys()):
        if name not in b:
            found.append((name, "only in the first build"))
        elif name not in a:
            found.append((name, "only in the second build"))
        elif a[name] != b[name]:
            found.append((name, "differs"))
    return found


class Failure(Exception):
    """A finding: a build that failed or that blaze did not serve."""


def pinned_build(eng: engine.Engine, subject: Subject, top: Path, profile: Profile, cores: set[int]) -> tuple[Path, float]:
    """A cold build of `subject` at `top/src`, with an empty home, store and target directory and a
    daemon of its own, every process of it on `cores`; the target directory and the seconds."""
    tree, home, store = top / "src", top / "home", top / "store"
    for path in (tree, home, store):
        shutil.rmtree(path, ignore_errors=True)
    shutil.copytree(subject.source, tree, symlinks=True, ignore=budgets.left_out)
    ws = tree / subject.sub if subject.sub else tree
    env = eng.env(home, store, None, subject.env)
    if subject.path_front is not None:
        env["PATH"] = f"{subject.path_front}{os.pathsep}{env['PATH']}"
    args = [*subject.build, *(["--release"] if profile == "release" else [])]
    step = f"{subject.label} {profile} on {len(cores)} cores"
    if sys.platform != "linux":
        raise engine.Failed(f"{step}: sched_setaffinity is Linux's, not {sys.platform}'s")
    before = os.sched_getaffinity(0)
    os.sched_setaffinity(0, cores)
    try:
        began = time.monotonic()
        ran = eng.build(step, ws, env, args)
        took = time.monotonic() - began
    finally:
        try:
            eng.stop(ws, env)
        finally:
            os.sched_setaffinity(0, before)
    if ran.returncode != 0:
        raise Failure(f"{step}: `blaze build` exited {ran.returncode}:\n{ran.said()}")
    if (who := eng.not_blazes(ran)) is not None:
        raise Failure(f"{step}: built by {who}, not blaze")
    return ws / "target", took


def compare(eng: engine.Engine, rep: report.Report, subject: Subject, top: Path, profile: Profile,
            cores_a: set[int], cores_b: set[int]) -> None:
    name = f"{subject.label} {profile}"
    try:
        built_a, took_a = pinned_build(eng, subject, top, profile, cores_a)
        first = top / f"target-{profile}-a"
        shutil.rmtree(first, ignore_errors=True)
        shutil.move(built_a, first)
        second, took_b = pinned_build(eng, subject, top, profile, cores_b)
    except Failure as e:
        rep.add(name, "fail", str(e))
        return
    sub = profile_dir(profile)
    a, b = engine.outputs(first, sub), engine.outputs(second, sub)
    numbers: dict[str, report.Number] = dict(files=len(a), cores_a=len(cores_a), cores_b=len(cores_b),
                                             seconds_a=round(took_a, 1), seconds_b=round(took_b, 1))
    if not a and not b:
        raise engine.Failed(f"{name}: neither build delivered a file under target/{sub}")
    found = differences(a, b)
    if not found:
        rep.add(name, "pass", f"{len(a)} files the same on {len(cores_a)} and {len(cores_b)} cores", **numbers)
        return
    proof = engine.proof_beside(str(eng.blaze))
    lines = [f"{path}: {how}" + (f", first at {engine.first_difference_by(proof, first / sub / path, second / sub / path)}"
                                 if how == "differs" else "") for path, how in found[:SHOWN]]
    more = f"\n... and {len(found) - SHOWN} more" if len(found) > SHOWN else ""
    rep.add(name, "fail", f"{len(found)} of {len(a.keys() | b.keys())} files differ:\n" + "\n".join(lines) + more,
            differing=len(found), **numbers)


def fixture_subject(top: Path, seed: str) -> Subject:
    """The oracle's fixture in the state of `seed`, written once into `top/source`, with the
    environment variables that state is built with and, where it asks for one, an empty
    directory in front of PATH (as oracle.py builds it)."""
    state = fixture.default_state() if seed == "base" else fixture.random_state(seed)
    source = top / "source"
    shutil.rmtree(source, ignore_errors=True)
    source.mkdir(parents=True)
    fixture.write(source, state)
    want = fixture.environment(state)
    env = {name: value for name in ("ORACLE_SCRIPT_VALUE", "ORACLE_SOURCE_ENV") if (value := want[name]) is not None}
    front: Path | None = None
    if want["PATH_EXTRA"]:
        front = top / "path-extra"
        front.mkdir(exist_ok=True)
    return Subject(f"fixture ({seed})", source, env=env, path_front=front)


def project_subject(name: str, top: Path) -> Subject:
    """bench.PROJECTS' `name` at its pinned revision, with its dependencies fetched once into
    `top/source`, so that both builds read the same lock file and need no network."""
    spec, ws = budgets.checkout(name, top / "source")
    env = budgets.project_env(spec)
    fetched = engine.cargo(f"{name}: cargo fetch", ["fetch"], ws, engine.reference_env(top / "fetch-target", env))
    if fetched.returncode != 0:
        raise engine.Failed(f"{name}: `cargo fetch` exited {fetched.returncode}:\n{fetched.said()}")
    return Subject(name, top / "source", str(ws.relative_to(top / "source")) if ws != top / "source" else "",
                   tuple(spec.get("build") or ()), env)


def main(args: Input) -> None:
    if sys.platform != "linux":
        fail(SCRIPT, f"a build is pinned to its cores with sched_setaffinity, as Linux's taskset does; {sys.platform} "
             "has none")
    unknown = [name for name in args.projects if name not in budgets.PROJECTS]
    if unknown:
        fail(SCRIPT, f"projects: no {', '.join(unknown)} in bench.PROJECTS (they are {', '.join(budgets.PROJECTS)})")
    available = sorted(os.sched_getaffinity(0))
    try:
        cores_a = cpu_set(available, args.cores_a)
        cores_b = cpu_set(available, args.cores_b or len(available))
    except ValueError as e:
        fail(SCRIPT, f"cores_a, cores_b: {e}")
    if len(cores_a) == len(cores_b):
        fail(SCRIPT, f"cores_a and cores_b are both {len(cores_a)}: the two builds have to run on different counts")
    try:
        eng = engine.Engine.of(args.engine)
    except engine.Failed as e:
        fail(SCRIPT, str(e))
    rep = report.Report(SCRIPT, args.engine, args.out)
    work = Path(args.work or tempfile.mkdtemp(prefix="bz-determinism-", dir="/tmp")).resolve()
    work.mkdir(parents=True, exist_ok=True)
    print(f"work: {work}; cores {len(cores_a)} and {len(cores_b)}", flush=True)
    subjects: list[tuple[str, Callable[[Path], Subject]]] = [
        (name, lambda top, name=name: project_subject(name, top)) for name in dict.fromkeys(args.projects)]
    if args.fixture:
        subjects.append(("fixture", lambda top: fixture_subject(top, args.fixture_seed)))

    def each(subject: Subject, top: Path) -> None:
        for profile in sorted(set(args.profiles), key=args.profiles.index):
            name = f"{subject.label} {profile}"
            rep.guarded(name, lambda p=profile: budgets.failing(name, lambda: compare(eng, rep, subject, top, p, cores_a,
                                                                                      cores_b)))

    for label, make in subjects:
        top = work / label
        shutil.rmtree(top, ignore_errors=True)
        top.mkdir(parents=True)
        subject = rep.guarded(f"{label}: setting up", lambda: budgets.failing(label, lambda: make(top)))
        if subject is not None:
            each(subject, top)
        if not args.keep:
            shutil.rmtree(top, ignore_errors=True)
    rep.extra.update(work=str(work), cores_a=len(cores_a), cores_b=len(cores_b), profiles=list(args.profiles))
    rep.finish()


def expect(holds: bool, what: str) -> None:
    if not holds:
        raise SystemExit(f"{SCRIPT} selftest: {what}")


def selftest() -> None:
    expect(cpu_set([3, 1, 2, 0], 2) == {0, 1}, "the first cores are taken")
    expect(cpu_set([5, 7], 2) == {5, 7}, "all of them when asked")
    for n in (0, 3):
        try:
            cpu_set([0, 1], n)
            expect(False, f"{n} of 2 cores is refused")
        except ValueError:
            pass
    expect(differences({"a": "1", "b": "2"}, {"a": "1", "b": "2"}) == [], "the same outputs have no difference")
    expect(differences({"a": "1", "b": "2", "d": "5"}, {"a": "1", "b": "3", "c": "4"})
           == [("b", "differs"), ("c", "only in the second build"), ("d", "only in the first build")],
           "every difference is named, in order")
    expect(profile_dir("dev") == "debug" and profile_dir("release") == "release", "profile directories")
    with tempfile.TemporaryDirectory() as tmp:
        for seed in ("base", "1"):
            top = Path(tmp) / seed
            top.mkdir()
            subject = fixture_subject(top, seed)
            state = fixture.default_state() if seed == "base" else fixture.random_state(seed)
            want = fixture.environment(state)
            expect((subject.source / "Cargo.toml").is_file(), f"fixture {seed}: the workspace is written")
            expect(all(subject.env.get(name) == want[name] for name in ("ORACLE_SCRIPT_VALUE", "ORACLE_SOURCE_ENV")),
                   f"fixture {seed}: its environment is {want}, not {subject.env}")
            expect((subject.path_front is not None) == bool(want["PATH_EXTRA"]), f"fixture {seed}: PATH_EXTRA")
    print(f"{SCRIPT} selftest: ok")


if __name__ == "__main__":
    if len(sys.argv) != 2:
        sys.exit(f"usage: {SCRIPT} '<json>' | selftest\n\n{__doc__}")
    if sys.argv[1] == "selftest":
        selftest()
    else:
        main(parse(Input, sys.argv[1], SCRIPT))
