"""determinism.py: every corpus build twice, on different core counts, byte for byte the same,
and one program's functions taken from the store another program's build filled, each computed
again to the same bytes (docs/v2/plan.md, "Deterministic", and docs/v2/plan.md). Linux only: a
build is pinned to its cores with the scheduler's affinity, as `taskset -c` pins one.

    uv run --frozen python harness/v2/determinism.py '{"engine": {"kind": "v2", "blaze": "target/release/blaze"}, "out": "/tmp/determinism.json"}'
    uv run --frozen python harness/v2/determinism.py '{"engine": {...}, "out": "...", "projects": ["ripgrep", "just", "fd"], "cores_a": 4}'
    uv run --frozen python harness/v2/determinism.py selftest

The input is one JSON object (inline, a file's path, or `-`):

    engine        the blaze measured (engine.EngineSpec)
    out           the result's path (report.py's JSON)
    work          where the checkout and its daemon's home (store and build directory) go; a new
                  directory under /tmp when empty
    projects      the projects of bench.PROJECTS built, at their pinned revisions (ripgrep, just;
                  name more to build more)
    fixture       also the oracle's fixture (harness/v2/fixture.py), built with the environment
                  its state asks for (true)
    fixture_seed  its state: "base" for the default one, any other seed for a random one ("base")
    profiles      dev, release or both (both)
    cores_a       the cores of the first build (2)
    cores_b       the cores of the second (0: every core this script may run on); not cores_a
    across        also the check across units, below (true)
    keep          keep the work directory (false)

For each project and profile: two cold builds of the same source at the same path, each with an
empty home (so an empty store and build directory) and its own daemon, the first pinned to the first
`cores_a` of the cores this script may run on and the second to the first `cores_b`; then
`engine.outputs` of both (every program and library a build delivers, by its BLAKE3 digest). One
outcome per (project, profile): pass when both builds delivered the same files with the same
bytes; fail naming each file that differs and where (`engine.first_difference_by`, the
`blaze-v2-proof` beside the blaze measured), or that is in
one build only, or when a build failed or was not blaze's.

Across units (`across units release`): two cold builds on two cores give the same bytes even when a
key misses an input, as long as both ask for the same things in the same order. What a build
finds in the store was put there by another build, of another program, which reached each
function by another way; a key that misses an input then holds one program's code where the
other's differs (rule keys-cover-every-input). So `harness/v2/across`, two programs over the
same functions of the standard library, is built in release: `first` into an empty home, then,
by a new daemon on that home's store, `second` with `BLAZE_VERIFY_STORE=1`, which computes
every function the store gives it again and refuses one whose bytes are not the stored ones
(docs/v2/plan/design/verify.md). It passes when `second` builds; it fails naming each function
refused. `second` is also built into an empty home of its own: the functions that build compiled
and the one on `first`'s store did not are the ones checked, and none checked is an error, not a
pass. On 2026-11-11 this found 36 functions of the standard library compiled two ways under one
key (`alloc::vec::from_elem::<u8>` among them): a release compile kept two requests of one
function apart by the crate each calling body was compiled in, which follows the order a unit
reaches its functions in (crates/hir-ty-codegen/v2/release.rs, `Requested`).

`selftest` checks the comparison, the choice of cores, the fixture's workspace and environment
and how a refusal is read, without blaze, cargo or the network.
"""

import os
import re
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
# The workspace of the check across units, and its outcome's name.
ACROSS = HERE / "across"
ACROSS_NAME = "across units release"
# A function the store check refused (`blaze_store_check::Mismatch`'s words): its name and the
# key it is stored under.
REFUSED = re.compile(r"BLAZE_VERIFY_STORE: the store's value of `(.+?)` \(identity ([0-9a-f]{64})\) is not what "
                     r"computing it again gives")


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
    across: bool = True
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
    """A cold build of `subject` at `top/src`, with an empty home (so an empty store and build
    directory: engine.layout) and a daemon of its own, every process of it on `cores`; its build
    directory (under the home, at the same path each time, as the source's path is the same)
    and the seconds."""
    tree, home = top / "src", top / "home"
    for path in (tree, home):
        shutil.rmtree(path, ignore_errors=True)
    shutil.copytree(subject.source, tree, symlinks=True, ignore=budgets.left_out)
    ws = tree / subject.sub if subject.sub else tree
    env = eng.env(home, None, subject.env)
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
    return eng.build_dir(ws, env), took


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
        raise engine.Failed(f"{name}: neither build delivered a file under its build directory's {sub}")
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


def refused(said: str) -> list[tuple[str, str]]:
    """Each function a build's output says the store check refused, once, in the order said: its
    name and the key it is stored under."""
    return list(dict.fromkeys((name, key) for name, key in REFUSED.findall(said)))


def across(eng: engine.Engine, rep: report.Report, top: Path) -> None:
    """The check across units (the module's text): `first` into an empty home, `second` on that
    home's store with every hit computed again, `second` into an empty home of its own."""
    tree, shared, alone = top / "src", top / "home", top / "home-alone"
    for path in (tree, shared, alone):
        shutil.rmtree(path, ignore_errors=True)
    shutil.copytree(ACROSS, tree)

    def build(step: str, home: Path, member: str, check: bool) -> tuple[engine.Ran, str]:
        """A release build of `member` by a daemon of its own in `home`, and what it is called."""
        label = f"{ACROSS_NAME}: {step}"
        env = eng.env(home, None, {"BLAZE_VERIFY_STORE": "1"} if check else None)
        try:
            return eng.build(label, tree, env, ["--release", "-p", member, "--message-format=json"]), label
        finally:
            eng.stop(tree, env)

    def compiled(ran: engine.Ran, label: str) -> int:
        lines = engine.worked_lines(ran.stdout)
        return engine.work_count(label, engine.required(label, lines[0] if lines else None), "compiled")

    first, label = build("`first` into an empty store", shared, "first", False)
    if first.returncode != 0:
        rep.add(ACROSS_NAME, "fail", f"{label}: `blaze build` exited {first.returncode}:\n{first.said()}")
        return
    second, label = build("`second` on the store `first` filled, every hit computed again", shared, "second", True)
    found = refused(second.stdout + second.stderr)
    if found:
        lines = [f"`{name}` (key {key[:16]})" for name, key in found[:SHOWN]]
        more = f"\n... and {len(found) - SHOWN} more" if len(found) > SHOWN else ""
        rep.add(ACROSS_NAME, "fail", f"{len(found)} functions `first`'s build kept in the store are not what `second`'s "
                "build computes under the same key, so the key misses an input:\n" + "\n".join(lines) + more,
                differing=len(found))
        return
    if second.returncode != 0:
        rep.add(ACROSS_NAME, "fail", f"{label}: `blaze build` exited {second.returncode}:\n{second.said()}")
        return
    on_store = compiled(second, label)
    cold, label = build("`second` into an empty store", alone, "second", False)
    if cold.returncode != 0:
        rep.add(ACROSS_NAME, "fail", f"{label}: `blaze build` exited {cold.returncode}:\n{cold.said()}")
        return
    from_nothing = compiled(cold, label)
    checked = from_nothing - on_store
    if checked <= 0:
        raise engine.Failed(f"{ACROSS_NAME}: nothing was checked: `second` compiled {on_store} functions on the store "
                            f"`first` filled and {from_nothing} from nothing, so it took none from that store")
    rep.add(ACROSS_NAME, "pass", f"{checked} functions `second` took from the store `first` filled, each computed "
            "again to the stored bytes", checked=checked, compiled_on_store=on_store, compiled_from_nothing=from_nothing)


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
    work = engine.work_dir(args.work, "bz-determinism-")
    work.mkdir(parents=True, exist_ok=True)
    print(f"work: {work}; cores {len(cores_a)} and {len(cores_b)}", flush=True)
    subjects: list[tuple[str, Callable[[Path], Subject]]] = [
        (name, lambda top, name=name: project_subject(name, top)) for name in dict.fromkeys(args.projects)]
    if args.fixture:
        subjects.append(("fixture", lambda top: fixture_subject(top, args.fixture_seed)))

    def each(subject: Subject, top: Path) -> None:
        # Declared: the lambdas below capture it, and a captured loop variable's inferred type is `str`.
        profile: Profile
        for profile in dict.fromkeys(args.profiles):
            name = f"{subject.label} {profile}"
            rep.guarded(name, lambda: budgets.failing(name, lambda: compare(eng, rep, subject, top, profile, cores_a,
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
    if args.across:
        top = work / "across"
        shutil.rmtree(top, ignore_errors=True)
        top.mkdir(parents=True)
        rep.guarded(ACROSS_NAME, lambda: budgets.failing(ACROSS_NAME, lambda: across(eng, rep, top)))
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
    one = ("BLAZE_VERIFY_STORE: the store's value of `<A as B>::c::<u8>` (identity " + "ab" * 32 + ") is not what "
           "computing it again gives: they differ first at byte 9 (stored 0x04, recomputed 0x0c; 5171 bytes stored, "
           "5259 recomputed); its entry recorded no reads (it is keyed by its whole content)")
    other = one.replace("ab" * 32, "cd" * 32).replace("::c::<u8>", "::d")
    expect(refused(f"error: ...\\n  {one}\\n  {other}\n{one}\n") == [("<A as B>::c::<u8>", "ab" * 32),
                                                                   ("<A as B>::d", "cd" * 32)],
           "each function refused is read once, in order, from JSON's text and plain text alike")
    expect(refused("Finished `release` profile [optimized] target(s) in 2.31s") == [], "a build that refused nothing")
    for path in ("Cargo.toml", "first/Cargo.toml", "first/src/main.rs", "second/Cargo.toml", "second/src/main.rs"):
        expect((ACROSS / path).is_file(), f"the workspace across units has {path}")
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
