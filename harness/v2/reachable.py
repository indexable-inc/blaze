"""reachable.py: dependencies built reachable-only against built whole, program for program
(docs/v2/plan/lanes/cold-v2/notes.md). A crate of the standard library, or of a package cargo caps
the lints of (registry, git), runs no check and compiles no roots of its own as a library unit:
the programs' codegen walks compile what they reach of it (`reached_only` in
crates/blaze-cargo/src/serve/unit/compile.rs). `BLAZE_DEPS=full` in a job's environment builds
every dependency whole instead. Leaving out code nothing reaches is a superset of rustc only when
the roots are complete, and this is the proof: the two builds' programs behave the same, their
tests end the same and they export the same symbols.

    uv run --frozen python harness/v2/reachable.py '{"engine": {"kind": "v2", "blaze": "target/release/blaze"}, "out": "/tmp/reachable.json", "projects": ["ripgrep"], "profiles": ["dev"]}'
    uv run --frozen python harness/v2/reachable.py '{"engine": {"kind": "v2", "blaze": "target/release/blaze"}, "out": "/tmp/reachable.json"}'
    uv run --frozen python harness/v2/reachable.py selftest

The input is one JSON object (inline, a file's path, or `-`):

    engine    the blaze measured (engine.EngineSpec)
    out       the result's path (report.py's JSON)
    work      where each project's source, its builds' trees, homes and stores go; a new
              directory under /tmp when empty
    projects  entries of harness/apps/apps.json (ripgrep and every local app under harness/apps
              with a Cargo.toml). The source is bench.PROJECTS' pinned clone where the name is
              there (budgets.checkout), else the entry's `repo` at its `rev`, else its `source`
              under harness/apps; how it is built, run and served is the entry's (`package`,
              `cargo_args`, `bin`, `env`, `build_env`, `setup`, `runs`, `scrub`, `serve`); the
              tests are bench.PROJECTS' `test` arguments where it has them, else the package's
    profiles  dev, release or both (both)
    checks    runs, tests, symbols or any of them (all three)
    keep      keep the work directory (false)

For each project and profile: two cold builds of the same source at the same path, each with an
empty home, store and target directory and its own daemon, the first with `BLAZE_DEPS=full` and
the second with it unset (and taken out of an inherited environment), each timed from nothing to
the end of `blaze build`. Then, with each build's tree and daemon in place:

runs     the entry's program (`target/<profile>/<bin>`) run with each command line of `runs` in
         the workspace (a list of arguments; `{"args", "stdin"}`, the file read on stdin;
         `{"tool"}`, a command that talks to the program served on a free port when the entry
         has `serve`; none listed is one run without arguments): exit status, stdout and stderr,
         with the program's path, its directory and the port named alike and `scrub`'s regexes
         taken out
symbols  every executable and shared library under `target/<profile>`, its `examples/` and
         `deps/` (a cargo hash in a name read as `-HASH`): the sorted names of its dynamic and its
         global defined symbols (`nm -D --defined-only`, `nm -g --defined-only` on Linux;
         `nm -gU` on macOS)
tests    `blaze test --no-fail-fast`: every test with its outcome, the totals and the exit
         status, as blaze_cli.test_results reads libtest's lines

One outcome per (project, profile, check), pass when the two builds give the same, fail naming
what differs (the first command lines, tests or symbols only in one), plus one per (project,
profile) `build` with both cold times and their ratio (reachable-only over full), failing when a
build failed or was not blaze's. The result's `extra.cold` lists the times. `selftest` checks the
input, the environments, the runner (with Python as the program and as its server), the test
and symbol comparisons and the listing of delivered files without blaze, cargo or the network.
"""

import contextlib
import json
import os
import re
import shutil
import socket
import subprocess
import sys
import tempfile
import time
from collections.abc import Callable, Iterator
from dataclasses import dataclass, field
from pathlib import Path
from types import SimpleNamespace
from typing import Any, Literal, cast, get_args

HERE = Path(__file__).resolve().parent
sys.path[:0] = [str(HERE), str(HERE.parent)]
from typed_input import fail, parse
import engine
import report
import budgets
import blaze_cli

SCRIPT = "reachable.py"
Profile = Literal["dev", "release"]
Check = Literal["runs", "tests", "symbols"]
type Mode = Literal["full", "reached"]
MODES: tuple[Mode, ...] = ("full", "reached")
MODE_NAMES: dict[Mode, str] = {"full": "BLAZE_DEPS=full", "reached": "reachable-only"}
# The variable crates/blaze-cargo/src/serve/unit/compile.rs (`DEPS`) reads from the job's environment.
DEPS = "BLAZE_DEPS"
SHOWN = 10
SYMBOLS_SHOWN = 5
BUILD_SECS = 4 * 3600
RUN_SECS = 600
SERVER_SECS = 60
APPS_JSON = engine.HARNESS / "apps" / "apps.json"
APPS: dict[str, dict[str, Any]] = json.loads(APPS_JSON.read_text())
LOCAL = tuple(name for name, spec in APPS.items()
              if "source" in spec and (engine.HARNESS / "apps" / spec["source"] / "Cargo.toml").is_file())
DEFAULT_PROJECTS = ("ripgrep", *(name for name in LOCAL if name != "ripgrep"))
SHARED = (".so", ".dylib", ".dll")
# The hash cargo puts in the name of a file of `deps/` (`rg-0123456789abcdef`, `libx-....so`).
HASHED = re.compile(r"-[0-9a-f]{16}(?=\.|$)")
TOTALS = ("executables not ok", "passed", "failed", "ignored", "measured")

test_results: Callable[[object], tuple[int, dict[tuple[str, str], int], tuple[int, ...]]] = getattr(
    blaze_cli, "test_results")
kept_passes: Callable[[str], list[str]] = getattr(blaze_cli, "kept_passes")


@dataclass(frozen=True)
class Input:
    engine: engine.EngineSpec
    out: str
    work: str = ""
    projects: tuple[str, ...] = DEFAULT_PROJECTS
    profiles: tuple[Profile, ...] = get_args(Profile)
    checks: tuple[Check, ...] = get_args(Check)
    keep: bool = False


@dataclass(frozen=True)
class Subject:
    """One project, set up once: its source under `source` (the tree at `tree`, and what the
    command lines read beside it, `beside`, linked beside each build's tree), its workspace in
    the tree (`sub`), the arguments of `blaze build` and `blaze test`, their environment, the
    program's name and environment, and its command lines, scrubs and server arguments."""

    label: str
    source: Path
    tree: str
    sub: str
    build: tuple[str, ...]
    test: tuple[str, ...]
    env: dict[str, str]
    run_env: dict[str, str]
    argv0: str
    runs: tuple[object, ...]
    scrub: tuple[str, ...]
    serve: tuple[str, ...] | None
    beside: tuple[str, ...]


type Behaviour = tuple[str, str, str]
type Symbols = dict[str, tuple[str, ...]]


@dataclass(frozen=True)
class Tests:
    """What `blaze test` said: its status, each test's outcome by name, libtest's totals."""

    status: int
    outcomes: dict[str, str]
    totals: tuple[int, ...]
    said: str


@dataclass
class Side:
    """What one build gave: its seconds, why it failed (None when it built), and each check's
    findings, or in `problems` why that check could not be made of it."""

    seconds: float = 0.0
    failed: str | None = None
    runs: list[Behaviour] = field(default_factory=list[Behaviour])
    tests: Tests | None = None
    symbols: dict[str, Symbols] = field(default_factory=dict[str, Symbols])
    problems: dict[str, str] = field(default_factory=dict[str, str])


class Problem(Exception):
    """A finding about one build that keeps a check from being made: no program, a server that
    did not start, `blaze test` not served by blaze."""


def profile_dir(profile: Profile) -> str:
    return "release" if profile == "release" else "debug"


def deps_env(env: dict[str, str], mode: Mode) -> dict[str, str]:
    """`env` with `BLAZE_DEPS=full` for the full build, and without any `BLAZE_DEPS` for the
    reachable-only one."""
    out = dict(env)
    out.pop(DEPS, None)
    if mode == "full":
        out[DEPS] = "full"
    return out


def free_port() -> int:
    with socket.socket() as probe:
        probe.bind(("127.0.0.1", 0))
        return cast(int, probe.getsockname()[1])


def clone(name: str, spec: dict[str, Any], into: Path) -> None:
    """apps.json's `repo` at its `rev`, with its submodules, into `into`."""
    into.mkdir(parents=True)
    env = engine.scrubbed()
    for argv in (["git", "init", "-q"], ["git", "fetch", "-q", "--depth", "1", spec["repo"], spec["rev"]],
                 ["git", "checkout", "-q", "FETCH_HEAD"],
                 ["git", "submodule", "update", "-q", "--init", "--recursive", "--depth", "1"]):
        engine.run(f"{name}: `{' '.join(argv[:2])}`", argv, into, env, 1800, check=True)


def vendored(runs: tuple[object, ...], ws: Path) -> bool:
    """Whether a command line reads the dependencies' source beside the workspace (apps.py's
    `cargo vendor` directory, `../<workspace>-vendor`)."""
    return any(f"{ws.name}-vendor" in json.dumps(run) for run in runs)


def subject(name: str, top: Path) -> Subject:
    """apps.json's `name` set up in `top/source`: its source, its `setup` run once, its
    dependencies fetched (and vendored beside it where a command line reads them), so that both
    builds read the same lock file and need no network."""
    if name not in APPS:
        raise engine.Failed(f"{name}: no entry in {APPS_JSON}")
    spec = APPS[name]
    if "prepare" in spec:
        raise engine.Failed(f"{name}: its tree is made by apps.py's `prepare` step ({spec['prepare']}), not here")
    source = top / "source"
    shutil.rmtree(source, ignore_errors=True)
    source.mkdir(parents=True)
    tree = source / name
    env: dict[str, str] = {}
    package_args = ("-p", spec["package"], *spec.get("cargo_args", []))
    test = package_args
    if name in budgets.PROJECTS:
        bench_spec, ws = budgets.checkout(name, tree)
        env.update(budgets.project_env(bench_spec))
        if "test" in bench_spec:
            test = tuple(cast(list[str], bench_spec["test"]))
    elif "repo" in spec:
        clone(name, spec, tree)
        ws = tree / spec.get("dir", "")
    else:
        local = engine.HARNESS / "apps" / spec["source"]
        if not (local / "Cargo.toml").is_file():
            raise engine.Failed(f"{name}: no Cargo.toml in {local}")
        shutil.copytree(local, tree, symlinks=True, ignore=budgets.left_out)
        ws = tree / spec.get("dir", "")
    env.update(cast(dict[str, str], spec.get("build_env") or {}))
    runs = tuple(cast(list[object], spec.get("runs") or [[]]))
    cargo_env = engine.reference_env(top / "fetch-target", env)
    engine.cargo(f"{name}: cargo fetch", ["fetch"], ws, cargo_env).checked(f"{name}: cargo fetch")
    if vendored(runs, ws):
        vendor = ws.parent / f"{ws.name}-vendor"
        engine.cargo(f"{name}: cargo vendor", ["vendor", "--locked", "--versioned-dirs", str(vendor)], ws,
                     cargo_env).checked(f"{name}: cargo vendor")
    for line in cast(list[str], spec.get("setup") or []):
        engine.run(f"{name}: setup", ["bash", "-c", line], ws, engine.scrubbed(), RUN_SECS, check=True)
    shutil.rmtree(top / "fetch-target", ignore_errors=True)
    serve = spec.get("serve")
    return Subject(
        label=name, source=source, tree=name, sub=str(ws.relative_to(tree)) if ws != tree else "",
        build=package_args, test=test, env=env, run_env=cast(dict[str, str], spec.get("env") or {}),
        argv0=spec.get("bin", spec["package"]), runs=runs, scrub=tuple(cast(list[str], spec.get("scrub") or [])),
        serve=tuple(cast(list[str], serve)) if serve is not None else None,
        beside=tuple(sorted(entry.name for entry in source.iterdir() if entry.name != name)))


# The runs.

def filled(argv: list[str], port: int) -> list[str]:
    return [arg.replace("{port}", str(port)) for arg in argv]


def cleaned(data: bytes, program: Path, port: int, scrub: tuple[str, ...]) -> str:
    """What a program said, with what differs from one build or run to the next named alike:
    its path, its directory, the port it was served on and the entry's `scrub` patterns."""
    text = data.decode(errors="replace").replace(str(program), "<program>").replace(str(program.parent), "<directory>")
    if port:
        text = text.replace(str(port), "<port>")
    for pattern in scrub:
        text = re.sub(pattern, "<scrubbed>", text)
    return text


def run_name(run: object) -> str:
    if isinstance(run, dict):
        given = cast(dict[str, Any], run)
        if "tool" in given:
            return " ".join(given["tool"])
        return f"{' '.join(given['args'])} < {given['stdin']}"
    return " ".join(cast(list[str], run)) or "(no arguments)"


def behaviour(s: Subject, run: object, program: Path, ws: Path, env: dict[str, str], port: int) -> Behaviour:
    """One command line of `runs`: the program's, or a tool's talking to it."""
    executable: Path | None = program
    stdin_path: Path | None = None
    if isinstance(run, dict):
        given = cast(dict[str, Any], run)
        if "tool" in given:
            argv, executable = filled(given["tool"], port), None
        else:
            argv, stdin_path = [s.argv0, *filled(given["args"], port)], ws / given["stdin"]
    else:
        argv = [s.argv0, *filled(cast(list[str], run), port)]
    try:
        with open(stdin_path, "rb") if stdin_path is not None else contextlib.nullcontext(subprocess.DEVNULL) as stdin:
            done = subprocess.run(argv, executable=executable, cwd=ws, env=env, stdin=stdin, capture_output=True,
                                  timeout=RUN_SECS)
    except subprocess.TimeoutExpired:
        return (f"no end in {RUN_SECS} s", "", "")
    except OSError as e:
        raise engine.Failed(f"{s.label}: `{run_name(run)}` did not start: {e}") from None
    return (str(done.returncode), *(cleaned(out, program, port, s.scrub) for out in (done.stdout, done.stderr)))


@contextlib.contextmanager
def served(s: Subject, program: Path, ws: Path, env: dict[str, str], port: int, log: Path) -> Iterator[None]:
    """The program served on `port` for as long as the block runs, when the entry has `serve`."""
    if s.serve is None:
        yield
        return
    with open(log, "wb") as err:
        child = subprocess.Popen([s.argv0, *filled(list(s.serve), port)], executable=program, cwd=ws, env=env,
                                 stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=err)
    try:
        deadline = time.monotonic() + SERVER_SECS
        while True:
            if child.poll() is not None:
                raise Problem(f"the server exited {child.returncode} before it listened on {port}:\n"
                              f"{log.read_bytes().decode(errors='replace')[-2000:]}")
            try:
                socket.create_connection(("127.0.0.1", port), timeout=1).close()
                break
            except OSError:
                if time.monotonic() > deadline:
                    raise Problem(f"the server did not listen on {port} in {SERVER_SECS} s:\n"
                                  f"{log.read_bytes().decode(errors='replace')[-2000:]}") from None
                time.sleep(0.05)
        yield
    finally:
        child.kill()
        child.wait()


def behaviours(s: Subject, program: Path, ws: Path, log: Path) -> list[Behaviour]:
    """Every command line of `runs` in order (a server keeps what the earlier ones did to it)."""
    if not program.is_file():
        raise Problem(f"no program {program} after the build")
    env = engine.scrubbed()
    env.pop(DEPS, None)
    env.update(s.run_env)
    port = free_port() if s.serve is not None else 0
    with served(s, program, ws, env, port, log):
        return [behaviour(s, run, program, ws, env, port) for run in s.runs]


def first_line_difference(a: str, b: str) -> str:
    left, right = a.splitlines(), b.splitlines()
    for at, (x, y) in enumerate(zip(left, right)):
        if x != y:
            return f"line {at + 1}: {x[:200]!r} against {y[:200]!r}"
    return f"{len(left)} lines against {len(right)}"


def compare_runs(runs: tuple[object, ...], a: list[Behaviour], b: list[Behaviour]) -> str | None:
    if a == b:
        return None
    lines: list[str] = []
    for run, x, y in zip(runs, a, b):
        if x == y:
            continue
        how = [f"status {x[0]} against {y[0]}"] if x[0] != y[0] else []
        how += [f"{stream}: {first_line_difference(p, q)}" for stream, p, q in (("stdout", x[1], y[1]),
                                                                                ("stderr", x[2], y[2])) if p != q]
        lines.append(f"`{run_name(run)}`: {'; '.join(how)}")
    if len(a) != len(b):
        lines.append(f"{len(a)} command lines ran against {len(b)}")
    more = f"\n... and {len(lines) - SHOWN} more" if len(lines) > SHOWN else ""
    return f"{len(lines)} of {len(runs)} command lines differ:\n" + "\n".join(lines[:SHOWN]) + more


# The tests.

def outcomes(tests: dict[tuple[str, str], int]) -> dict[str, str]:
    """Each test's outcome by its name, from blaze_cli.test_results' counts of (name, "ran" or
    "FAILED"): "FAILED" when libtest listed it as failed, else "ran", with how many times when
    more than once."""
    by_name: dict[str, list[str]] = {}
    for (name, what), n in sorted(tests.items()):
        by_name.setdefault(name, []).append(what if n == 1 else f"{what} x{n}")
    return {name: ", ".join(whats) for name, whats in by_name.items()}


def tested(eng: engine.Engine, step: str, s: Subject, ws: Path, env: dict[str, str], release: list[str]) -> Tests:
    kept_file = ws / "target" / "blaze" / "tests.ndjson"
    kept_file.unlink(missing_ok=True)
    ran = eng.build(f"{step}: test", ws, env, ["--message-format=json", "--no-fail-fast", *s.test, *release],
                    command="test", timeout=BUILD_SECS)
    if (who := eng.not_blazes(ran)) is not None:
        raise Problem(f"`blaze test` was served by {who}, not blaze")
    status, tests, totals = test_results(SimpleNamespace(returncode=ran.returncode, stdout=ran.stdout,
                                                         kept=kept_passes(str(kept_file))))
    return Tests(status, outcomes(tests), tuple(totals), ran.said())


def compare_tests(a: Tests, b: Tests) -> str | None:
    if (a.status, a.outcomes, a.totals) == (b.status, b.outcomes, b.totals):
        return None
    lines: list[str] = []
    if a.status != b.status:
        lines.append(f"`blaze test` exited {a.status} against {b.status}")
    if a.totals != b.totals:
        lines.append("totals: " + ", ".join(f"{what} {x} against {y}" for what, x, y in zip(TOTALS, a.totals, b.totals)
                                            if x != y))
    differing = [name for name in sorted(a.outcomes.keys() | b.outcomes.keys())
                 if a.outcomes.get(name) != b.outcomes.get(name)]
    for name in differing[:SHOWN]:
        lines.append(f"{name}: {a.outcomes.get(name, 'not run')} against {b.outcomes.get(name, 'not run')}")
    if len(differing) > SHOWN:
        lines.append(f"... and {len(differing) - SHOWN} more tests")
    for mode, side in zip(MODES, (a, b)):
        if side.status != 0 and a.status != b.status:
            lines.append(f"{MODE_NAMES[mode]} said:\n{side.said[-1500:]}")
    return "\n".join(lines)


# The symbols.

def delivered(top: Path) -> dict[str, Path]:
    """Every executable and shared library of `top` (target/<profile>), its `examples/` and its
    `deps/`, by its path under `top` with a cargo hash read as `-HASH`; where two names read the
    same, the later one keeps its own."""
    found: dict[str, Path] = {}
    for sub, prefix in ((top, ""), (top / "examples", "examples/"), (top / "deps", "deps/")):
        try:
            with os.scandir(sub) as listed:
                entries = sorted(listed, key=lambda e: e.name)
        except (FileNotFoundError, NotADirectoryError):
            continue
        for entry in entries:
            if not entry.is_file(follow_symlinks=False):
                continue
            suffix = Path(entry.name).suffix
            if not (suffix in SHARED or suffix == ".exe"
                    or (not suffix and entry.stat(follow_symlinks=False).st_mode & 0o111)):
                continue
            key = prefix + HASHED.sub("-HASH", entry.name)
            found[prefix + entry.name if key in found else key] = Path(entry.path)
    return found


def nm_tables() -> tuple[tuple[str, tuple[str, ...]], ...]:
    """Each symbol table compared, with the `nm` that lists its defined names in POSIX form."""
    if sys.platform == "linux":
        return (("dynamic", ("nm", "-D", "--defined-only", "-P")), ("global", ("nm", "-g", "--defined-only", "-P")))
    if sys.platform == "darwin":
        return (("global", ("nm", "-gU", "-P")),)
    raise engine.Failed(f"exported symbols: no nm for {sys.platform}")


def nm_names(text: str) -> tuple[str, ...]:
    """The sorted names of `nm -P`'s lines (the name is the first field), without the header of
    a file or an architecture."""
    names = {line.split(" ", 1)[0] for line in text.splitlines()
             if line.strip() and not (" " not in line and line.endswith(":"))}
    return tuple(sorted(names))


def symbols_of(path: Path) -> Symbols:
    out: Symbols = {}
    for table, argv in nm_tables():
        ran = engine.run(f"`{' '.join(argv)} {path}`", [*argv, str(path)], path.parent, engine.scrubbed(), RUN_SECS)
        if ran.returncode != 0 and not (not ran.stdout.strip() and "no symbols" in ran.stderr):
            raise engine.Failed(f"`{' '.join(argv)} {path}` exited {ran.returncode}:\n{ran.said()}")
        out[table] = nm_names(ran.stdout)
    return out


def exported(top: Path) -> dict[str, Symbols]:
    files = delivered(top)
    if not files:
        raise Problem(f"no executable or shared library under {top}")
    return {name: symbols_of(path) for name, path in files.items()}


def compare_symbols(a: dict[str, Symbols], b: dict[str, Symbols]) -> str | None:
    lines: list[str] = []
    for name in sorted(a.keys() | b.keys()):
        if name not in b:
            lines.append(f"{name}: only in the {MODE_NAMES['full']} build")
            continue
        if name not in a:
            lines.append(f"{name}: only in the {MODE_NAMES['reached']} build")
            continue
        for table in sorted(a[name].keys() | b[name].keys()):
            x, y = set(a[name].get(table, ())), set(b[name].get(table, ()))
            if x == y:
                continue
            how = [f"{len(names)} only in the {MODE_NAMES[mode]} build, first {', '.join(sorted(names)[:SYMBOLS_SHOWN])}"
                   for mode, names in (("full", x - y), ("reached", y - x)) if names]
            lines.append(f"{name} ({table}): {'; '.join(how)}")
    if not lines:
        return None
    more = f"\n... and {len(lines) - SHOWN} more" if len(lines) > SHOWN else ""
    return f"{len(lines)} differences:\n" + "\n".join(lines[:SHOWN]) + more


# The builds.

def attempt(side: Side, check: Check, checks: tuple[Check, ...], fn: Callable[[], None]) -> None:
    if check not in checks:
        return
    try:
        fn()
    except Problem as e:
        side.problems[check] = str(e)


def side(eng: engine.Engine, s: Subject, top: Path, profile: Profile, mode: Mode, checks: tuple[Check, ...]) -> Side:
    """A cold build of `s` in `mode` at `top/build/<tree>`, with an empty home, store and target
    directory and a daemon of its own, timed; then each check made of it with its tree and daemon
    in place, and the daemon stopped."""
    builds = top / "build"
    tree, home, store = builds / s.tree, top / "home", top / "store"
    for path in (tree, home, store):
        shutil.rmtree(path, ignore_errors=True)
    builds.mkdir(parents=True, exist_ok=True)
    for entry in s.beside:
        if not os.path.lexists(builds / entry):
            (builds / entry).symlink_to(s.source / entry)
    shutil.copytree(s.source / s.tree, tree, symlinks=True, ignore=budgets.left_out)
    ws = tree / s.sub if s.sub else tree
    env = deps_env(eng.env(home, store, None, s.env), mode)
    release = ["--release"] if profile == "release" else []
    step = f"{s.label} {profile} ({MODE_NAMES[mode]})"
    got = Side()
    try:
        began = time.monotonic()
        ran = eng.build(f"{step}: build", ws, env, [*s.build, *release], timeout=BUILD_SECS)
        got.seconds = time.monotonic() - began
        if ran.returncode != 0:
            got.failed = f"`blaze build` exited {ran.returncode}:\n{ran.said()}"
            return got
        if (who := eng.not_blazes(ran)) is not None:
            got.failed = f"built by {who}, not blaze"
            return got
        target = ws / "target" / profile_dir(profile)

        def symbols() -> None:
            got.symbols = exported(target)

        def runs() -> None:
            got.runs = behaviours(s, target / f"{s.argv0}{engine.EXE}", ws, top / f"server-{mode}.stderr")

        def tests() -> None:
            got.tests = tested(eng, step, s, ws, env, release)

        attempt(got, "symbols", checks, symbols)
        attempt(got, "runs", checks, runs)
        attempt(got, "tests", checks, tests)
        return got
    finally:
        eng.stop(ws, env)


def compare(eng: engine.Engine, rep: report.Report, s: Subject, top: Path, profile: Profile,
            checks: tuple[Check, ...], cold: list[dict[str, report.Number]]) -> None:
    name = f"{s.label} {profile}"
    full = side(eng, s, top, profile, "full", checks)
    reached = side(eng, s, top, profile, "reached", checks)
    numbers: dict[str, report.Number] = dict(
        seconds_full=round(full.seconds, 1), seconds_reached=round(reached.seconds, 1),
        ratio=round(reached.seconds / full.seconds, 3) if full.seconds > 0 else None)
    sides: dict[Mode, Side] = {"full": full, "reached": reached}
    broken = [f"{MODE_NAMES[mode]}: {sides[mode].failed}" for mode in MODES if sides[mode].failed is not None]
    if broken:
        rep.add(f"{name} build", "fail", "\n".join(broken), **numbers)
        for check in checks:
            rep.add(f"{name} {check}", "error", f"not compared: a build failed (`{name} build`)")
        return
    cold.append(dict(project=s.label, profile=profile, **numbers))
    rep.add(f"{name} build", "pass", f"cold {full.seconds:.1f} s with {MODE_NAMES['full']}, "
            f"{reached.seconds:.1f} s {MODE_NAMES['reached']}", **numbers)
    for check in checks:
        problems = [f"{MODE_NAMES[mode]}: {sides[mode].problems[check]}" for mode in MODES
                    if check in sides[mode].problems]
        if problems:
            rep.add(f"{name} {check}", "fail", "\n".join(problems))
            continue
        if check == "runs":
            found, counts = compare_runs(s.runs, full.runs, reached.runs), dict(runs=len(s.runs))
        elif check == "tests":
            a, b = cast(Tests, full.tests), cast(Tests, reached.tests)
            found, counts = compare_tests(a, b), dict(tests=len(a.outcomes), status=a.status)
        else:
            found, counts = compare_symbols(full.symbols, reached.symbols), dict(files=len(full.symbols))
        if found is None:
            rep.add(f"{name} {check}", "pass", f"the same with {MODE_NAMES['full']} and {MODE_NAMES['reached']}",
                    **counts)
        else:
            rep.add(f"{name} {check}", "fail", found, **counts)


def main(args: Input) -> None:
    unknown = [name for name in args.projects if name not in APPS]
    if unknown:
        fail(SCRIPT, f"projects: no {', '.join(unknown)} in {APPS_JSON} (they are {', '.join(APPS)})")
    if not args.projects or not args.profiles or not args.checks:
        fail(SCRIPT, "projects, profiles and checks each name one at least")
    checks = tuple(dict.fromkeys(args.checks))
    try:
        eng = engine.Engine.of(args.engine)
        if "symbols" in checks:
            nm_tables()
    except engine.Failed as e:
        fail(SCRIPT, str(e))
    rep = report.Report(SCRIPT, args.engine, args.out)
    work = Path(args.work or tempfile.mkdtemp(prefix="bz-reachable-", dir="/tmp")).resolve()
    work.mkdir(parents=True, exist_ok=True)
    print(f"work: {work}", flush=True)
    cold: list[dict[str, report.Number]] = []

    def set_up(project: str, top: Path) -> Subject | None:
        return rep.guarded(f"{project}: setting up", lambda: budgets.failing(project, lambda: subject(project, top)))

    def each(s: Subject, top: Path, profile: Profile) -> None:
        label = f"{s.label} {profile}"
        rep.guarded(label, lambda: budgets.failing(label, lambda: compare(eng, rep, s, top, profile, checks, cold)))

    for project in dict.fromkeys(args.projects):
        top = work / project
        shutil.rmtree(top, ignore_errors=True)
        top.mkdir(parents=True)
        s = set_up(project, top)
        if s is not None:
            for profile in dict.fromkeys(args.profiles):
                each(s, top, profile)
        if not args.keep:
            shutil.rmtree(top, ignore_errors=True)
    rep.extra.update(work=str(work), cold=cold, profiles=list(args.profiles), checks=list(checks))
    rep.finish()


def expect(holds: bool, what: str) -> None:
    if not holds:
        raise SystemExit(f"{SCRIPT} selftest: {what}")


def selftest() -> None:
    spec = '{"engine": {"kind": "v2", "blaze": "blaze"}, "out": "/tmp/x.json"'
    given = parse(Input, spec + "}", SCRIPT)
    expect(given.projects[0] == "ripgrep" and set(LOCAL) <= set(given.projects), f"default projects: {given.projects}")
    expect(given.profiles == ("dev", "release") and given.checks == ("runs", "tests", "symbols"), "default axes")
    expect(all(name in APPS for name in given.projects), "every default project is in apps.json")
    expect(len(LOCAL) > 0 and all((engine.HARNESS / "apps" / APPS[n]["source"] / "Cargo.toml").is_file() for n in LOCAL),
           f"the local apps have manifests: {LOCAL}")
    for bad in (', "speed": 1}', ', "checks": ["bytes"]}'):
        try:
            parse(Input, spec + bad, SCRIPT)
            expect(False, f"{bad} is refused")
        except SystemExit as e:
            expect(str(e).startswith(SCRIPT), f"{bad}: {e}")
    inherited = {"PATH": "/bin", DEPS: "full"}
    expect(deps_env(inherited, "full")[DEPS] == "full", "the full build has BLAZE_DEPS=full")
    expect(DEPS not in deps_env(inherited, "reached"), "the reachable-only build has no BLAZE_DEPS, inherited or not")
    expect(inherited[DEPS] == "full", "the environment given is not changed")
    expect(profile_dir("dev") == "debug" and profile_dir("release") == "release", "profile directories")
    expect(vendored(({"tool": ["x", "../ripgrep-vendor"]},), Path("/w/ripgrep")) and not vendored(([],), Path("/w/rg")),
           "a command line reading the vendored source is seen")

    program = Path(sys.executable)
    with tempfile.TemporaryDirectory() as tmp:
        ws = Path(tmp)
        (ws / "in.txt").write_text("from stdin\n")
        printing = "import sys; print(sys.argv[1:]); print('took 1.5s'); sys.stderr.write('err')"
        runs: tuple[object, ...] = (["-c", printing, "a"], {"args": ["-c", "import sys; print(sys.stdin.read())"],
                                                             "stdin": "in.txt"}, ["-c", "import sys; sys.exit(3)"])
        s = Subject("selftest", ws, "t", "", (), (), {}, {"SELFTEST": "1"}, "python3", runs, (r"[0-9.]+s\b",), None, ())
        seen = behaviours(s, program, ws, ws / "log")
        expect(seen[0] == ("0", "['a']\ntook <scrubbed>\n", "err"), f"a run, scrubbed: {seen[0]}")
        expect(cleaned(f"{program} in {program.parent}/x".encode(), program, 0, ()) == "<program> in <directory>/x",
               "the program's path and directory are named alike")
        expect(seen[1] == ("0", "from stdin\n\n", ""), f"a run reading stdin: {seen[1]}")
        expect(seen[2][0] == "3", f"an exit status: {seen[2]}")
        expect(compare_runs(runs, seen, seen) is None, "the same runs have no difference")
        other = [seen[0], ("0", "from stdin\nmore\n", ""), ("1", "", "")]
        found = compare_runs(runs, seen, other) or ""
        expect(found.startswith("2 of 3 command lines differ") and "status 3 against 1" in found
               and "stdout: line 2: '' against 'more'" in found, f"runs that differ are named: {found}")
        fetch = ("import sys, urllib.request; "
                 "print(urllib.request.urlopen(f'http://127.0.0.1:{sys.argv[1]}/in.txt').read().decode(), sys.argv[1])")
        server = Subject("served", ws, "t", "", (), (), {}, {}, "python3", ({"tool": [sys.executable, "-c", fetch, "{port}"]},),
                         (), ("-m", "http.server", "--bind", "127.0.0.1", "{port}"), ())
        got = behaviours(server, program, ws, ws / "server.log")
        expect(got == [("0", "from stdin\n <port>\n", "")], f"a tool talking to the served program: {got}")
        try:
            behaviours(s, ws / "missing", ws, ws / "log")
            expect(False, "a missing program is a problem")
        except Problem:
            pass

        top = ws / "target" / "debug"
        for at in (top, top / "deps", top / "examples"):
            at.mkdir(parents=True)
        for path, mode in ((top / "rg", 0o755), (top / "rg.d", 0o644), (top / "librg.rlib", 0o644),
                           (top / "deps" / "rg-0123456789abcdef", 0o755), (top / "deps" / "libx-0123456789abcdef.so", 0o644),
                           (top / "deps" / "libx-fedcba9876543210.so", 0o644), (top / "examples" / "demo", 0o755),
                           (top / ".cargo-lock", 0o644)):
            path.write_bytes(b"")
            path.chmod(mode)
        listed = delivered(top)
        expect(sorted(listed) == ["deps/libx-HASH.so", "deps/libx-fedcba9876543210.so", "deps/rg-HASH", "examples/demo", "rg"],
               f"executables and shared libraries, hashes read alike: {sorted(listed)}")
    expect(nm_names("b T 10 4\na D 20 8\n\nlib.so:\nb T 30 4\n") == ("a", "b"), "nm -P's names, sorted, headers left out")

    expect(outcomes({("t::a", "ran"): 1, ("t::b", "ran"): 1, ("t::b", "FAILED"): 1, ("t::c", "ran"): 2})
           == {"t::a": "ran", "t::b": "FAILED, ran", "t::c": "ran x2"}, "test outcomes by name")
    a = Tests(0, {"t::a": "ran", "t::b": "ran"}, (0, 2, 0, 0, 0), "")
    b = Tests(101, {"t::a": "ran", "t::b": "FAILED, ran"}, (1, 1, 1, 0, 0), "test t::b ... FAILED")
    expect(compare_tests(a, a) is None, "the same tests have no difference")
    found = compare_tests(a, b) or ""
    expect("exited 0 against 101" in found and "t::b: ran against FAILED, ran" in found and "passed 2 against 1" in found
           and "test t::b ... FAILED" in found, f"tests that differ are named: {found}")

    one: dict[str, Symbols] = {"rg": {"dynamic": ("main",), "global": ("_start", "main", "x_only_full")},
                               "deps/rg-HASH": {"global": ("main",)}}
    two: dict[str, Symbols] = {"rg": {"dynamic": ("main",), "global": ("_start", "main", "y_only_reached")}}
    expect(compare_symbols(one, one) is None, "the same symbols have no difference")
    found = compare_symbols(one, two) or ""
    expect("deps/rg-HASH: only in the BLAZE_DEPS=full build" in found
           and "rg (global): 1 only in the BLAZE_DEPS=full build, first x_only_full; "
               "1 only in the reachable-only build, first y_only_reached" in found, f"symbols that differ: {found}")
    print(f"{SCRIPT} selftest: ok")


if __name__ == "__main__":
    if len(sys.argv) != 2:
        sys.exit(f"usage: {SCRIPT} '<json>' | selftest\n\n{__doc__}")
    if sys.argv[1] == "selftest":
        selftest()
    else:
        main(parse(Input, sys.argv[1], SCRIPT))
