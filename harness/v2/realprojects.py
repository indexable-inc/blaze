"""realprojects.py: blaze's correctness verdict on real code. Big real Rust workspaces, each pinned
in corpus.json (pins.py), are built and tested by blaze, and each project's own tests are the
judge (AGENTS.md rule verify-on-real-projects). No rustc and no cargo run anywhere: fetching is
git, building and testing is blaze, and nothing is compared with another compiler's output. A
failure is a blaze bug.

    uv run --frozen python harness/v2/realprojects.py run '{"out": "/tmp/rp"}'
    uv run --frozen python harness/v2/realprojects.py run '{"out": "/tmp/rp", "projects": ["ripgrep"], "baseline": "/tmp/rp-last/result.json"}'
    uv run --frozen python harness/v2/realprojects.py run '{"out": "/tmp/rp", "where": "ix", "projects": ["bevy", "polars"]}'
    uv run --frozen python harness/v2/realprojects.py merge '{"results": ["a/result.json", "b/result.json"], "out": "/tmp/rp"}'
    uv run --frozen python harness/v2/realprojects.py selftest

The input is one JSON object (inline, a file's path, or `-` for stdin), `Run` or `Merge` below.

`run`, for each workspace (all of corpus.json's, or `projects`), `jobs` at a time, the longest
first, each with a daemon home of its own (`home`):

    fetch   the commit, by git, from GitHub into `sources` (kept between runs), refused unless
            HEAD is `rev` and its tree is `tree`; with `submodules`, those at the commits the
            tree pins. The workspace `blaze` is this repository's `commit`, by `git archive`.
            Then a copy of it in the run's work directory.
    lock    the project's own Cargo.lock (`committed`) or the harness's (`harness`,
            harness/v2/locks/<name>.Cargo.lock); either is built `--locked`. A harness lock
            that is missing stops the project, unless `write_locks`, which has blaze resolve
            one and writes it there to be committed.
    sandbox with the workspace's `sandbox_tests`: `sandboxed-tests` added to `experimental`
            in `[workspace.metadata.blaze]` of the copy's root Cargo.toml (merged into a table
            that is there; refused, naming the file, where it cannot be merged), so every test
            runs in blaze's sandbox. The fetched source in `sources` is never edited.
    tree    the BLAKE3 of the copy as built (every path, its executable bit and its bytes;
            `.git` left out; after the `sandbox` edit), the lock's sha256, and the blaze
            binary's BLAKE3: the exact tree the result is of (rule measure-the-exact-tree).
            With `sandbox_tests`, the BLAKE3 of the copy before the edit too.
    build   `blaze build --workspace --all-targets --message-format=json`: the units built
            (`compiler-artifact`), and every unit that failed with blaze's error
            (`compiler-message` at level error, and plain `error:` lines).
    test    `blaze test --workspace --no-fail-fast --message-format=json`: each test of each
            test executable and doc test passed, failed or ignored (libtest's lines), with the
            panic of each failure; each executable that crashed or never built (blaze's
            `blaze-unchecked` line) is a failure too. result.json and report.md say per project
            whether its tests ran sandboxed.

Each step is bounded (`minutes` of the workspace for the build and for the tests, fixed bounds
for the rest), and a step the harness could not finish names the step, the command, its exit
status and the end of what it printed. Failures are grouped by blaze's error with numbers,
paths and hashes taken out, so one bug that breaks 300 tests is one line.

Out, in `out`: `result.json` (`Result`), `report.md` (a table per project: units built and
failed, tests passed, failed and ignored, the top failure clusters, and what changed against
the baseline), and `logs/<project>/<step>.log`. With `baseline` (an earlier `result.json`),
each project at the same commit and lock says which tests and units newly broke and which got
fixed; `write_baseline` copies this run's result there for the next run. The exit status is 1
when the harness could not finish a project, or something newly broke against the baseline,
or with `strict` anything failed; else 0.

`where: "ix"` runs each project in an ix VM of its own (docs/contributing.md, "Working in
parallel"): this repository's `commit` is shipped as a bundle, the release of blaze `vm.blaze`
names (empty: .github/blaze-version) is installed by site/install.sh, the project runs there
with `where: "local"`, its out directory comes back under `vm/<project>/`, and the VM is
removed. `merge` puts several results into one report (CI's per-project jobs, the VMs).
`selftest` runs the checks that need neither blaze nor the network.
"""

import asyncio
import dataclasses
import hashlib
import json
import os
import platform
import re
import shutil
import sys
import tarfile
import tempfile
import time
import tomllib
from collections import Counter
from collections.abc import Awaitable, Callable, Mapping, Sequence
from dataclasses import dataclass, field
from pathlib import Path
from typing import Literal, cast

HERE = Path(__file__).resolve().parent
ROOT = HERE.parent.parent
sys.path[:0] = [str(HERE), str(HERE.parent)]
import blake3
import pins
import storelib
from pins import Workspace
from typed_input import fail, parse

SCRIPT = "realprojects.py"
SCHEMA = 1
SOURCES = Path.home() / ".cache" / "blaze" / "realprojects" / "sources"
FETCH_MINUTES = 30
SMALL_MINUTES = 10
# How much of what a failed command printed its error carries.
TAIL = 3000
# A line of `--message-format=json` can be long (a unit's every file, a rendered diagnostic).
LINE_LIMIT = 64 << 20
# How long the output of a command that exited is still read, for what is in the pipe.
DRAIN_SECS = 30
# What the caller's environment may carry that would change what a blaze command builds or how
# it prints, none of which a real project's build is given.
SCRUBBED = (
    "RUSTFLAGS", "RUSTDOCFLAGS", "CARGO_ENCODED_RUSTFLAGS", "CARGO_TARGET_DIR", "CARGO_BUILD_TARGET",
    "CARGO_BUILD_TARGET_DIR", "CARGO_INCREMENTAL", "RUSTC", "RUSTC_WRAPPER", "RUSTC_WORKSPACE_WRAPPER",
    "RUSTDOC", "RUSTUP_TOOLCHAIN", "BLAZE_BACKEND", "BLAZE_OPT", "BLAZE_EMIT", "BLAZE_HOME", "VIRTUAL_ENV",
)


# ---------------------------------------------------------------------------------------------
# The input


@dataclass(frozen=True)
class Vm:
    """The ix VM each project runs in under `where: "ix"`: `ix new`'s size, and the release of
    blaze install.sh installs there (a Linux blaze; empty: .github/blaze-version). `nix` are the
    nixpkgs packages the run has (`nix shell`)."""

    cpus: int = 32
    memory: int = 128
    arch: Literal["x86_64", "arm64"] = "x86_64"
    ttl: str = "12h"
    blaze: str = ""
    nix: tuple[str, ...] = ("uv",)


@dataclass(frozen=True)
class Run:
    """`run`'s input.

    out             the directory result.json, report.md and logs/ are written to
    blaze           the blaze under test: a path, or a name on PATH (`where: "local"`)
    projects        which of corpus.json's workspaces, by name; empty: all of them
    jobs            how many projects at once
    work            where the copies, homes and build directories go; empty: a new one in /tmp
    sources         where fetched checkouts are kept between runs; empty: SOURCES
    commit          the commit of this repository the workspace `blaze` is
    commit_label    what that commit's build script is told it is (empty: the commit); the VM's
                    copy of a commit is another commit of the same tree
    home            a daemon home per project, or one for the whole run (either reads the
                    machine's toolchain, ~/.blaze/toolchains, whatever BLAZE_HOME says)
    baseline        an earlier result.json to say against what newly broke and got fixed
    write_baseline  where this run's result.json is copied for the next run; empty: nowhere
    write_locks     have blaze resolve a missing harness lock and write it to harness/v2/locks
    keep            keep the work directory
    strict          exit 1 when anything failed, not only when something newly broke
    where           this machine, or an ix VM per project
    vm              the VMs, under `where: "ix"`
    """

    out: str
    blaze: str = "blaze"
    projects: tuple[str, ...] = ()
    jobs: int = 2
    work: str = ""
    sources: str = ""
    commit: str = "HEAD"
    commit_label: str = ""
    home: Literal["per-project", "shared"] = "per-project"
    baseline: str = ""
    write_baseline: str = ""
    write_locks: bool = False
    keep: bool = False
    strict: bool = False
    where: Literal["local", "ix"] = "local"
    vm: Vm = Vm()


@dataclass(frozen=True)
class Merge:
    """`merge`'s input: the result.json of each run, the directory the merged result and report
    are written to, the projects that were to run (each one no result has is an error), and the
    baseline as in `Run`."""

    results: tuple[str, ...]
    out: str
    expected: tuple[str, ...] = ()
    baseline: str = ""
    write_baseline: str = ""
    strict: bool = False


# ---------------------------------------------------------------------------------------------
# The result (result.json), read back by `merge` and as a baseline with typed_input.

type Outcome = Literal["passed", "failed", "ignored"]
type StepName = Literal["fetch", "lock", "sandbox", "tree", "build", "test", "write lock", "stop", "vm"]


@dataclass(frozen=True)
class TestOutcome:
    """One test: the executable it is in (`<crate> (<target>)`, as blaze's `Running` line names
    it, without the hash), libtest's name of it, and how it ended."""

    exe: str
    name: str
    outcome: Outcome


@dataclass(frozen=True)
class Failure:
    """One thing that failed: a unit, a test (`exe :: name`) or a test executable; the cluster
    it falls in (blaze's error with the numbers, paths and hashes taken out); the first lines
    of what was said."""

    where: str
    cluster: str
    detail: str


@dataclass(frozen=True)
class StepResult:
    """A step: its command, its exit status (None: it did not end, or never started), its wall
    time, its log (relative to result.json's directory, "" if none) and, when the harness could
    not finish it, why."""

    step: StepName
    argv: tuple[str, ...]
    exit: int | None
    wall_s: float
    log: str
    error: str


@dataclass(frozen=True)
class Tree:
    """The exact tree a project's result is of: `digest` is the copy as built, after every edit
    the harness made to it (the lock, the `sandbox` step); `digest_before_sandbox` the copy
    before the `sandbox` step edited its Cargo.toml ("" when it did not)."""

    rev: str
    tree: str
    digest: str
    lock_sha256: str
    digest_before_sandbox: str = ""


@dataclass(frozen=True)
class Host:
    system: str
    machine: str
    cpus: int
    where: str


@dataclass(frozen=True)
class ProjectResult:
    name: str
    repo: str
    tag: str
    tree: Tree
    host: Host
    verdict: Literal["pass", "fail", "error"]
    steps: tuple[StepResult, ...]
    units_built: int
    units_held_back: int
    build_failures: tuple[Failure, ...]
    tests: tuple[TestOutcome, ...]
    test_failures: tuple[Failure, ...]
    # The tests ran in blaze's sandbox: the `sandbox` step opted the copy into `sandboxed-tests`.
    sandbox_tests: bool = False


@dataclass(frozen=True)
class Blaze:
    """The blaze under test: where it was, the BLAKE3 of its binary and of `blaze-engine`
    beside it ("" if none), and what `blaze --version` said."""

    path: str
    blake3: str
    engine_blake3: str
    version: str


@dataclass(frozen=True)
class Result:
    schema: int
    started: str
    blaze: Blaze
    projects: tuple[ProjectResult, ...]


class StepFailed(Exception):
    """A step the harness could not finish, its message naming the step, the command, its exit
    status and the end of what it printed."""


# ---------------------------------------------------------------------------------------------
# Reading blaze's output: a state machine fed one line at a time, owning no IO.

ANSI = re.compile(r"\x1b\[[0-9;?]*[A-Za-z]")
RUNNING = re.compile(r"^\s*Running (.+?) \((.+)\)$")
DOC_TESTS = re.compile(r"^\s*Doc-tests (\S+)$")
RUNNING_TESTS = re.compile(r"^running \d+ tests?$")
TEST_LINE = re.compile(r"^test (.+?)(?: - (?:should panic|compile fail|compilation))? \.\.\. ?(.*)$")
RESULT_LINE = re.compile(r"^test result: (\w+)\. (\d+) passed; (\d+) failed; (\d+) ignored; (\d+) measured")
SECTION = re.compile(r"^---- (.+?) stdout ----$")
LISTED = re.compile(r"^    (\S.*)$")
PANIC = re.compile(r"^thread '[^']*'(?: \(\d+\))? panicked at [^\n]*?:?$")
CRASHED = re.compile(r"process didn't exit successfully: .*\((exit status: \d+|signal: .*)\)\s*$")
PLAIN_ERROR = re.compile(r"^error(?:\[(E\d+)\])?: (.+)$")
# The `error:` lines that sum up failures named elsewhere, not failures of their own.
SUMMING_UP = re.compile(
    r"^(aborting due to|could not compile|test failed, to rerun|\d+ targets? failed|build failed"
    r"|could not build|failed to build|.*previous errors?)"
)
HASHED = re.compile(r"-[0-9a-f]{16}$")


def normalized(text: str) -> str:
    """`text` as a cluster's key: its first line, without colour, paths, hashes, addresses or
    numbers, at most 160 characters. Names in backticks stay: they are what tells two bugs
    apart."""
    line = ANSI.sub("", text).strip().splitlines()[0] if text.strip() else ""
    line = re.sub(r"(?:[A-Za-z]:)?(?:/[\w.@+-]+){2,}/?", "<path>", line)
    line = re.sub(r"\b0x[0-9a-fA-F]+\b", "0x…", line)
    line = re.sub(r"\b[0-9a-f]{12,}\b", "<hash>", line)
    line = re.sub(r"\b\d+\b", "N", line)
    return line[:160]


def package_name(package_id: str) -> str:
    """The name of a package by cargo's id of it: `path+file:///x/ws#name@0.1.0`,
    `registry+https://...#name@1.0.0`, `path+file:///x/name#0.1.0`, or the old `name 0.1.0 (...)`."""
    if "#" in package_id:
        url, _, rest = package_id.rpartition("#")
        if "@" in rest:
            return rest.split("@")[0]
        return url.rstrip("/").rsplit("/", 1)[-1]
    return package_id.split(" ")[0]


def exe_label(target: str, path: str, packages: Mapping[str, str]) -> str:
    """A test executable as `<package>/<crate> (<target>)`: `nu-path/nu_path (unittests
    src/lib.rs)`; without its package when the build named no executable at that path."""
    stem = HASHED.sub("", Path(path).stem)
    package = packages.get(Path(path).name)
    return f"{package}/{stem} ({target})" if package else f"{stem} ({target})"


@dataclass
class Executable:
    """The test executable being read."""

    label: str
    outcomes: dict[str, Outcome] = field(default_factory=dict[str, Outcome])
    pending: list[str] = field(default_factory=list[str])
    listed: list[str] = field(default_factory=list[str])
    sections: dict[str, list[str]] = field(default_factory=dict[str, list[str]])
    section: str | None = None
    in_list: bool = False
    started: bool = False
    ended: bool = False
    crashed: str = ""


@dataclass
class Reader:
    """What a `blaze build` or `blaze test` printed (stdout and stderr in one stream), read a line
    at a time: `feed` each line, then `finish`."""

    units_built: int = 0
    units_held_back: int = 0
    build_failures: list[Failure] = field(default_factory=list[Failure])
    tests: list[TestOutcome] = field(default_factory=list[TestOutcome])
    test_failures: list[Failure] = field(default_factory=list[Failure])
    exe: Executable | None = None
    # Each test executable's file name to its package, from `compiler-artifact`'s `executable`:
    # two packages' `tests/it.rs` are two executables named `it`.
    packages: dict[str, str] = field(default_factory=dict[str, str])

    def feed(self, raw: str) -> None:
        line = ANSI.sub("", raw.rstrip("\n").rstrip("\r"))
        if line.startswith("{"):
            try:
                message: object = json.loads(line)
            except json.JSONDecodeError:
                message = None
            if isinstance(message, dict):
                self.json(cast(dict[str, object], message))
                return
        self.text(line)

    def json(self, message: dict[str, object]) -> None:
        reason = message.get("reason")
        if reason == "compiler-artifact":
            self.units_built += 1
            executable = message.get("executable")
            if isinstance(executable, str) and executable:
                self.packages[Path(executable).name] = package_name(str(message.get("package_id") or "?"))
        elif reason == "compiler-message":
            self.compiler_message(message)
        elif reason == "blaze-unchecked":
            self.unchecked(message)

    def compiler_message(self, message: dict[str, object]) -> None:
        inner = message.get("message")
        if not isinstance(inner, dict):
            return
        diagnostic = cast(dict[str, object], inner)
        level = diagnostic.get("level")
        if not isinstance(level, str) or not level.startswith("error"):
            return
        said = str(diagnostic.get("message") or "")
        if SUMMING_UP.match(said):
            return
        code_of = diagnostic.get("code")
        code = ""
        if isinstance(code_of, dict):
            code = str(cast(dict[str, object], code_of).get("code") or "")
        target = message.get("target")
        unit = package_name(str(message.get("package_id") or "?"))
        if isinstance(target, dict):
            named = cast(dict[str, object], target)
            kinds = named.get("kind")
            kind = str(cast(list[object], kinds)[0]) if isinstance(kinds, list) and kinds else "?"
            unit = f"{unit} ({kind} {named.get('name')})"
        rendered = str(diagnostic.get("rendered") or said)
        cluster = f"{code} {normalized(said)}".strip()
        self.build_failures.append(Failure(unit, cluster, "\n".join(rendered.splitlines()[:12])))

    def unchecked(self, message: dict[str, object]) -> None:
        behind = message.get("behind")
        if isinstance(behind, list):
            for it in cast(list[object], behind):
                if isinstance(it, dict):
                    units = cast(dict[str, object], it).get("units")
                    self.units_held_back += len(cast(list[object], units)) if isinstance(units, list) else 0
        tests = message.get("tests")
        if isinstance(tests, list):
            for it in cast(list[object], tests):
                if isinstance(it, dict):
                    entry = cast(dict[str, object], it)
                    why = str(entry.get("why") or "")
                    unit = str(entry.get("unit") or "?")
                    held = str(entry.get("behind") or "")
                    cluster = f"never built: {normalized(why)}" if why else "never built"
                    detail = f"behind {held}: {why}" if held else why
                    self.test_failures.append(Failure(unit, cluster, detail))

    def text(self, line: str) -> None:
        exe = self.exe
        if exe is not None and not exe.ended and exe.section is not None:
            # A failed test's captured output is its own, whatever it looks like (a test that runs
            # `cargo test` prints `Running` and `test result:` lines of its own).
            self.test_text(exe, line)
            return
        if m := RUNNING.match(line):
            self.start(exe_label(m.group(1), m.group(2), self.packages))
            return
        if m := DOC_TESTS.match(line):
            self.start(f"{m.group(1)} (doc-tests)")
            return
        exe = self.exe
        if exe is not None and not exe.ended:
            self.test_text(exe, line)
            return
        if exe is not None and (m := CRASHED.search(line)) and not exe.crashed:
            exe.crashed = m.group(1)
            return
        if (m := PLAIN_ERROR.match(line)) and not SUMMING_UP.match(m.group(2)):
            code = m.group(1) or ""
            self.build_failures.append(Failure("(the build)", f"{code} {normalized(m.group(2))}".strip(), line))

    def test_text(self, exe: Executable, line: str) -> None:
        if m := SECTION.match(line):
            name = m.group(1)
            exe.section = name
            exe.sections[name] = []
            exe.in_list = False
            return
        if line == "failures:":
            # libtest prints it before the failures' output and again before their list.
            exe.section = None
            exe.in_list = True
            return
        if exe.section is not None:
            exe.sections[exe.section].append(line)
            return
        if exe.in_list:
            if m := LISTED.match(line):
                exe.listed.append(m.group(1).strip())
                return
            if line.strip():
                exe.in_list = False
        if RUNNING_TESTS.match(line):
            exe.started = True
            return
        if RESULT_LINE.match(line):
            exe.ended = True
            return
        if m := CRASHED.search(line):
            exe.crashed = m.group(1)
            return
        if m := TEST_LINE.match(line):
            name, status = m.group(1), m.group(2).strip()
            if status == "ok" or status.startswith("bench"):
                exe.outcomes[name] = "passed"
            elif status == "FAILED":
                exe.outcomes[name] = "failed"
            elif status.startswith("ignored"):
                exe.outcomes[name] = "ignored"
            else:
                # The result lands on a later line when the test (or a program it started) prints
                # in between; the list of failures at the end decides it.
                exe.pending.append(name)

    def start(self, label: str) -> None:
        self.close()
        self.exe = Executable(label)

    def close(self) -> None:
        """The executable being read is done: each test's outcome, each failure's panic, and the
        executable itself a failure when it crashed or ended with no `test result:` line."""
        exe = self.exe
        if exe is None:
            return
        self.exe = None
        failed = set(exe.listed) | {name for name, it in exe.outcomes.items() if it == "failed"}
        crashed = exe.started and not exe.ended
        for name in exe.pending:
            # In an executable that crashed, a test with no outcome is the one it crashed in (one
            # thread prints `test x ... ` before running x).
            exe.outcomes.setdefault(name, "failed" if name in failed or crashed else "passed")
        for name in sorted(failed):
            exe.outcomes[name] = "failed"
        for name in sorted(exe.outcomes):
            self.tests.append(TestOutcome(exe.label, name, exe.outcomes[name]))
            if exe.outcomes[name] == "failed":
                lines = exe.sections.get(name, [])
                self.test_failures.append(
                    Failure(f"{exe.label} :: {name}", panic_cluster(lines), "\n".join(lines[:12]))
                )
        if crashed:
            why = exe.crashed or "no `test result:` line"
            self.test_failures.append(Failure(exe.label, f"crashed: {normalized(why)}", why))
        elif exe.crashed and not failed:
            self.test_failures.append(Failure(exe.label, f"exited: {normalized(exe.crashed)}", exe.crashed))

    def finish(self) -> "Reader":
        self.close()
        return self


def panic_cluster(lines: Sequence[str]) -> str:
    """The cluster of a failed test by what its section printed: the message of its first panic,
    else its first line."""
    for at, line in enumerate(lines):
        if PANIC.match(line):
            message = lines[at + 1] if at + 1 < len(lines) and line.endswith(":") else line
            return f"panic: {normalized(message)}"
        if line.startswith("note: test did not panic as expected"):
            return "did not panic as expected"
    first = next((line for line in lines if line.strip()), "")
    return f"failed: {normalized(first)}" if first else "failed, printing nothing"


# ---------------------------------------------------------------------------------------------
# Running commands


@dataclass(frozen=True)
class Ran:
    status: int
    tail: str
    seconds: float


async def run(
    step: str,
    argv: Sequence[str],
    *,
    cwd: Path,
    env: Mapping[str, str] | None = None,
    minutes: float,
    log: Path | None = None,
    check: bool = True,
    each: Callable[[str], None] | None = None,
    stdin: Path | None = None,
    stdout: Path | None = None,
) -> Ran:
    """Runs `argv` in a process group of its own, stdout and stderr in one stream, each line
    written to `log` and given to `each` as it comes; `stdout` instead takes the bytes whole (an
    archive). Past `minutes` the group is killed and the step fails; with `check`, so does a
    status other than 0. Every failure names the step, the command, the status and the tail."""
    command = " ".join(argv)
    began = time.monotonic()
    tail: list[str] = []
    held = 0
    out_file = stdout.open("wb") if stdout is not None else None
    in_file = stdin.open("rb") if stdin is not None else None
    log_file = log.open("ab") if log is not None else None  # a step's commands share its log
    try:
        try:
            proc = await asyncio.create_subprocess_exec(
                *argv,
                cwd=cwd,
                env=dict(env) if env is not None else None,
                stdin=in_file if in_file is not None else asyncio.subprocess.DEVNULL,
                stdout=out_file if out_file is not None else asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.PIPE if out_file is not None else asyncio.subprocess.STDOUT,
                start_new_session=True,
                limit=LINE_LIMIT,
            )
        except OSError as e:
            raise StepFailed(f"{step}: `{command}` in {cwd} did not start: {e}") from None
        stream = proc.stderr if out_file is not None else proc.stdout

        async def read() -> None:
            nonlocal held
            if stream is None:
                return
            async for raw in stream:
                if log_file is not None:
                    log_file.write(raw)
                line = raw.decode(errors="replace")
                tail.append(line)
                held += len(line)
                while held > TAIL * 2 and len(tail) > 1:
                    held -= len(tail.pop(0))
                if each is not None:
                    each(line)

        reader = asyncio.create_task(read())
        try:
            async with asyncio.timeout(minutes * 60):
                status = await proc.wait()
            # The command ended; a process it left behind (a test's server) may still hold the
            # pipe open. What is in the pipe is read for a bounded time, then the group is ended.
            try:
                async with asyncio.timeout(DRAIN_SECS):
                    await asyncio.shield(reader)
            except TimeoutError:
                kill_group(proc.pid)
                tail.append(f"\n[realprojects: a process {command!r} started still held its output "
                            f"{DRAIN_SECS} s after it exited; its process group was killed]\n")
                reader.cancel()
        except TimeoutError:
            # The whole group: the tests and build scripts the command started too.
            kill_group(proc.pid)
            await proc.wait()
            raise StepFailed(
                f"{step}: `{command}` in {cwd} ran past {minutes:g} minutes; its last lines:\n{''.join(tail)[-TAIL:]}"
            ) from None
        except BaseException:
            # Cancelled (another project's harness bug) or anything else: nothing is left running.
            kill_group(proc.pid)
            raise
        finally:
            if not reader.done():
                reader.cancel()
            try:
                await reader
            except asyncio.CancelledError:
                pass
            except ValueError as e:
                # A line longer than LINE_LIMIT.
                raise StepFailed(f"{step}: `{command}` in {cwd} printed a line past {LINE_LIMIT} bytes: {e}") from None
    finally:
        for f in (out_file, in_file, log_file):
            if f is not None:
                f.close()
    said = "".join(tail)[-TAIL:]
    if check and status != 0:
        raise StepFailed(f"{step}: `{command}` in {cwd} exited {status}:\n{said}")
    return Ran(status, said, time.monotonic() - began)


def kill_group(pid: int) -> None:
    try:
        os.killpg(pid, 9)
    except (ProcessLookupError, PermissionError):
        pass


def blaze_env(home: Path, extra: Sequence[pins.Env]) -> dict[str, str]:
    """The environment of a blaze command: the caller's without SCRUBBED, the daemon home
    `home`, no colour, no backtraces (a panic prints as a user's does), and the workspace's own
    variables."""
    env = {k: v for k, v in os.environ.items() if k not in SCRUBBED}
    env.update(BLAZE_HOME=str(home), RUST_BACKTRACE="0", CARGO_TERM_COLOR="never")
    env.update({it.name: it.value for it in extra})
    return env


# ---------------------------------------------------------------------------------------------
# Fetching and the exact tree


async def git_out(step: str, argv: Sequence[str], cwd: Path, env: Mapping[str, str] | None = None) -> str:
    with tempfile.NamedTemporaryFile(suffix=".out") as out:
        await run(step, argv, cwd=cwd, env=env, minutes=SMALL_MINUTES, stdout=Path(out.name))
        return Path(out.name).read_text().strip()


async def fetched(ws: Workspace, sources: Path, log: Path) -> Path:
    """The checkout of `ws` in `sources`, at its pin: fetched when it is not there whole."""
    at = sources / f"{ws.name}-{ws.rev[:12]}"
    if at.is_dir() and (at / ".realprojects-done").is_file():
        return at
    shutil.rmtree(at, ignore_errors=True)
    part = sources / f".{ws.name}-{ws.rev[:12]}.{os.getpid()}.part"
    shutil.rmtree(part, ignore_errors=True)
    part.mkdir(parents=True)
    url = f"https://github.com/{ws.repo}"
    step = f"{ws.name}: fetch"
    await run(step, ["git", "init", "-q", str(part)], cwd=sources, minutes=SMALL_MINUTES, log=log)
    await run(step, ["git", "-C", str(part), "fetch", "-q", "--depth", "1", url, ws.rev], cwd=part,
              minutes=FETCH_MINUTES, log=log)
    await run(step, ["git", "-C", str(part), "checkout", "-q", "--detach", "FETCH_HEAD"], cwd=part,
              minutes=SMALL_MINUTES, log=log)
    head = await git_out(step, ["git", "-C", str(part), "rev-parse", "HEAD"], part)
    tree = await git_out(step, ["git", "-C", str(part), "rev-parse", "HEAD^{tree}"], part)
    if head != ws.rev or tree != ws.tree:
        raise StepFailed(f"{step}: {url} gave commit {head} with tree {tree}; corpus.json pins {ws.rev} with tree {ws.tree}")
    if ws.submodules:
        await run(step, ["git", "-C", str(part), "submodule", "update", "--init", "--recursive", "--depth", "1", "-q"],
                  cwd=part, minutes=FETCH_MINUTES, log=log)
    (part / ".realprojects-done").write_text(f"{ws.rev} {ws.tree}\n")
    part.rename(at)
    return at


async def exported(commit: str, dest: Path, log: Path) -> tuple[str, str, str]:
    """This repository's `commit` at `dest`, as `git archive` gives it: its sha, tree id and
    commit date (UTC)."""
    step = "blaze: fetch"
    sha = await git_out(step, ["git", "-C", str(ROOT), "rev-parse", "--verify", f"{commit}^{{commit}}"], ROOT)
    tree = await git_out(step, ["git", "-C", str(ROOT), "rev-parse", f"{sha}^{{tree}}"], ROOT)
    date = await git_out(step, ["git", "-C", str(ROOT), "show", "-s", "--date=format-local:%Y-%m-%d", "--format=%cd", sha],
                         ROOT, {**os.environ, "TZ": "UTC0"})
    archive = dest.with_name(dest.name + ".tar")
    await run(step, ["git", "-C", str(ROOT), "archive", "--format=tar", "-o", str(archive), sha], cwd=ROOT,
              minutes=SMALL_MINUTES, log=log)

    def unpack() -> None:
        dest.mkdir(parents=True)
        with tarfile.open(archive) as tar:
            tar.extractall(dest, filter="data")
        archive.unlink()

    await asyncio.to_thread(unpack)
    return sha, tree, date


async def copied(source: Path, dest: Path, log: Path, step: str) -> None:
    """`source` at `dest`, cloned block for block where the volume can (APFS, Btrfs, XFS)."""
    if sys.platform == "darwin":
        argv = ["/bin/cp", "-cR", str(source), str(dest)]
    else:
        argv = ["cp", "-a", "--reflink=auto", str(source), str(dest)]
    await run(step, argv, cwd=source.parent, minutes=FETCH_MINUTES, log=log)
    (dest / ".realprojects-done").unlink(missing_ok=True)


def tree_digest(root: Path) -> str:
    """The BLAKE3 of the tree at `root`: each file's path, executable bit and BLAKE3, and each
    symbolic link's target, in path order; `.git` (a directory or a submodule's file) left out."""
    entries: list[tuple[str, str]] = []
    for at, dirs, files in os.walk(root):
        dirs[:] = sorted(d for d in dirs if d != ".git" and not (Path(at) / d).is_symlink())
        for name in files + [d for d in os.listdir(at) if (Path(at) / d).is_symlink() and (Path(at) / d).is_dir()]:
            if name == ".git":
                continue
            path = Path(at) / name
            rel = path.relative_to(root).as_posix()
            if path.is_symlink():
                entries.append((rel, f"link {os.readlink(path)}"))
            else:
                mode = "x" if os.access(path, os.X_OK) else "-"
                entries.append((rel, f"{mode} {blake3.blake3(path.read_bytes()).hexdigest()}"))
    whole = blake3.blake3()
    for rel, what in sorted(entries):
        whole.update(f"{rel}\0{what}\n".encode())
    return whole.hexdigest()


SANDBOXED_TESTS = "sandboxed-tests"
SANDBOX_TABLE = "[workspace.metadata.blaze]"


def sandboxed_manifest(text: str, where: str) -> str:
    """The root manifest `text` (at `where`, for errors) with `sandboxed-tests` in `experimental`
    of `[workspace.metadata.blaze]`: the table appended when there is none, the key added to the
    table that is there, the experiment added to a one-line list of strings. Anything else (no
    `[workspace]`, the table written inline or by dotted keys, `experimental` not such a list)
    is refused naming the file; the result is parsed back and must be `text` with that one
    experiment added."""
    def refuse(why: str) -> StepFailed:
        return StepFailed(f"sandbox: {where}: cannot add {SANDBOXED_TESTS!r} to `experimental` in {SANDBOX_TABLE}: {why}")

    try:
        before = tomllib.loads(text)
    except tomllib.TOMLDecodeError as e:
        raise refuse(f"it is not TOML: {e}") from None
    workspace = before.get("workspace")
    if not isinstance(workspace, dict):
        raise refuse("it has no [workspace] table (this edit would make it a workspace root)")
    metadata = cast(dict[str, object], workspace).get("metadata", {})
    blaze = cast(dict[str, object], metadata).get("blaze") if isinstance(metadata, dict) else None
    header = re.compile(r"^[ \t]*\[[ \t]*workspace[ \t]*\.[ \t]*metadata[ \t]*\.[ \t]*blaze[ \t]*\][ \t]*(#.*)?$", re.MULTILINE)
    if blaze is None:
        after = text + ("" if text.endswith("\n") or not text else "\n") + f"\n{SANDBOX_TABLE}\nexperimental = [\"{SANDBOXED_TESTS}\"]\n"
    elif not isinstance(blaze, dict):
        raise refuse(f"`workspace.metadata.blaze` is a {type(blaze).__name__}, not a table")
    elif "experimental" not in blaze:
        found = list(header.finditer(text))
        if len(found) != 1:
            raise refuse(f"the table is written inline or by dotted keys, not as one {SANDBOX_TABLE} header")
        at = found[0].end()
        after = text[:at] + f"\nexperimental = [\"{SANDBOXED_TESTS}\"]" + text[at:]
    else:
        experimental = cast(dict[str, object], blaze)["experimental"]
        if not isinstance(experimental, list) or not all(isinstance(x, str) for x in cast(list[object], experimental)):
            raise refuse(f"`experimental` is {experimental!r}, not a list of strings")
        if SANDBOXED_TESTS in experimental:
            return text
        found = list(header.finditer(text))
        lists = re.compile(r"^([ \t]*experimental[ \t]*=[ \t]*\[)([^\]\n]*\][ \t]*(#.*)?)$", re.MULTILINE)
        at = found[0].end() if len(found) == 1 else -1
        line = lists.search(text, at) if at >= 0 else None
        nxt = re.compile(r"^[ \t]*\[", re.MULTILINE).search(text, at) if at >= 0 else None
        if line is None or (nxt is not None and nxt.start() < line.start()):
            raise refuse(f"`experimental` = {experimental!r} is not one line under one {SANDBOX_TABLE} header")
        sep = ", " if experimental else ""
        after = text[:line.start()] + line.group(1) + f"\"{SANDBOXED_TESTS}\"{sep}" + line.group(2) + text[line.end():]
    # The edit is held to what it must be: the same manifest with the one experiment added.
    want = json.loads(json.dumps(before))
    table = want["workspace"].setdefault("metadata", {}).setdefault("blaze", {})
    table["experimental"] = [SANDBOXED_TESTS, *table.get("experimental", [])]
    try:
        got = tomllib.loads(after)
    except tomllib.TOMLDecodeError as e:
        raise refuse(f"the edited manifest does not parse ({e})") from None
    if json.loads(json.dumps(got)) != want:
        raise refuse("the edited manifest is not the same manifest with the one experiment added")
    return after


def file_blake3(path: Path) -> str:
    return blake3.blake3(path.read_bytes()).hexdigest() if path.is_file() else ""


# ---------------------------------------------------------------------------------------------
# One project


@dataclass
class Project:
    """What `run_project` gathers of one workspace as its steps go."""

    ws: Workspace
    host: Host
    steps: list[StepResult] = field(default_factory=list[StepResult])
    tree: Tree = Tree("", "", "", "")
    build: Reader = field(default_factory=Reader)
    test: Reader = field(default_factory=Reader)
    errors: list[str] = field(default_factory=list[str])
    before_sandbox: str = ""
    sandboxed: bool = False

    def result(self) -> ProjectResult:
        bad_exit = any(s.exit not in (0, None) for s in self.steps if s.step in ("build", "test"))
        failed = bad_exit or self.build.build_failures or self.test.test_failures
        verdict = "error" if self.errors else "fail" if failed else "pass"
        return ProjectResult(
            name=self.ws.name, repo=self.ws.repo, tag=self.ws.tag, tree=self.tree, host=self.host,
            verdict=verdict, steps=tuple(self.steps), units_built=self.build.units_built,
            units_held_back=self.build.units_held_back, build_failures=tuple(self.build.build_failures),
            tests=tuple(self.test.tests), test_failures=tuple(self.test.test_failures),
            sandbox_tests=self.sandboxed,
        )


def this_host(where: str) -> Host:
    return Host(platform.system(), platform.machine(), os.cpu_count() or 0, where)


async def step(project: Project, name: StepName, argv: Sequence[str], body: Callable[[], Awaitable[int]],
               logged: bool = False) -> bool:
    """Runs `body` as the step `name` of `project`, recording it (with its log under logs/ when
    `logged`): False when the harness could not finish it, its error kept."""
    began = time.monotonic()
    log = f"logs/{project.ws.name}/{name.replace(' ', '-')}.log" if logged else ""
    try:
        status = await body()
    except Exception as e:  # noqa: BLE001 - a harness bug fails its project, not the run (rule panics-fail-one-job)
        message = str(e) if isinstance(e, StepFailed) else f"{project.ws.name}: {name}: {type(e).__name__}: {e}"
        project.errors.append(message)
        project.steps.append(StepResult(name, tuple(argv), None, round(time.monotonic() - began, 2), log, message))
        return False
    project.steps.append(StepResult(name, tuple(argv), status, round(time.monotonic() - began, 2), log, ""))
    return True


async def run_project(ws: Workspace, args: Run, blaze: Path, work: Path, out: Path, sources: Path) -> ProjectResult:
    """Every step of `ws`, never raising for what the harness could not do: that is the result's
    error, and the steps after it do not run."""
    project = Project(ws, this_host(args.where))
    root = work / ws.name
    shutil.rmtree(root, ignore_errors=True)
    root.mkdir(parents=True)
    logs = out / "logs" / ws.name
    shutil.rmtree(logs, ignore_errors=True)
    logs.mkdir(parents=True)
    checkout = root / "ws"
    home = work / "home" if args.home == "shared" else root / "home"
    env = blaze_env(home, ws.env)
    rev, tree = ws.rev, ws.tree

    async def fetch() -> int:
        nonlocal rev, tree
        if ws.repo == "self":
            rev, tree, date = await exported(args.commit, checkout, logs / "fetch.log")
            # The export has no `.git`: blaze's build script is told the commit, as release.yaml
            # tells it.
            env.update(BLAZE_GIT_SHA=(args.commit_label or rev)[:7], BLAZE_GIT_DATE=date)
        else:
            source = await fetched(ws, sources, logs / "fetch.log")
            await copied(source, checkout, logs / "fetch.log", f"{ws.name}: fetch")
        return 0

    locked: list[str] = []
    writing_lock = False

    async def lock() -> int:
        nonlocal writing_lock
        lock_path = checkout / "Cargo.lock"
        if ws.lock == "committed":
            if not lock_path.is_file():
                raise StepFailed(f"{ws.name}: lock: corpus.json says the project commits its Cargo.lock, and {rev} has none")
            locked.append("--locked")
        elif ws.lock_file().is_file():
            shutil.copyfile(ws.lock_file(), lock_path)
            locked.append("--locked")
        elif args.write_locks:
            lock_path.unlink(missing_ok=True)
            writing_lock = True
        else:
            raise StepFailed(
                f"{ws.name}: lock: no harness lock at {ws.lock_file()}; run once with `\"write_locks\": true` "
                "to have blaze resolve one and write it there, then commit it"
            )
        return 0

    async def sandbox() -> int:
        # The copy's manifest only: a new file in its place, so nothing it was cloned or linked
        # from (the source in `sources`) changes.
        manifest = checkout / "Cargo.toml"
        if manifest.is_symlink() or not manifest.is_file():
            raise StepFailed(f"{ws.name}: sandbox: {manifest} is not a file of the copy's own")
        project.before_sandbox = await asyncio.to_thread(tree_digest, checkout)
        edited = sandboxed_manifest(manifest.read_text(), f"{ws.name}'s {manifest.name} (the copy at {manifest})")
        part = manifest.with_name(manifest.name + ".realprojects")
        part.write_text(edited)
        part.replace(manifest)
        project.sandboxed = True
        return 0

    async def measure() -> int:
        digest = await asyncio.to_thread(tree_digest, checkout)
        lock_path = checkout / "Cargo.lock"
        lock_sha = hashlib.sha256(lock_path.read_bytes()).hexdigest() if lock_path.is_file() else ""
        project.tree = Tree(rev, tree, digest, lock_sha, project.before_sandbox)
        return 0

    def build_argv() -> list[str]:
        return [str(blaze), "build", "--workspace", "--all-targets", "--message-format=json", *locked, *ws.build_args]

    def test_argv() -> list[str]:
        return [str(blaze), "test", "--workspace", "--no-fail-fast", "--message-format=json", *locked, *ws.test_args]

    async def build() -> int:
        try:
            ran = await run(f"{ws.name}: build", build_argv(), cwd=checkout, env=env, minutes=ws.minutes,
                            log=logs / "build.log", check=False, each=project.build.feed)
        finally:
            project.build.finish()
        return ran.status

    async def test() -> int:
        try:
            ran = await run(f"{ws.name}: test", test_argv(), cwd=checkout, env=env, minutes=ws.minutes,
                            log=logs / "test.log", check=False, each=project.test.feed)
        finally:
            # A run that timed out still says the tests that ended, and the one it hung in.
            project.test.finish()
        return ran.status

    async def write_lock() -> int:
        written = checkout / "Cargo.lock"
        if not written.is_file():
            raise StepFailed(f"{ws.name}: write lock: blaze wrote no Cargo.lock in {checkout}")
        pins.LOCKS.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(written, ws.lock_file())
        lock_sha = hashlib.sha256(written.read_bytes()).hexdigest()
        project.tree = dataclasses.replace(project.tree, lock_sha256=lock_sha)
        print(f"{ws.name}: wrote {ws.lock_file()} (sha256 {lock_sha}); commit it", flush=True)
        return 0

    home.mkdir(parents=True, exist_ok=True, mode=0o700)
    try:
        ok = (await step(project, "fetch", ["git", "fetch", ws.repo, rev or args.commit], fetch, logged=True)
              and await step(project, "lock", [], lock)
              and (not ws.sandbox_tests or await step(project, "sandbox", [], sandbox))
              and await step(project, "tree", [], measure))
        if ok:
            # The tests run after a failed build too: blaze builds what it can and runs every test
            # that built, so the test step says what the build failure cost.
            built = await step(project, "build", build_argv(), build, logged=True)
            if built and writing_lock and await step(project, "write lock", [], write_lock):
                locked.append("--locked")
            await step(project, "test", test_argv(), test, logged=True)
    finally:
        if args.home == "per-project":
            await stop_daemon(project, blaze, checkout, env)
        if not args.keep:
            await asyncio.to_thread(shutil.rmtree, root, True)
    result = project.result()
    print(f"{ws.name}: {result.verdict} ({summary_line(result)})", flush=True)
    return result


async def stop_daemon(project: Project, blaze: Path, checkout: Path, env: Mapping[str, str]) -> None:
    if not checkout.is_dir():
        return
    # Recorded as a step of its own, not the project's error: a daemon that is not running (it
    # was never started, or it ended) may say so with a status other than 0.
    began = time.monotonic()
    argv = (str(blaze), "stop")
    try:
        ran = await run(f"{project.ws.name}: stopping the daemon", argv, cwd=checkout, env=env, minutes=5, check=False)
        project.steps.append(StepResult("stop", argv, ran.status, round(ran.seconds, 2), "", ""))
    except StepFailed as e:
        project.steps.append(StepResult("stop", argv, None, round(time.monotonic() - began, 2), "", str(e)))


def summary_line(p: ProjectResult) -> str:
    counts = Counter(t.outcome for t in p.tests)
    return (f"{p.units_built} units built, {len({f.where for f in p.build_failures})} failed; "
            f"tests {counts['passed']} passed, {counts['failed']} failed, {counts['ignored']} ignored")


# ---------------------------------------------------------------------------------------------
# The blaze under test


async def blaze_of(name: str, work: Path) -> tuple[Path, Blaze]:
    found = shutil.which(name)
    if found is None:
        raise StepFailed(f"blaze: `{name}` is no file and not on PATH")
    path = Path(found).resolve()
    with tempfile.NamedTemporaryFile(suffix=".version") as said:
        await run("blaze --version", [str(path), "--version"], cwd=work, minutes=SMALL_MINUTES, stdout=Path(said.name))
        version = Path(said.name).read_text().strip().splitlines()
    engine = path.with_name("blaze-engine" + path.suffix)
    return path, Blaze(str(path), file_blake3(path), file_blake3(engine), version[0] if version else "")


# ---------------------------------------------------------------------------------------------
# In ix VMs


async def run_in_vm(ws: Workspace, args: Run, out: Path, scratch: Path) -> Path:
    """`ws` run in an ix VM of its own; the path of the result.json it sent back."""
    name = f"rp-{ws.name}-{os.getpid()}"[:40]
    log = out / "logs" / ws.name / "vm.log"
    log.parent.mkdir(parents=True, exist_ok=True)
    step = f"{ws.name}: vm"
    sha = await git_out(step, ["git", "-C", str(ROOT), "rev-parse", "--verify", f"{args.commit}^{{commit}}"], ROOT)
    # One parentless commit of the tree: a third of the size of the history's bundle.
    lone = await git_out(step, ["git", "-C", str(ROOT), "commit-tree", f"{sha}^{{tree}}", "-m", f"realprojects {sha}"], ROOT)
    # A branch, so `git clone` of the bundle takes it; deleted once bundled.
    ref = f"refs/heads/{name}"
    bundle = scratch / f"{name}.bundle"
    await run(step, ["git", "-C", str(ROOT), "update-ref", ref, lone], cwd=ROOT, minutes=SMALL_MINUTES, log=log)
    try:
        await run(step, ["git", "-C", str(ROOT), "bundle", "create", str(bundle), ref], cwd=ROOT,
                  minutes=SMALL_MINUTES, log=log)
    finally:
        await run(step, ["git", "-C", str(ROOT), "update-ref", "-d", ref], cwd=ROOT, minutes=SMALL_MINUTES, log=log)
    release = args.vm.blaze or (ROOT / ".github" / "blaze-version").read_text().strip()
    vm = args.vm
    try:
        # Inside the `try`: an `ix new` that timed out may have made the VM all the same.
        await run(step, ["ix", "new", "--name", name, "--no-shell", "--cpus", str(vm.cpus), "--memory", str(vm.memory),
                         "--memory-commit", "--arch", vm.arch, "--ttl", vm.ttl], cwd=scratch, minutes=SMALL_MINUTES,
                  log=log)
        shell = ["ix", "shell", name, "--"]
        await run(step, [*shell, "bash", "-c", f"cat > /root/src.bundle && git clone -q -b {name} /root/src.bundle /root/blaze"],
                  cwd=scratch, minutes=FETCH_MINUTES, log=log, stdin=bundle)
        await run(step, [*shell, "bash", "-c", f"cd /root/blaze && sh site/install.sh --version {release} --verbose"],
                  cwd=scratch, minutes=FETCH_MINUTES, log=log)
        remote = json.dumps({
            "out": "/root/out", "blaze": "/root/.cargo/bin/blaze", "projects": [ws.name], "jobs": 1,
            "commit": lone, "commit_label": sha, "write_locks": args.write_locks, "where": "local",
        })
        nix = ["nixpkgs#" + pkg for pkg in vm.nix]
        await run(step, [*shell, "nix", "shell", *nix, "-c", "uv", "run", "--frozen", "--project", "/root/blaze",
                         "python", "/root/blaze/harness/v2/realprojects.py", "run", remote],
                  cwd=scratch, minutes=ws.minutes * 2 + 60, log=log, check=False)
        archive = scratch / f"{name}.tar"
        await run(step, [*shell, "tar", "-C", "/root/out", "-c", "."], cwd=scratch, minutes=FETCH_MINUTES, stdout=archive)
        if args.write_locks and ws.lock == "harness" and not ws.lock_file().is_file():
            fetched_lock = scratch / f"{name}.Cargo.lock"
            got = await run(step, [*shell, "cat", f"/root/blaze/harness/v2/locks/{ws.name}.Cargo.lock"], cwd=scratch,
                            minutes=SMALL_MINUTES, stdout=fetched_lock, check=False)
            # Only a lock the VM wrote: none there leaves none here, never an empty one.
            if got.status == 0 and fetched_lock.stat().st_size > 0:
                pins.LOCKS.mkdir(parents=True, exist_ok=True)
                shutil.move(fetched_lock, ws.lock_file())
    finally:
        if not args.keep:
            await run(step, ["ix", "rm", name, "--force"], cwd=scratch, minutes=SMALL_MINUTES, log=log, check=False)
    dest = out / "vm" / ws.name

    def unpack() -> None:
        shutil.rmtree(dest, ignore_errors=True)
        dest.mkdir(parents=True)
        with tarfile.open(archive) as tar:
            tar.extractall(dest, filter="data")

    await asyncio.to_thread(unpack)
    result = dest / "result.json"
    if not result.is_file():
        raise StepFailed(f"{step}: the VM's run sent back no result.json; see {log}")
    return result


def missing(ws: Workspace, at: StepName, message: str, where: str = "") -> ProjectResult:
    """The result of a project the harness has no result of: an error at step `at`."""
    log = f"logs/{ws.name}/vm.log" if at == "vm" else ""
    step = StepResult(at, (), None, 0.0, log, f"{ws.name}: {at}: {message}")
    return ProjectResult(ws.name, ws.repo, ws.tag, Tree(ws.rev, ws.tree, "", ""), Host("", "", 0, where), "error",
                         (step,), 0, 0, (), (), ())


# ---------------------------------------------------------------------------------------------
# Against the baseline


@dataclass(frozen=True)
class Changes:
    """What changed in a project against the baseline, or why it was not compared."""

    compared: bool
    why: str = ""
    broke: tuple[str, ...] = ()
    fixed: tuple[str, ...] = ()
    gone: tuple[str, ...] = ()
    new_tests: int = 0
    units_broke: tuple[str, ...] = ()
    units_fixed: tuple[str, ...] = ()
    # Test executables that newly crashed, exited other than 0 or never built (a `harness =
    # false` target has no tests of its own to say it), and those that no longer do.
    exes_broke: tuple[str, ...] = ()
    exes_fixed: tuple[str, ...] = ()


def changes(now: ProjectResult, before: ProjectResult | None) -> Changes:
    """`now` against `before`: tests that passed and fail (or no longer ran, `gone`), tests that
    failed and pass, units that newly fail or got fixed. Only at the same commit and lock: else
    which moved."""
    if before is None:
        return Changes(False, "not in the baseline")
    if (now.tree.rev, now.tree.tree) != (before.tree.rev, before.tree.tree):
        return Changes(False, f"the pin moved from {before.tree.rev[:12]} to {now.tree.rev[:12]}")
    if now.tree.lock_sha256 != before.tree.lock_sha256:
        return Changes(False, "the lock moved")
    if before.verdict == "error" or now.verdict == "error":
        return Changes(False, "the harness did not finish one of the two runs")
    was = {(t.exe, t.name): t.outcome for t in before.tests}
    now_ = {(t.exe, t.name): t.outcome for t in now.tests}
    name: Callable[[tuple[str, str]], str] = lambda k: f"{k[0]} :: {k[1]}"
    broke = sorted(name(k) for k, o in now_.items() if o == "failed" and was.get(k) == "passed")
    fixed = sorted(name(k) for k, o in now_.items() if o == "passed" and was.get(k) == "failed")
    gone = sorted(name(k) for k, o in was.items() if o == "passed" and k not in now_)
    units_was, units_now = failed_units(before), failed_units(now)
    exes_was, exes_now = failed_exes(before), failed_exes(now)
    return Changes(True, "", tuple(broke), tuple(fixed), tuple(gone), len(set(now_) - set(was)),
                   tuple(sorted(units_now - units_was)), tuple(sorted(units_was - units_now)),
                   tuple(sorted(exes_now - exes_was)), tuple(sorted(exes_was - exes_now)))


def failed_units(p: ProjectResult) -> set[str]:
    """Each unit that failed to build; an error no unit was named for (a plain `error:` line) by
    its cluster, so a new one is not hidden behind an old one."""
    return {f"{f.where}: {f.cluster}" if f.where == "(the build)" else f.where for f in p.build_failures}


def failed_exes(p: ProjectResult) -> set[str]:
    """Each test executable that failed as a whole, with how (its cluster)."""
    return {f"{f.where}: {f.cluster}" for f in p.test_failures if " :: " not in f.where}


def newly_broken(c: Changes) -> bool:
    return bool(c.broke or c.gone or c.units_broke or c.exes_broke)


# ---------------------------------------------------------------------------------------------
# The report


def clusters(failures: Sequence[Failure], top: int) -> list[tuple[str, int, list[str]]]:
    """The `top` largest clusters: (key, how many, the first three places)."""
    by: dict[str, list[str]] = {}
    for f in failures:
        by.setdefault(f.cluster, []).append(f.where)
    ranked = sorted(by.items(), key=lambda kv: (-len(kv[1]), kv[0]))
    return [(key, len(places), sorted(set(places))[:3]) for key, places in ranked[:top]]


def cell(text: str) -> str:
    return text.replace("|", "\\|").replace("\n", " ")


def step_of(p: ProjectResult, name: str) -> StepResult | None:
    return next((s for s in p.steps if s.step == name), None)


def report(result: Result, compared: Mapping[str, Changes], baseline: str) -> str:
    b = result.blaze
    lines = [
        f"# Real projects under blaze {b.version or '?'}",
        "",
        (f"blaze `{b.path}`, BLAKE3 `{b.blake3[:16]}`; started {result.started}; "
         f"baseline: {f'`{baseline}`' if baseline else 'none'}."),
        "Each project is built by blaze and judged by its own tests; every failure is a blaze bug.",
        "",
        "| Project | Commit | Verdict | Units built | Units failed | Tests passed | Failed | Ignored | Build s | Test s | Against the baseline |",
        "|---|---|---|---:|---:|---:|---:|---:|---:|---:|---|",
    ]
    for p in result.projects:
        counts = Counter(t.outcome for t in p.tests)
        build, test = step_of(p, "build"), step_of(p, "test")
        c = compared.get(p.name)
        against = "" if c is None else (
            f"{len(c.broke) + len(c.gone)} tests broke, {len(c.fixed)} fixed; {len(c.units_broke)} units broke, "
            f"{len(c.units_fixed)} fixed; {len(c.exes_broke)} executables broke, {len(c.exes_fixed)} fixed"
            if c.compared else c.why)
        lines.append(
            f"| {p.name} | `{p.tree.rev[:12]}` | {p.verdict} | {p.units_built} | {len({f.where for f in p.build_failures})} "
            f"| {counts['passed']} | {counts['failed']} | {counts['ignored']} "
            f"| {build.wall_s if build else ''} | {test.wall_s if test else ''} | {cell(against)} |"
        )
    for p in result.projects:
        lines += ["", f"## {p.name} {p.tag}".rstrip(), ""]
        lines.append(
            f"`{p.repo}` at `{p.tree.rev}` (tree `{p.tree.tree}`), files BLAKE3 `{p.tree.digest[:16]}`, "
            f"Cargo.lock sha256 `{p.tree.lock_sha256[:16]}`; on {p.host.system} {p.host.machine} ({p.host.where})."
        )
        lines.append("")
        lines.append(
            f"Tests ran sandboxed (`{SANDBOXED_TESTS}`, added to the copy's Cargo.toml; files BLAKE3 before the edit "
            f"`{p.tree.digest_before_sandbox[:16]}`)." if p.sandbox_tests else "Tests ran unsandboxed."
        )
        lines.append("")
        lines.append("Steps: " + ", ".join(
            f"{s.step} {'did not finish' if s.exit is None else f'exit {s.exit}'} in {s.wall_s} s" for s in p.steps))
        for s in p.steps:
            if s.error:
                lines += ["", f"The harness could not finish `{s.step}` (log `{s.log}`):", "", "```", s.error[-TAIL:], "```"]
        if p.build_failures:
            units = len({f.where for f in p.build_failures})
            lines += ["", (f"Build failures ({len(p.build_failures)} errors in {units} units, "
                           f"{p.units_held_back} units held back behind them):"),
                      "", "| Errors | blaze's error | Units |", "|---:|---|---|"]
            lines += [f"| {n} | {cell(key)} | {cell(', '.join(where))} |" for key, n, where in clusters(p.build_failures, 10)]
        if p.test_failures:
            lines += ["", f"Test failures ({len(p.test_failures)}):", "", "| Tests | Failure | Tests |", "|---:|---|---|"]
            lines += [f"| {n} | {cell(key)} | {cell(', '.join(where))} |" for key, n, where in clusters(p.test_failures, 10)]
        c = compared.get(p.name)
        listed = (("Newly failing", c.broke), ("Passed before, did not run", c.gone), ("Fixed", c.fixed),
                  ("Units newly failing", c.units_broke), ("Units fixed", c.units_fixed),
                  ("Test executables newly failing", c.exes_broke), ("Test executables fixed", c.exes_fixed)
                  ) if c is not None and c.compared else ()
        if any(names for _, names in listed):
            lines.append("")
            for label, names in listed:
                if names:
                    shown = ", ".join(f"`{n}`" for n in names[:20])
                    lines.append(f"- {label} ({len(names)}): {shown}{', ...' if len(names) > 20 else ''}")
    return "\n".join(lines) + "\n"


def encoded(result: Result) -> str:
    return json.dumps(dataclasses.asdict(result), indent=1, sort_keys=False) + "\n"


def load_result(path: str) -> Result:
    return parse(Result, path, SCRIPT)


def finish(result: Result, out: Path, baseline: str, write_baseline: str, strict: bool) -> int:
    """Writes result.json and report.md, compares with `baseline`, copies the result to
    `write_baseline`: the exit status."""
    before = {p.name: p for p in load_result(baseline).projects} if baseline else {}
    compared = {p.name: changes(p, before.get(p.name)) for p in result.projects} if baseline else {}
    out.mkdir(parents=True, exist_ok=True)
    (out / "result.json").write_text(encoded(result))
    (out / "report.md").write_text(report(result, compared, baseline))
    if write_baseline:
        Path(write_baseline).parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(out / "result.json", write_baseline)
    print(f"{SCRIPT}: wrote {out / 'result.json'} and {out / 'report.md'}", flush=True)
    errored = [p.name for p in result.projects if p.verdict == "error"]
    broke = [name for name, c in compared.items() if newly_broken(c)]
    failed = [p.name for p in result.projects if p.verdict != "pass"]
    if errored:
        print(f"{SCRIPT}: the harness could not finish {', '.join(errored)}", flush=True)
    if broke:
        print(f"{SCRIPT}: newly broken against the baseline: {', '.join(broke)}", flush=True)
    return 1 if errored or broke or (strict and failed) else 0


# ---------------------------------------------------------------------------------------------
# run and merge


def chosen(corpus: pins.Corpus, names: Sequence[str]) -> list[Workspace]:
    known = {w.name: w for w in corpus.workspaces}
    if unknown := sorted(set(names) - set(known)):
        fail(SCRIPT, f"projects: corpus.json has no workspace {', '.join(unknown)} (it has {', '.join(known)})")
    return [known[n] for n in dict.fromkeys(names)] if names else list(corpus.workspaces)


async def run_all(args: Run) -> int:
    if args.jobs < 1:
        fail(SCRIPT, f"jobs: at least 1, not {args.jobs}")
    workspaces = chosen(pins.load_corpus(script=SCRIPT), args.projects)
    out = Path(args.out).resolve()
    out.mkdir(parents=True, exist_ok=True)
    try:
        work = Path(storelib.work_dir(args.work, "bz-realprojects-"))
    except storelib.Failed as e:
        fail(SCRIPT, str(e))
    work.mkdir(parents=True, exist_ok=True)
    sources = Path(args.sources).expanduser().resolve() if args.sources else SOURCES
    sources.mkdir(parents=True, exist_ok=True)
    started = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())
    print(f"{SCRIPT}: work {work}, out {out}", flush=True)
    gate = asyncio.Semaphore(args.jobs)
    # The longest first (Graham's LPT rule), so no big build runs alone at the end; the result
    # keeps corpus.json's order.
    order = sorted(range(len(workspaces)), key=lambda i: -workspaces[i].minutes)
    try:
        if args.where == "ix":
            results = await vm_runs(workspaces, order, args, out, work, gate)
            return merge_results(results, out, args.baseline, args.write_baseline, args.strict,
                                 [w.name for w in workspaces])
        try:
            blaze, identity = await blaze_of(args.blaze, work)
        except StepFailed as e:
            fail(SCRIPT, str(e))
        done: dict[int, ProjectResult] = {}

        async def one(i: int) -> None:
            async with gate:
                done[i] = await run_project(workspaces[i], args, blaze, work, out, sources)

        async with asyncio.TaskGroup() as group:
            for i in order:
                group.create_task(one(i))
        if args.home == "shared":
            env = blaze_env(work / "home", ())
            await run("stopping the daemon", [str(blaze), "stop"], cwd=work, env=env, minutes=5, check=False)
        result = Result(SCHEMA, started, identity, tuple(done[i] for i in range(len(workspaces))))
        return finish(result, out, args.baseline, args.write_baseline, args.strict)
    finally:
        if not args.keep:
            shutil.rmtree(work, ignore_errors=True)


async def vm_runs(workspaces: list[Workspace], order: list[int], args: Run, out: Path, work: Path,
                  gate: asyncio.Semaphore) -> list[str]:
    """Each workspace in a VM of its own; the result.json of each, a VM that sent none back
    written as a result of its own whose project is an error."""
    paths: dict[int, str] = {}

    async def one(i: int) -> None:
        ws = workspaces[i]
        async with gate:
            try:
                paths[i] = str(await run_in_vm(ws, args, out, work))
            except (StepFailed, OSError) as e:
                lone = out / "vm" / ws.name / "result.json"
                lone.parent.mkdir(parents=True, exist_ok=True)
                identity = Blaze("", "", "", f"the release {args.vm.blaze or 'of .github/blaze-version'}")
                result = Result(SCHEMA, "", identity, (missing(ws, "vm", str(e), f"ix {args.vm.arch}"),))
                lone.write_text(encoded(result))
                paths[i] = str(lone)

    async with asyncio.TaskGroup() as group:
        for i in order:
            group.create_task(one(i))
    return [paths[i] for i in range(len(workspaces))]


def merge_results(paths: Sequence[str], out: Path, baseline: str, write_baseline: str, strict: bool,
                  expected: Sequence[str] = ()) -> int:
    """One result of several, each one's logs copied under `out`. Each project of `expected` that
    no result has (its job was killed before it wrote one) is an error, never left out. Results
    of two blazes are refused: one report is the verdict on one blaze."""
    results = [(Path(p).resolve(), load_result(p)) for p in paths]
    if not results and not expected:
        fail(SCRIPT, "results: none given")
    out = out.resolve()
    out.mkdir(parents=True, exist_ok=True)
    measured = {r.blaze.blake3 for _, r in results if r.blaze.blake3}
    if len(measured) > 1:
        fail(SCRIPT, f"results: of {len(measured)} blazes ({', '.join(sorted(h[:12] for h in measured))}); "
                     "a report is the verdict on one")
    projects: list[ProjectResult] = []
    seen: set[str] = set()
    for path, r in results:
        if path.parent != out and (path.parent / "logs").is_dir():
            shutil.copytree(path.parent / "logs", out / "logs", dirs_exist_ok=True)
        for p in r.projects:
            if p.name in seen:
                fail(SCRIPT, f"results: {p.name} is in two of them")
            seen.add(p.name)
            projects.append(p)
    known = {w.name: w for w in pins.load_corpus(script=SCRIPT).workspaces}
    for name in expected:
        if name not in seen:
            ws = known.get(name)
            if ws is None:
                fail(SCRIPT, f"expected: corpus.json has no workspace {name}")
            projects.append(missing(ws, "vm" if (out / "logs" / name / "vm.log").is_file() else "test",
                                    "no result.json came back for it (its job or VM ended before the harness wrote one)"))
    blank = Blaze("", "", "", "")
    identity = next((r.blaze for _, r in results if r.blaze.blake3), results[0][1].blaze if results else blank)
    started = min((r.started for _, r in results if r.started), default="")
    return finish(Result(SCHEMA, started, identity, tuple(projects)), out, baseline, write_baseline, strict)


# ---------------------------------------------------------------------------------------------
# selftest

TRANSCRIPT = """\
{"reason":"compiler-artifact","package_id":"path+file:///w#a@0.1.0","target":{"kind":["lib"],"name":"a"},"executable":"/r/build/w-1/debug/deps/a-0123456789abcdef"}
{"reason":"compiler-artifact","package_id":"path+file:///w#b@0.1.0","target":{"kind":["lib"],"name":"b"}}
{"reason":"compiler-message","package_id":"path+file:///w/c#0.1.0","target":{"kind":["lib"],"name":"c"},"message":{"level":"error","code":{"code":"E0308"},"message":"mismatched types at 17","rendered":"error[E0308]: mismatched types\\n --> src/lib.rs:17:5"}}
{"reason":"compiler-message","package_id":"path+file:///w/c#0.1.0","target":{"kind":["lib"],"name":"c"},"message":{"level":"error","code":null,"message":"aborting due to 1 previous error","rendered":""}}
{"reason":"blaze-unchecked","stopped":false,"behind":[{"unit":"c (lib)","why":"failed","units":["d (lib)","e (bin)"]}],"open":[],"tests":[{"unit":"c (test \\"t\\")","behind":"c (lib)","why":"its library failed"}]}
error: blaze cannot compile `core::intrinsics::foo` in `b::bar`: no lowering for 3 operands
     Running unittests src/lib.rs (/r/build/w-1/debug/deps/a-0123456789abcdef)

running 4 tests
test x::passes ... ok
test x::ignored ... ignored
test x::prints ... hello from a child
ok
test x::fails ... FAILED

failures:

---- x::fails stdout ----
thread 'x::fails' (42) panicked at src/lib.rs:9:5:
assertion `left == right` failed
  left: 1
 right: 2
     Running unittests src/lib.rs (/tmp/inner/debug/deps/inner-0123456789abcdef)
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s
note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace


failures:
    x::fails

test result: FAILED. 2 passed; 1 failed; 1 ignored; 0 measured; 0 filtered out; finished in 0.01s

error: test failed, to rerun pass `-p a --lib`
     Running tests/it.rs (/r/build/w-1/debug/deps/it-fedcba9876543210)

running 2 tests
test one ... ok
test two ... error: test failed, to rerun pass `-p a --test it`
Caused by:
  process didn't exit successfully: `/r/build/w-1/debug/deps/it-fedcba9876543210` (signal: 11, SIGSEGV: invalid memory reference)
   Doc-tests a

running 1 test
test src/lib.rs - f (line 3) ... ok

test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.20s
"""


def read_transcript(text: str) -> Reader:
    reader = Reader()
    for line in text.splitlines(keepends=True):
        reader.feed(line)
    return reader.finish()


def test_reader() -> None:
    r = read_transcript(TRANSCRIPT)
    assert r.units_built == 2, r.units_built
    assert r.units_held_back == 2, r.units_held_back
    units = [(f.where, f.cluster) for f in r.build_failures]
    assert units == [("c (lib c)", "E0308 mismatched types at N"),
                     ("(the build)", "blaze cannot compile `core::intrinsics::foo` in `b::bar`: no lowering for N operands")], units
    got = {(t.exe, t.name): t.outcome for t in r.tests}
    assert got == {
        ("a/a (unittests src/lib.rs)", "x::passes"): "passed",
        ("a/a (unittests src/lib.rs)", "x::ignored"): "ignored",
        ("a/a (unittests src/lib.rs)", "x::prints"): "passed",
        ("a/a (unittests src/lib.rs)", "x::fails"): "failed",
        ("it (tests/it.rs)", "one"): "passed",
        ("it (tests/it.rs)", "two"): "failed",
        ("a (doc-tests)", "src/lib.rs - f (line 3)"): "passed",
    }, got
    failures = [(f.where, f.cluster) for f in r.test_failures]
    assert failures == [
        ('c (test "t")', "never built: its library failed"),
        ("a/a (unittests src/lib.rs) :: x::fails", "panic: assertion `left == right` failed"),
        ("it (tests/it.rs) :: two", "failed, printing nothing"),
        ("it (tests/it.rs)", "crashed: signal: N, SIGSEGV: invalid memory reference"),
    ], failures


def test_normalized() -> None:
    assert normalized("error at /a/b/c.rs:12:3 in 0xdeadbeef") == "error at <path>:N:N in 0x…"
    assert normalized("`foo::<u8>` has 3 fields\nsecond line") == "`foo::<u8>` has N fields"
    assert normalized("hash 0123456789abcdef0123 here") == "hash <hash> here"
    assert normalized("") == ""
    assert package_name("path+file:///w#a@0.1.0") == "a"
    assert package_name("path+file:///w/c#0.1.0") == "c"
    assert package_name("registry+https://github.com/rust-lang/crates.io-index#serde@1.0.0") == "serde"
    assert package_name("serde 1.0.0 (registry+https://...)") == "serde"
    exe = "/x/deps/nu_path-0123456789abcdef"
    assert exe_label("unittests src/lib.rs", exe, {}) == "nu_path (unittests src/lib.rs)"
    assert exe_label("unittests src/lib.rs", exe, {Path(exe).name: "nu-path"}) == "nu-path/nu_path (unittests src/lib.rs)"


def project(name: str, tests: Sequence[tuple[str, Outcome]], units: Sequence[str] = (), rev: str = "r" * 40,
            lock: str = "l") -> ProjectResult:
    return ProjectResult(
        name, "o/" + name, "v1", Tree(rev, "t" * 40, "d", lock), Host("Linux", "x86_64", 8, "local"), "fail",
        (StepResult("build", ("blaze", "build"), 0, 1.0, "logs/x/build.log", ""),), 3, 0,
        tuple(Failure(u, "E0308 x", "") for u in units), tuple(TestOutcome("e", n, o) for n, o in tests),
        tuple(Failure(f"e :: {n}", "panic: x", "") for n, o in tests if o == "failed"),
    )


def test_changes() -> None:
    before = project("p", [("a", "passed"), ("b", "failed"), ("c", "passed"), ("d", "ignored")], ["u1"])
    now = project("p", [("a", "failed"), ("b", "passed"), ("d", "ignored"), ("e", "passed")], ["u2"])
    c = changes(now, before)
    assert c.compared and c.broke == ("e :: a",) and c.fixed == ("e :: b",) and c.gone == ("e :: c",), c
    assert c.new_tests == 1 and c.units_broke == ("u2",) and c.units_fixed == ("u1",), c
    crashed = dataclasses.replace(before, test_failures=(*before.test_failures, Failure("e2", "crashed: x", "")))
    c = changes(crashed, before)
    assert c.exes_broke == ("e2: crashed: x",) and newly_broken(c), c
    assert changes(before, crashed).exes_fixed == ("e2: crashed: x",)
    assert newly_broken(c)
    assert not newly_broken(changes(before, before))
    assert changes(now, None).why == "not in the baseline"
    assert "pin moved" in changes(project("p", [], rev="s" * 40), before).why
    assert changes(project("p", [], lock="m"), before).why == "the lock moved"


def test_round_trip() -> None:
    result = Result(SCHEMA, "2026-11-03T00:00:00Z", Blaze("/b", "h" * 64, "", "blaze 0.1"),
                    (project("p", [("a", "passed"), ("b", "failed")], ["u"]),))
    with tempfile.TemporaryDirectory() as tmp:
        path = Path(tmp) / "result.json"
        path.write_text(encoded(result))
        assert load_result(str(path)) == result, "result.json does not read back as it was written"
        out = Path(tmp) / "merged"
        status = merge_results([str(path)], out, str(path), "", False)
        merged = load_result(str(out / "result.json"))
        assert status == 0 and merged.projects[0].steps[0].log == "logs/x/build.log", merged.projects[0].steps
        assert merge_results([str(path)], Path(tmp) / "m2", "", "", False, ("p", "ripgrep")) == 1, "a missing project passed"
        gone = load_result(str(Path(tmp) / "m2" / "result.json")).projects[1]
        assert (gone.name, gone.verdict) == ("ripgrep", "error"), gone
        text = (out / "report.md").read_text()
        assert "| p |" in text and "E0308 x" in text and "panic: x" in text, text


def test_corpus() -> None:
    corpus = pins.load_corpus(script=SCRIPT)
    names = [w.name for w in corpus.workspaces]
    assert names[0] == "blaze", "blaze comes first: blaze builds blaze"
    assert len(set(names)) == len(names), "a workspace is listed twice"
    assert len(names) >= 10, names
    for w in corpus.workspaces[1:]:
        assert re.fullmatch(r"[\w.-]+/[\w.-]+", w.repo), w
        assert re.fullmatch(r"[0-9a-f]{40}", w.rev) and re.fullmatch(r"[0-9a-f]{40}", w.tree), w
        assert w.minutes > 0 and w.why, w
    blaze = corpus.workspaces[0]
    assert blaze.repo == "self" and blaze.rev == "" and blaze.tree == "" and blaze.lock == "committed", blaze
    # One pin per project across the harnesses: where blaze_cli.py pins the same project, the same
    # commit (its `cargo-lib` is the whole of cargo's workspace).
    import importlib
    cli = cast(dict[str, dict[str, object]], importlib.import_module("blaze_cli").PROJECTS)
    for w in corpus.workspaces[1:]:
        other = cli.get("cargo-lib" if w.name == "cargo" else w.name)
        if other is not None:
            assert other["rev"] == w.rev, f"{w.name}: corpus.json pins {w.rev}, blaze_cli.py {other['rev']}"


def test_input() -> None:
    run_ = parse(Run, '{"out": "/tmp/x", "projects": ["ripgrep"], "vm": {"cpus": 8}}', SCRIPT)
    assert run_.projects == ("ripgrep",) and run_.vm.cpus == 8 and run_.vm.arch == "x86_64", run_
    try:
        parse(Run, '{"out": "/tmp/x", "project": ["ripgrep"]}', SCRIPT)
    except SystemExit as e:
        assert "no field project" in str(e.code), e.code
    else:
        raise AssertionError("an unknown field was taken")


def test_sandboxed_manifest() -> None:
    def experimental(text: str) -> object:
        return tomllib.loads(text)["workspace"]["metadata"]["blaze"]["experimental"]

    bare = '[package]\nname = "a"\n\n[workspace]\nmembers = ["b"]\n'
    assert experimental(sandboxed_manifest(bare, "x")) == ["sandboxed-tests"]
    table = bare + '\n[workspace.metadata.blaze]  # ours\nconst-macros = 1\n\n[profile.dev]\nopt-level = 1\n'
    edited = sandboxed_manifest(table, "x")
    assert experimental(edited) == ["sandboxed-tests"] and tomllib.loads(edited)["profile"]["dev"]["opt-level"] == 1, edited
    listed = bare + '\n[workspace.metadata.blaze]\nexperimental = ["const-macros"] # x\n'
    assert experimental(sandboxed_manifest(listed, "x")) == ["sandboxed-tests", "const-macros"]
    already = bare + '\n[workspace.metadata.blaze]\nexperimental = ["sandboxed-tests"]\n'
    assert sandboxed_manifest(already, "x") == already
    for refused in ('[package]\nname = "a"\n', bare + 'metadata.blaze = { sandbox = {} }\n',
                    bare + '\n[workspace.metadata.blaze]\nexperimental = "const-macros"\n',
                    bare + '\n[workspace.metadata.blaze]\nexperimental = [\n  "const-macros",\n]\n'):
        try:
            sandboxed_manifest(refused, "the/Cargo.toml")
        except StepFailed as e:
            assert "the/Cargo.toml" in str(e) and "sandboxed-tests" in str(e), e
        else:
            raise AssertionError(f"edited a manifest it cannot merge into:\n{refused}")


def test_tree_digest() -> None:
    with tempfile.TemporaryDirectory() as tmp:
        root = Path(tmp)
        (root / "src").mkdir()
        (root / "src" / "lib.rs").write_text("fn a() {}\n")
        (root / ".git").mkdir()
        (root / ".git" / "HEAD").write_text("x\n")
        first = tree_digest(root)
        (root / ".git" / "HEAD").write_text("y\n")
        assert tree_digest(root) == first, ".git is not the tree"
        (root / "src" / "lib.rs").write_text("fn b() {}\n")
        assert tree_digest(root) != first, "an edit is"


SELFTESTS = (test_reader, test_normalized, test_changes, test_round_trip, test_corpus, test_input, test_sandboxed_manifest,
             test_tree_digest)


def selftest() -> int:
    failed = 0
    for test in SELFTESTS:
        try:
            test()
            print(f"ok    {test.__name__}", flush=True)
        except (Exception, SystemExit) as e:  # noqa: BLE001 - a selftest names every check that failed
            failed += 1
            print(f"FAIL  {test.__name__}: {type(e).__name__}: {e}", flush=True)
    print(f"{SCRIPT} selftest: {len(SELFTESTS) - failed} of {len(SELFTESTS)} pass", flush=True)
    return 1 if failed else 0


def main() -> None:
    match sys.argv[1:]:
        case ["selftest"]:
            sys.exit(selftest())
        case ["run", text]:
            sys.exit(asyncio.run(run_all(parse(Run, text, SCRIPT))))
        case ["merge", text]:
            it = parse(Merge, text, SCRIPT)
            sys.exit(merge_results(it.results, Path(it.out), it.baseline, it.write_baseline, it.strict, it.expected))
        case _:
            fail(SCRIPT, f"usage: {SCRIPT} run JSON | merge JSON | selftest\n\n{__doc__}")


if __name__ == "__main__":
    main()
