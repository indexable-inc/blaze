"""corpus.py: real crates built, run and tested by blaze, each judged by its own verdicts: the
build succeeds, its programs run and exit 0, and its own tests pass (docs/v2/plan/lanes/proof.md).

    uv run --frozen python harness/v2/corpus.py '{"engine": {"kind": "v1", "blaze": "target/release/blaze"}, "out": "/tmp/corpus.json"}'
    uv run --frozen python harness/v2/corpus.py '{"engine": {...}, "out": "...", "projects": ["crates.io"], "crates": ["itoa"]}'
    uv run --frozen python harness/v2/corpus.py selftest

The input is one JSON object (inline, a file's path, or `-` for stdin):

    engine    the blaze under test (engine.EngineSpec)
    out       the path the result JSON is written to (report.py)
    work      the directory everything is built in; empty: a new one under /tmp
    projects  which of PROJECTS run; empty: all of them
    crates    which crates of corpus.json the project `crates.io` runs, by name; empty: all
    commit    the commit of this repository the project `blaze` is (HEAD)
    jobs      how many projects run at once (2), each with a daemon and a home of its own
    keep      keep each project's checkout and target directory after it ran
    backends  the dev tier's backends to run every project at, two or more of "stencil" and
              "cranelift" (engine.EngineSpec.dev_backend, which is then "default"); empty:
              one run at the engine's own

The projects:

    blaze      this repository at `commit`, exported with `git archive`: `blaze-engine`'s
               programs built, `blaze --version` and `blaze-engine --help` run from a directory
               outside any workspace, and the tests of the crates in BLAZE_TESTS
    nushell, ripgrep, helix
               pinned, cloned and run as blaze_cli.PROJECTS says (`build`, `run`, `env`) and
               tested as bench.PROJECTS says (`test`); the two have to pin the same commit
    crates.io  each crate of corpus.json: its `.crate` from static.crates.io, checked against
               the sha256 the sparse index (index.crates.io) lists for that version when it
               is fetched, unpacked, and its own tests run

blaze does each step through the engine into the workspace's `target`. Each (project, step) is
one outcome:

    pass   a build exits 0; a program (`run`, `exec`) exits 0; in a test run every test that
           ran passed but the project's known failures (OPEN), and it exited 0 or with only
           those failed (blaze_cli.test_verdict)
    fail   any of those does not hold, a test in OPEN passed or did not run (take it out), or
           the engine says the step was not blaze's (`not_blazes`)
    error  the harness could not say: a fetch or a checksum failed, a command timed out, or the
           engine gave an answer it could not read
    unmeasured
           the engine has no answer to a step (`engine.Unmeasurable`)

With `backends`, each project's steps run once per backend, in order, each run with a daemon
home of its own and a clean `target`. Each outcome above is named `<project> [<backend>]:
<step>`, and each (project, step) that every backend ran is one more, `<project>: <step>:
<first> against <other>`, for each backend after the first:

    pass   the two blaze runs exit alike, panic alike (each `thread '…' panicked at` with its
           message, as a multiset: tests panic in any order) and gave the same output
           (blaze_cli.program_output, blaze_cli.test_results)
    fail   any of those differ, named with both backends

A project stops at a build step that does not pass. Each project's dependencies resolve once,
by `cargo fetch`, into the `Cargo.lock` every run builds with; the result's `extra.locks`
names its sha256 per project, so two runs that resolved differently show it.
`selftest` runs the checks of the parts that
need neither blaze nor the network.
"""

import collections
import concurrent.futures
import dataclasses
import hashlib
import importlib
import io
import json
import os
import re
import shutil
import subprocess
import sys
import tarfile
import tempfile
import time
import urllib.error
import urllib.request
from collections.abc import Callable
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Literal, cast, get_args

HERE = Path(__file__).resolve().parent
sys.path[:0] = [str(HERE), str(HERE.parent)]
from typed_input import fail, parse
import engine, report

blaze_cli: Any = importlib.import_module("blaze_cli")

SCRIPT = "corpus.py"
Name = Literal["blaze", "nushell", "ripgrep", "helix", "crates.io"]
Command = Literal["build", "run", "test", "exec"]
type Forced = Literal["stencil", "cranelift"]
PINNED: tuple[Name, ...] = ("nushell", "ripgrep", "helix")
BLAZE_TESTS = ("syntax-tree", "blaze-protocol", "stdx", "intern")
INDEX = "https://index.crates.io/"
DOWNLOAD = "https://static.crates.io/crates/{name}/{name}-{version}.crate"
FETCH_SECS = 120
EXEC_SECS = 600
# Tests of a project known to fail under blaze, by the project's name (`Program.name`, its
# version in it) and libtest's name of the test, each with why: OPEN of unittests.py. One that
# passes or no longer runs fails its step until it is taken out.
OPEN: dict[str, dict[str, str]] = {}


@dataclass(frozen=True)
class Crate:
    name: str
    version: str


@dataclass(frozen=True)
class Corpus:
    """corpus.json."""

    about: str
    crates: tuple[Crate, ...]


@dataclass(frozen=True)
class Input:
    engine: engine.EngineSpec
    out: str
    work: str = ""
    projects: tuple[Name, ...] = ()
    crates: tuple[str, ...] = ()
    commit: str = "HEAD"
    jobs: int = 2
    keep: bool = False
    backends: tuple[Forced, ...] = ()


def refused_backends(args: Input) -> str | None:
    """What is wrong with the input's `backends`, or None."""
    if not args.backends:
        return None
    if len(args.backends) < 2:
        return f"backends: two or more to compare, not {list(args.backends)} (one run at one is engine.dev_backend)"
    if len(set(args.backends)) != len(args.backends):
        return f"backends: each once, not {list(args.backends)}"
    if args.engine.dev_backend != "default":
        return (f"backends {list(args.backends)} with engine.dev_backend {args.engine.dev_backend!r}: "
                "each run's backend is the one `backends` gives it")
    return None


@dataclass(frozen=True)
class Step:
    """One command blaze runs. `exec` runs a program the build made, `args` being its name
    under `target/debug` and its arguments."""

    label: str
    command: Command
    args: tuple[str, ...] = ()


@dataclass(frozen=True)
class Program:
    """A workspace blaze builds: `prepare` puts its checkout at the path it is given."""

    name: str
    dir: str
    prepare: Callable[[Path], None]
    steps: tuple[Step, ...]
    env: dict[str, str] = field(default_factory=dict[str, str])


@dataclass(frozen=True)
class Found:
    """An outcome, made in a worker and added to the report by the main thread."""

    name: str
    verdict: report.Verdict
    detail: str = ""
    numbers: dict[str, report.Number] = field(default_factory=dict[str, report.Number])


@dataclass(frozen=True)
class Tested:
    """What `blaze_cli.test_results` reads of a test run: with `kept`, the tests `blaze test`
    passed without running them again (its `tests.ndjson`)."""

    returncode: int
    stdout: str
    kept: list[str]


def load_corpus(path: Path = HERE / "corpus.json") -> Corpus:
    return parse(Corpus, str(path), SCRIPT)


def git(*args: str) -> str:
    done = subprocess.run(["git", "-C", str(engine.ROOT), *args], capture_output=True, text=True,
                          env={**os.environ, "TZ": "UTC0"})
    if done.returncode != 0:
        raise engine.Failed(f"`git {' '.join(args)}` exited {done.returncode}: {done.stderr.strip()}")
    return done.stdout.strip()


def export(sha: str, dest: Path) -> None:
    """The tree of commit `sha` of this repository at `dest`, as `git archive` gives it."""
    dest.mkdir(parents=True)
    with subprocess.Popen(["git", "-C", str(engine.ROOT), "archive", "--format=tar", sha],
                          stdout=subprocess.PIPE, stderr=subprocess.PIPE) as proc:
        if proc.stdout is None or proc.stderr is None:
            raise engine.Failed("git archive: no pipe")
        with tarfile.open(fileobj=proc.stdout, mode="r|") as tar:
            tar.extractall(dest, filter="data")
        said = proc.stderr.read().decode(errors="replace")
    if proc.returncode != 0:
        raise engine.Failed(f"`git archive {sha}` exited {proc.returncode}: {said.strip()}")


def blaze_program(commit: str) -> Program:
    sha = git("rev-parse", "--verify", f"{commit}^{{commit}}")
    date = git("show", "-s", "--date=format-local:%Y-%m-%d", "--format=%cd", sha)
    tests = tuple(word for crate in BLAZE_TESTS for word in ("-p", crate))
    steps = (
        Step("build", "build", ("-p", "blaze-engine", "--bins")),
        Step("blaze --version", "exec", ("blaze", "--version")),
        Step("blaze-engine --help", "exec", ("blaze-engine", "--help")),
        Step("test", "test", tests),
    )
    # The export has no `.git`: its build script is told the commit, as release.yaml tells it.
    env = {"BLAZE_GIT_SHA": sha[:7], "BLAZE_GIT_DATE": date}
    return Program(f"blaze@{sha[:12]}", "blaze", lambda ws: export(sha, ws), steps, env)


def pinned_program(name: Name) -> Program:
    bench: Any = importlib.import_module("bench")
    spec: dict[str, Any] = blaze_cli.PROJECTS[name]
    timed: dict[str, Any] = bench.PROJECTS[name]
    if spec["rev"] != timed["rev"]:
        raise engine.Failed(f"{name}: blaze_cli.py pins {spec['rev']} and bench.py {timed['rev']}")
    build = tuple[str, ...](spec.get("build") or ())
    run = tuple[str, ...](spec.get("run") or ())
    steps = [Step("build", "build", build)]
    if run:
        steps.append(Step("run", "run", (*build, "--", *run)))
    steps.append(Step("test", "test", tuple[str, ...](timed.get("test", spec.get("test", ())))))

    def prepare(ws: Path) -> None:
        try:
            source = str(blaze_cli.fetch(name, spec))
        except (AssertionError, subprocess.CalledProcessError) as e:
            raise engine.Failed(f"{name}: fetching {spec['repo']} at {spec['tag']}: {e}") from None
        shutil.copytree(source, ws, symlinks=True, ignore=shutil.ignore_patterns("target"))

    env = dict[str, str](spec.get("env") or {})
    return Program(f"{name}@{spec['tag']}", name, prepare, tuple(steps), env)


def index_path(name: str) -> str:
    """Where the sparse index keeps the versions of crate `name`."""
    low = name.lower()
    if len(low) <= 2:
        return f"{len(low)}/{low}"
    if len(low) == 3:
        return f"3/{low[0]}/{low}"
    return f"{low[:2]}/{low[2:4]}/{low}"


def listed_cksum(index: str, name: str, version: str) -> str:
    """The sha256 the index file `index` of crate `name` lists for `version`."""
    for line in index.splitlines():
        if not line.strip():
            continue
        entry: Any = json.loads(line)
        if not isinstance(entry, dict):
            continue
        fields = cast("dict[str, Any]", entry)
        if fields.get("vers") != version:
            continue
        cksum: Any = fields.get("cksum")
        if not isinstance(cksum, str) or not re.fullmatch(r"[0-9a-f]{64}", cksum):
            raise engine.Failed(f"{name} {version}: the index lists the checksum {cksum!r}")
        return cksum
    raise engine.Failed(f"{name}: the index lists no version {version}")


def verified(data: bytes, cksum: str, what: str) -> bytes:
    got = hashlib.sha256(data).hexdigest()
    if got != cksum:
        raise engine.Failed(f"{what}: its sha256 is {got}, and the index lists {cksum}")
    return data


def download(url: str) -> bytes:
    try:
        with urllib.request.urlopen(url, timeout=FETCH_SECS) as answer:
            body: bytes = answer.read()
            return body
    except (urllib.error.URLError, OSError) as e:
        raise engine.Failed(f"fetching {url}: {e}") from None


def fetch_crate(crate: Crate, cache: Path) -> bytes:
    """The `.crate` of `crate`, kept in `cache` once it was verified; verified against the index
    again each time."""
    cksum = listed_cksum(download(INDEX + index_path(crate.name)).decode(), crate.name, crate.version)
    what = f"{crate.name}-{crate.version}.crate"
    kept = cache / what
    if kept.is_file():
        return verified(kept.read_bytes(), cksum, str(kept))
    data = verified(download(DOWNLOAD.format(name=crate.name, version=crate.version)), cksum, what)
    cache.mkdir(parents=True, exist_ok=True)
    part = kept.with_name(what + f".{os.getpid()}.part")
    part.write_bytes(data)
    part.replace(kept)
    return data


def unpack(data: bytes, top: str, dest: Path) -> None:
    """The `.crate` `data`, every entry of which is under `top/`, unpacked as `dest`."""
    staging = dest.with_name(dest.name + ".unpack")
    shutil.rmtree(staging, ignore_errors=True)
    try:
        with tarfile.open(fileobj=io.BytesIO(data), mode="r:gz") as tar:
            stray = [n for n in tar.getnames() if n != top and not n.startswith(top + "/")]
            if stray:
                raise engine.Failed(f"{top}.crate has entries outside {top}/: {stray[:3]}")
            tar.extractall(staging, filter="data")
    except (tarfile.TarError, OSError) as e:
        raise engine.Failed(f"unpacking {top}.crate: {e}") from None
    (staging / top).rename(dest)
    shutil.rmtree(staging)


def crate_program(crate: Crate, cache: Path) -> Program:
    top = f"{crate.name}-{crate.version}"
    return Program(f"crates.io/{crate.name}@{crate.version}", f"c-{top}",
                   lambda ws: unpack(fetch_crate(crate, cache), top, ws), (Step("test", "test"),))


def judge(command: Command, ran: engine.Ran, said: object, who: str | None,
          known: frozenset[str]) -> tuple[report.Verdict, str]:
    """The verdict of a step by its own result: a build or a program exits 0; a test run passes
    by its own tests (`blaze_cli.test_verdict`, `known` its known failures). A step blaze did
    not do (`who`, from `not_blazes`) fails."""
    if who is not None:
        return "fail", f"not blaze's: {who}"
    if command == "test":
        wrong: str | None = blaze_cli.test_verdict(said, known)
        return ("fail", f"{wrong}\n{ran.said(1500)}") if wrong else ("pass", "")
    if ran.returncode != 0:
        what = "build" if command == "build" else "program"
        return "fail", f"the {what} exits {ran.returncode}\n{ran.said(1500)}"
    return "pass", ""


@dataclass(frozen=True)
class Outcome:
    """What a step gave, as the backends are compared: its exit status, what it said
    (`blaze_cli.program_output`, `test_results`, or a build's status) and its panics
    (`panics`)."""

    status: int
    said: object
    panics: tuple[str, ...]

# A panic as the pinned nightly's std prints it: `thread 'main' panicked at src/main.rs:2:5:`
# then the message on the next line (a thread id in parentheses after the name since 1.88).
PANIC = re.compile(r"^thread '([^']*)'(?: \(\d+\))? panicked at (.*?)(:?)$(?:\n(.*))?", re.M)


def panics(text: str) -> tuple[str, ...]:
    """Each panic `text` reports, by thread, place and message, sorted: test threads panic in
    any order."""
    found: list[str] = []
    for match in PANIC.finditer(text):
        thread, at, colon, message = match.groups()
        found.append(f"{thread} at {at}: {message}" if colon and message is not None else f"{thread} at {at}")
    return tuple(sorted(found))


def said_difference(x: str, a: object, y: str, b: object) -> str:
    """The first thing that differs between what backends `x` and `y` gave (`Outcome.said`)."""
    if isinstance(a, tuple) and isinstance(b, tuple):
        left, right = cast(tuple[object, ...], a), cast(tuple[object, ...], b)
        if len(left) == len(right):
            for p, q in zip(left, right):
                if p != q:
                    return said_difference(x, p, y, q)
    if isinstance(a, collections.Counter) and isinstance(b, collections.Counter):
        p, q = cast(collections.Counter[object], a), cast(collections.Counter[object], b)
        return (f"tests differ: {x} only {sorted((p - q).items(), key=repr)[:3]}, "
                f"{y} only {sorted((q - p).items(), key=repr)[:3]}")
    if isinstance(a, str) and isinstance(b, str):
        pairs = [(p, q) for p, q in zip(a.splitlines(), b.splitlines()) if p != q][:2]
        return f"output differs ({x}, {y}): {pairs or (a[-200:], b[-200:])}"
    return f"{x} gave {a!r:.300} and {y} {b!r:.300}"


def backend_difference(x: str, a: Outcome, y: str, b: Outcome) -> str | None:
    """None when backends `x` and `y` ran a step alike: the same exit status, the same panics
    and the same output; else the first of those that differs."""
    if a.status != b.status:
        return f"{x} exits {a.status} and {y} {b.status}"
    if a.panics != b.panics:
        p, q = collections.Counter(a.panics), collections.Counter(b.panics)
        return f"panics differ: {x} only {sorted(p - q)[:3]}, {y} only {sorted(q - p)[:3]}"
    if a.said != b.said:
        return said_difference(x, a.said, y, b.said)
    return None


def compare_backends(project: str, steps: tuple[Step, ...],
                     runs: list[tuple[Forced, dict[str, Outcome]]]) -> list[Found]:
    """One outcome per step every backend of `runs` ran, comparing each backend after the
    first with the first. A step a backend did not get to (its build failed or the harness
    could not do it) is said by that run's own outcomes, not compared."""
    if len(runs) < 2:
        return []
    (first, base), rest = runs[0], runs[1:]
    found: list[Found] = []
    for step in steps:
        for other, ran in rest:
            if step.label not in base or step.label not in ran:
                continue
            differs = backend_difference(first, base[step.label], other, ran[step.label])
            name = f"{project}: {step.label}: {first} against {other}"
            found.append(Found(name, "fail", differs) if differs is not None else Found(name, "pass"))
    return found


def step_of(eng: engine.Engine, program: Program, step: Step, ws: Path, scratch: Path,
            ours_env: dict[str, str], label: str) -> tuple[Found, Outcome]:
    """The outcome of `step` named `label: <step>`, and what it gave."""
    name = f"{label}: {step.label}"
    if step.command == "exec":
        program_name, *args = step.args
        cwd = scratch / "cwd"
        cwd.mkdir(exist_ok=True)
        env = {**engine.scrubbed(), **program.env, "BLAZE_HOME": str(scratch / "program-home")}
        ours = engine.run(name, [str(ws / "target" / "debug" / (program_name + engine.EXE)), *args], cwd, env, EXEC_SECS)
        who = None
        mine = blaze_cli.program_output(ours)
    else:
        tests = ws / "target" / "blaze" / "tests.ndjson"
        if step.command == "test":
            tests.unlink(missing_ok=True)
        ours = eng.build(name, ws, ours_env, step.args, command=step.command)
        who = eng.not_blazes(ours)
        if step.command == "test":
            kept: list[str] = blaze_cli.kept_passes(str(tests))
            mine = blaze_cli.test_results(Tested(ours.returncode, ours.stdout, kept))
        elif step.command == "run":
            mine = blaze_cli.program_output(ours)
        else:
            mine = (ours.returncode,)
    verdict, detail = judge(step.command, ours, mine, who, frozenset(OPEN.get(program.name, {})))
    found = Found(name, verdict, detail, {"blaze_s": round(ours.seconds, 2)})
    return found, Outcome(ours.returncode, mine, panics(ours.stdout + "\n" + ours.stderr))


def unfinished(label: str, doing: str, e: Exception) -> Found:
    """The outcome of a step the harness could not finish."""
    if isinstance(e, engine.Unmeasurable):
        return Found(f"{label}: {doing}", "unmeasured", str(e))
    if isinstance(e, engine.Failed):
        return Found(f"{label}: {doing}", "error", str(e))
    return Found(f"{label}: {doing}", "error", f"{type(e).__name__}: {e}")


def run_steps(eng: engine.Engine, program: Program, ws: Path, root: Path, home: Path, store: Path,
              label: str) -> tuple[list[Found], dict[str, Outcome]]:
    """Every step of `program` through `eng` with its daemon at `home`, until a build step
    that does not pass: their outcomes named `label: <step>`, and what each gave by step."""
    ours_env = eng.env(home, store, extra=program.env)
    found: list[Found] = []
    ran: dict[str, Outcome] = {}
    doing = ""
    try:
        for step in program.steps:
            doing = step.label
            done, outcome = step_of(eng, program, step, ws, root, ours_env, label)
            found.append(done)
            ran[step.label] = outcome
            if step.command == "build" and done.verdict != "pass":
                break
    except (engine.Unmeasurable, engine.Failed, OSError) as e:
        found.append(unfinished(label, doing, e))
    finally:
        try:
            if home.is_dir():
                eng.stop(ws, ours_env)
        except engine.Failed as e:
            found.append(Found(f"{label}: stopping the daemon", "error", str(e)))
    return found, ran


def run_program(engines: list[tuple[Forced | None, engine.Engine]], program: Program, root: Path, store: Path,
                keep: bool) -> tuple[list[Found], str]:
    """Every outcome of `program`, built in `root` once per engine of `engines` (one per
    backend, or one with None), and the sha256 of the `Cargo.lock` every run built with (""
    when it never got that far). Never raises for what the harness could not do: that is an
    outcome too."""
    ws = root / "ws"
    shutil.rmtree(root, ignore_errors=True)
    root.mkdir(parents=True)
    try:
        program.prepare(ws)
    except (engine.Failed, OSError) as e:
        return [Found(f"{program.name}: fetch", "error", str(e))], ""
    found: list[Found] = []
    lock = ""
    doing = "cargo fetch"
    try:
        fetch_env = engine.reference_env(root / "fetch-target", program.env)
        engine.run(f"{program.name}: cargo fetch", ["cargo", "fetch"], ws, fetch_env, 1800, check=True)
        lock = hashlib.sha256((ws / "Cargo.lock").read_bytes()).hexdigest()
        doing = "running it"
        runs: list[tuple[Forced, dict[str, Outcome]]] = []
        for at, (backend, eng) in enumerate(engines):
            if at:
                # Each backend's run starts from no output of the one before it.
                shutil.rmtree(ws / "target", ignore_errors=True)
            home = root / ("h" if backend is None else f"h-{backend}")
            label = program.name if backend is None else f"{program.name} [{backend}]"
            done, ran = run_steps(eng, program, ws, root, home, store, label)
            found += done
            if backend is not None:
                runs.append((backend, ran))
        found += compare_backends(program.name, program.steps, runs)
    except (engine.Unmeasurable, engine.Failed, OSError) as e:
        found.append(unfinished(program.name, doing, e))
    finally:
        if not keep:
            shutil.rmtree(root, ignore_errors=True)
    return found, lock


def largest_first(programs: list[Program]) -> list[int]:
    """The indices of `programs` in the order to start them: the whole workspaces (blaze, nushell, ripgrep,
    helix: minutes each) before the crates.io crates (seconds each), each group as given. The
    longest first is Graham's LPT rule, so no big build runs alone at the end while the other
    slots sit idle; the report keeps the input's order all the same."""
    return sorted(range(len(programs)), key=lambda i: programs[i].name.startswith("crates.io/"))


def overhead(before: os.times_result, after: os.times_result, wall_s: float) -> dict[str, float]:
    """The harness's own CPU time over a run against its children's (cargo fetching, blaze's
    command line, the programs), whose share is to stay under 1%. A daemon blaze detaches is no
    child, so its CPU time is left out: the share is an upper bound."""
    own = (after.user - before.user) + (after.system - before.system)
    children = (after.children_user - before.children_user) + (after.children_system - before.children_system)
    return dict(wall_s=round(wall_s, 3), harness_cpu_s=round(own, 3), children_cpu_s=round(children, 3),
                harness_share=round(own / (own + children), 5) if own + children > 0 else 0.0)


def programs_of(args: Input, corpus: Corpus, cache: Path) -> list[Program]:
    names = args.projects or get_args(Name)
    chosen: list[Program] = []
    for name in dict.fromkeys(names):
        if name == "blaze":
            chosen.append(blaze_program(args.commit))
        elif name == "crates.io":
            chosen += [crate_program(c, cache) for c in corpus.crates if not args.crates or c.name in args.crates]
        else:
            chosen.append(pinned_program(name))
    return chosen


def engines_of(args: Input) -> list[tuple[Forced | None, engine.Engine]]:
    """The engine of each run of a project: one per backend of `backends`, its dev tier forced
    to it, or the input's engine alone."""
    if not args.backends:
        return [(None, engine.Engine.of(args.engine))]
    return [(b, engine.Engine.of(dataclasses.replace(args.engine, dev_backend=b))) for b in args.backends]


def main(args: Input) -> None:
    if args.jobs < 1:
        fail(SCRIPT, f"jobs: at least 1, not {args.jobs}")
    corpus = load_corpus()
    if unknown := sorted(set(args.crates) - {c.name for c in corpus.crates}):
        fail(SCRIPT, f"crates: corpus.json has no {', '.join(unknown)}")
    if wrong := refused_backends(args):
        fail(SCRIPT, wrong)
    try:
        engines = engines_of(args)
    except engine.Failed as e:
        fail(SCRIPT, str(e))
    work = Path(args.work or tempfile.mkdtemp(prefix="bz-corpus-", dir="/tmp")).resolve()
    work.mkdir(parents=True, exist_ok=True)
    try:
        programs = programs_of(args, corpus, work / "crates.io")
    except engine.Failed as e:
        fail(SCRIPT, str(e))
    rep = report.Report(SCRIPT, args.engine, args.out)
    rep.extra.update(work=str(work), programs=[p.name for p in programs], toolchain=engine.TOOLCHAIN,
                     backends=list(args.backends))
    print(f"work: {work}", flush=True)
    store = work / "store"
    locks: dict[str, str] = {}
    began, wall = os.times(), time.monotonic()
    with concurrent.futures.ThreadPoolExecutor(args.jobs) as pool:
        submitted = {i: pool.submit(run_program, engines, programs[i], work / programs[i].dir, store, args.keep)
                     for i in largest_first(programs)}
        running = {submitted[i]: p.name for i, p in enumerate(programs)}
        # In the order of the projects, not the order they end in: one report for one input.
        for done, name in running.items():
            founds, lock = done.result()
            locks[name] = lock
            for found in founds:
                rep.add(found.name, found.verdict, found.detail, **found.numbers)
    rep.extra["locks"] = dict(sorted(locks.items()))
    rep.extra.update(overhead(began, os.times(), time.monotonic() - wall))
    if not args.keep:
        shutil.rmtree(store, ignore_errors=True)
    rep.finish()


def test_index_path() -> None:
    assert index_path("a") == "1/a"
    assert index_path("cc") == "2/cc"
    assert index_path("syn") == "3/s/syn"
    assert index_path("serde") == "se/rd/serde"
    assert index_path("Inflector") == "in/fl/inflector"


def test_listed_cksum() -> None:
    a, b = "a" * 64, "b" * 64
    index = "\n".join(json.dumps(dict(name="x", vers=v, cksum=c, yanked=False)) for v, c in (("1.0.0", a), ("1.1.0", b)))
    assert listed_cksum(index + "\n", "x", "1.1.0") == b
    for wrong, version in ((index, "2.0.0"), (json.dumps(dict(vers="1.0.0", cksum="nope")), "1.0.0")):
        try:
            listed_cksum(wrong, "x", version)
        except engine.Failed:
            continue
        raise AssertionError(f"{version} of {wrong!r} was taken")


def test_verified() -> None:
    data = b"crate"
    assert verified(data, hashlib.sha256(data).hexdigest(), "x") == data
    try:
        verified(data, "0" * 64, "x")
    except engine.Failed:
        return
    raise AssertionError("a checksum that differs was taken")


def crate_bytes(entries: dict[str, bytes]) -> bytes:
    out = io.BytesIO()
    with tarfile.open(fileobj=out, mode="w:gz") as tar:
        for name, body in entries.items():
            info = tarfile.TarInfo(name)
            info.size = len(body)
            tar.addfile(info, io.BytesIO(body))
    return out.getvalue()


def test_unpack() -> None:
    with tempfile.TemporaryDirectory() as tmp:
        dest = Path(tmp) / "ws"
        unpack(crate_bytes({"x-1.0.0/Cargo.toml": b"[package]\n", "x-1.0.0/src/lib.rs": b""}), "x-1.0.0", dest)
        assert (dest / "Cargo.toml").read_bytes() == b"[package]\n" and (dest / "src" / "lib.rs").is_file()
        for entries in ({"x-1.0.0/a": b"", "y/b": b""}, {"x-1.0.0/../../escaped": b""}):
            try:
                unpack(crate_bytes(entries), "x-1.0.0", Path(tmp) / "other")
            except engine.Failed:
                continue
            raise AssertionError(f"{list(entries)} was unpacked")
        assert not (Path(tmp) / "escaped").exists()


def test_judge() -> None:
    none: frozenset[str] = frozenset()
    assert judge("build", ran_with(0, ""), (0,), None, none) == ("pass", "")
    assert judge("build", ran_with(101, ""), (101,), None, none)[0] == "fail"
    assert judge("build", ran_with(0, ""), (0,), "cargo: no daemon", none) == ("fail", "not blaze's: cargo: no daemon")
    assert judge("run", ran_with(1, ""), (1, "", ""), None, none)[0] == "fail"
    assert judge("exec", ran_with(0, "hi"), (0, "hi", ""), None, none) == ("pass", "")
    failing = blaze_cli.test_results(Tested(101, LIBTEST, []))
    verdict, detail = judge("test", ran_with(101, LIBTEST), failing, None, none)
    assert verdict == "fail" and "1 tests failed: b" in detail, detail
    assert judge("test", ran_with(101, LIBTEST), failing, None, frozenset({"b"})) == ("pass", "")
    passing = LIBTEST.replace("test b ... FAILED", "test b ... ok").replace("    b\n", "").replace("1 failed", "0 failed")
    assert judge("test", ran_with(0, passing), blaze_cli.test_results(Tested(0, passing, [])), None, none) == ("pass", "")


LIBTEST = """running 2 tests
test a ... ok
test b ... FAILED

failures:

---- b stdout ----
boom

failures:
    b

test result: FAILED. 1 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
"""


def test_test_results() -> None:
    verdict = blaze_cli.test_verdict
    ran = blaze_cli.test_results(Tested(101, LIBTEST, []))
    assert verdict(ran, {"b"}) is None, verdict(ran, {"b"})
    # `a` kept by blaze (passed as it is now) ran and passed.
    kept = LIBTEST.replace("test a ... ok\n", "").replace("1 passed", "0 passed")
    assert verdict(blaze_cli.test_results(Tested(101, kept, ["a"])), {"b"}) is None
    worse = LIBTEST.replace("test a ... ok", "test a ... FAILED").replace("    b\n", "    a\n    b\n")
    said = verdict(blaze_cli.test_results(Tested(101, worse, [])), {"b"})
    assert said is not None and "failed: a" in said, said
    said = verdict(ran, {"b", "a", "c"})
    assert said is not None and "passed: a" in said and "did not run: c" in said, said
    said = verdict(blaze_cli.test_results(Tested(101, "error[E0425]: cannot find value `x`\n", [])))
    assert said is not None and "exited 101" in said and "no test ran" in said, said


def test_corpus() -> None:
    corpus = load_corpus()
    names = [c.name for c in corpus.crates]
    assert 45 <= len(names) <= 60, len(names)
    assert len(set(names)) == len(names), "a crate is listed twice"
    assert names == sorted(names), "corpus.json's crates are in order of name"
    for crate in corpus.crates:
        assert re.fullmatch(r"[A-Za-z0-9_-]+", crate.name), crate
        assert re.fullmatch(r"\d+\.\d+\.\d+", crate.version), crate


def test_pinned() -> None:
    for name in PINNED:
        program = pinned_program(name)
        assert [s.command for s in program.steps][0] == "build" and program.steps[-1].command == "test", program
    helix = pinned_program("helix")
    assert helix.env.get("HELIX_DISABLE_AUTO_GRAMMAR_BUILD") == "1"


def refused(fn: Callable[[], object]) -> str:
    """What `fn` was refused with: a `Failed`, or the exit `typed_input.parse` and `fail` make."""
    try:
        fn()
    except engine.Failed as e:
        return str(e)
    except SystemExit as e:
        return str(e.code)
    raise AssertionError(f"{fn} was taken")


def test_backends_input() -> None:
    base: dict[str, Any] = {"engine": {"kind": "v2", "blaze": sys.executable}, "out": ""}
    plain = parse(Input, json.dumps(base), SCRIPT)
    assert plain.backends == () and plain.engine.dev_backend == "default" and refused_backends(plain) is None
    both = parse(Input, json.dumps({**base, "backends": ["stencil", "cranelift"]}), SCRIPT)
    assert both.backends == ("stencil", "cranelift") and refused_backends(both) is None, both
    assert "backends[1]" in refused(lambda: parse(Input, json.dumps({**base, "backends": ["stencil", "llvm"]}), SCRIPT))
    assert "backends[0]" in refused(lambda: parse(Input, json.dumps({**base, "backends": ["default", "stencil"]}), SCRIPT))
    assert "no field" in refused(lambda: parse(Input, json.dumps({**base, "backend": ["stencil"]}), SCRIPT))
    for wrong, says in ((["stencil"], "two or more"), (["stencil", "stencil"], "each once")):
        got = refused_backends(parse(Input, json.dumps({**base, "backends": wrong}), SCRIPT))
        assert got is not None and says in got, (wrong, got)
    forced = {**base, "engine": {**base["engine"], "dev_backend": "stencil"}, "backends": ["stencil", "cranelift"]}
    got = refused_backends(parse(Input, json.dumps(forced), SCRIPT))
    assert got is not None and "engine.dev_backend" in got, got


def test_dev_backend() -> None:
    assert "engine.dev_backend" in refused(
        lambda: parse(Input, json.dumps({"engine": {"kind": "v2", "blaze": "b", "dev_backend": "gpu"}, "out": ""}), SCRIPT))
    assert "dev_backend" in refused(lambda: engine.EngineSpec("v2", "b", dev_backend=cast(Any, "gpu")))
    assert engine.V2(engine.EngineSpec("v2", sys.executable)).spec.dev_backend == "default"
    for backend in ("stencil", "cranelift"):
        v2 = engine.EngineSpec("v2", sys.executable, dev_backend=backend)
        said = refused(lambda: engine.Engine.of(v2))
        assert "codegen-backend" in said and "requests-backend-axis.md" in said, said
        assert "v1" in refused(lambda: engine.Engine.of(dataclasses.replace(v2, kind="v1")))
        assert "LLVM" in refused(lambda: engine.Engine.of(dataclasses.replace(v2, backend="llvm")))
        with_backends = Input(engine.EngineSpec("v2", sys.executable), "", backends=("stencil", "cranelift"))
        assert "codegen-backend" in refused(lambda: engines_of(with_backends))
    assert [e.spec for _, e in engines_of(Input(engine.EngineSpec("v2", sys.executable), ""))] == [
        engine.EngineSpec("v2", sys.executable)]
    forced = engine.dev_backend_args
    assert forced("default", "build", ("-p", "x")) == ("-p", "x")
    assert forced("stencil", "check", ("-p", "x")) == ("-p", "x")
    assert forced("stencil", "build", ("-p", "x")) == (
        "-p", "x", "-Zcodegen-backend", "--config", 'profile.dev.codegen-backend="stencil"')
    assert forced("cranelift", "run", ("--bin", "a", "--", "--release")) == (
        "--bin", "a", "-Zcodegen-backend", "--config", 'profile.dev.codegen-backend="cranelift"', "--", "--release")
    assert forced("stencil", "test", ())[-1] == 'profile.test.codegen-backend="stencil"'
    for args, profile in ((("--release",), "release"), (("-r", "-p", "x"), "release"), (("--profile", "ci"), "ci"),
                          (("--profile=ci",), "ci"), (("--", "--profile=ci"), "test")):
        assert engine.profile_of("test", args) == profile, (args, engine.profile_of("test", args))


def test_panics() -> None:
    said = ("thread 'main' (12345) panicked at src/main.rs:2:5:\nboom\n"
            "note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace\n"
            "thread 'b' panicked at tests/t.rs:9:1:\nassertion failed: ok\n"
            "thread 'old' panicked at 'gone', src/lib.rs:1:1\n")
    assert panics(said) == ("b at tests/t.rs:9:1: assertion failed: ok", "main at src/main.rs:2:5: boom",
                            "old at 'gone', src/lib.rs:1:1"), panics(said)
    assert panics("thread 'main' panicked at src/main.rs:2:5:") == ("main at src/main.rs:2:5",)
    assert panics("all fine\n") == ()


def ran_with(returncode: int, stdout: str, stderr: str = "") -> engine.Ran:
    return engine.Ran(("blaze", "run"), 0.1, returncode, stdout, stderr)


def test_compare_backends() -> None:
    def program(returncode: int, stdout: str, stderr: str = "") -> Outcome:
        ran = ran_with(returncode, stdout, stderr)
        return Outcome(returncode, blaze_cli.program_output(ran), panics(stdout + "\n" + stderr))

    def tests(returncode: int, stdout: str) -> Outcome:
        return Outcome(returncode, blaze_cli.test_results(Tested(returncode, stdout, [])), panics(stdout))

    steps = (Step("build", "build"), Step("run", "run"), Step("test", "test"))
    panicked = "thread 'main' panicked at src/main.rs:2:5:\nboom\n"
    same = {"build": Outcome(0, (0,), ()), "run": program(101, "hi\n", panicked), "test": tests(101, LIBTEST)}
    found = compare_backends("p@1", steps, [("stencil", same), ("cranelift", dict(same))])
    assert [(f.name, f.verdict) for f in found] == [
        ("p@1: build: stencil against cranelift", "pass"), ("p@1: run: stencil against cranelift", "pass"),
        ("p@1: test: stencil against cranelift", "pass")], found
    # A panic's thread id is not the program's behavior.
    assert backend_difference("stencil", same["run"], "cranelift",
                              program(101, "hi\n", panicked.replace("'main'", "'main' (7)"))) is None
    other = {
        "build": Outcome(0, (0,), ()),
        "run": program(101, "hi\n", "thread 'main' panicked at src/main.rs:3:5:\nboom\n"),
        "test": tests(101, LIBTEST.replace("test a ... ok", "test a ... FAILED").replace("    b\n", "    a\n    b\n")),
    }
    found = {f.name: f for f in compare_backends("p@1", steps, [("stencil", same), ("cranelift", other)])}
    run = found["p@1: run: stencil against cranelift"]
    assert run.verdict == "fail" and "panics differ" in run.detail and "stencil" in run.detail \
        and "cranelift" in run.detail, run
    test = found["p@1: test: stencil against cranelift"]
    assert test.verdict == "fail" and "tests differ" in test.detail, test
    exits = backend_difference("stencil", same["run"], "cranelift", program(0, "hi\n"))
    assert exits == "stencil exits 101 and cranelift 0", exits
    printed = backend_difference("stencil", program(0, "a\nb\n"), "cranelift", program(0, "a\nc\n"))
    assert printed is not None and "('b', 'c')" in printed and "stencil" in printed, printed
    # A build that failed at one backend stops its run: the steps after it are not compared.
    stopped = {"build": Outcome(101, (101,), ())}
    found_list = compare_backends("p@1", steps, [("stencil", same), ("cranelift", stopped)])
    assert [(f.name, f.verdict) for f in found_list] == [("p@1: build: stencil against cranelift", "fail")], found_list
    assert compare_backends("p@1", steps, [("stencil", same)]) == []


def test_largest_first() -> None:
    def program(name: str) -> Program:
        return Program(name, name, lambda ws: None, ())

    given = [program(n) for n in ("crates.io/a@1", "nushell@0.1", "crates.io/b@2", "blaze@abc")]
    assert largest_first(given) == [1, 3, 0, 2], largest_first(given)


def test_overhead() -> None:
    before = os.times_result((1.0, 0.5, 10.0, 5.0, 0.0))
    after = os.times_result((1.5, 0.6, 70.0, 25.0, 100.0))
    got = overhead(before, after, 12.0)
    assert got == dict(wall_s=12.0, harness_cpu_s=0.6, children_cpu_s=80.0, harness_share=round(0.6 / 80.6, 5)), got
    assert overhead(before, before, 0.0)["harness_share"] == 0.0


SELFTESTS = (test_index_path, test_listed_cksum, test_verified, test_unpack, test_judge, test_test_results, test_corpus,
             test_pinned, test_backends_input, test_dev_backend, test_panics, test_compare_backends, test_overhead,
             test_largest_first)


def selftest() -> int:
    failed = 0
    for test in SELFTESTS:
        try:
            test()
            print(f"ok    {test.__name__}", flush=True)
        except (Exception, SystemExit) as e:
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
