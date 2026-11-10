#!/usr/bin/env python3
"""failures.py: a test run held to harness/known-failures.toml, the one list of tests that may fail.

usage: uv run --frozen harness/failures.py CONFIG

CONFIG is one JSON object, inline, the path of a file, or `-` for standard input
(AGENTS.md rule scripts-take-typed-input). Either it compares logs already written:

    {"results": ["<log>", ...], "profile": "dev" | "release", "os": "linux" | "macos" | "windows",
     "tail": 20, "report": "<path>"}

or it runs `blaze test --no-fail-fast` of the packages named (or of the whole workspace), writes
its log, and compares that:

    {"run": {"packages": ["hir-def", ...], "workspace": false, "release": false, "args": [],
             "timeout_s": 3600}, "tail": 20, "report": "<path>"}

A log is what `blaze test` prints, stdout and stderr together, in libtest's text format or its
JSON one (`-- -Z unstable-options --format json`). A test is named `<binary>::<path>`, the binary
as the `Running` line names it with `-` as `_`.

The list is the only exemption there is. The run fails on:
- a test that fails and is not listed (a new failure);
- a listed test that passes, is ignored, or no longer exists in a binary that ran in full (the
  list only shrinks: take it out);
- a test binary that did not finish (a crash, an abort, a stack overflow) and is not listed as
  `<binary>::*`: whatever it had left to run is unchecked, so it never passes quietly; and one
  listed so that finishes;
- a package asked for none of whose test binaries ran (its test build failed, most likely) and
  is not listed as `*`; and one listed so whose tests run;
- a list that does not hold together (an entry without a cause, a test of a binary its crate
  does not have).
Every failure names the crate, the test and the tail of its output. Listed tests this run could
not check (their crate did not run, they were filtered out or unreached, or the entry is `only`
for another profile or OS) are counted, so a partial run never reads as clean.

`record` (harness/unittests.py's `{"record": true}`) writes the list again from a run: see
`recorded`. The comparison is stdlib-only, so harness/unittests.py imports it under CI's plain
python3.
"""

import asyncio
import glob
import json
import os
import re
import subprocess
import sys
import time
import tomllib
from collections.abc import Iterable
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Literal, cast

ROOT = Path(__file__).resolve().parent.parent
BASELINE = ROOT / "harness" / "known-failures.toml"
type Profile = Literal["dev", "release"]
type Os = Literal["linux", "macos", "windows"]

COLOR = re.compile(r"\x1b\[[0-9;]*m")
# The binary of a `Running` line: `Running unittests src/lib.rs (target/debug/deps/x-<hash>)`,
# the build directory's second layout `build/<package>/<hash>/out/x-<hash>`, and
# `Running target/debug/deps/x-<hash>`.
BINARY = re.compile(r"[/\\]([\w-]+?)-[0-9a-f]{8,}(?:\.exe)?\b")
RUNNING = re.compile(r"^\s+Running\s")
DOCTESTS = re.compile(r"^\s+Doc-tests (\S+)")
RESULT = re.compile(r"^test (.+?) \.\.\. (ok|FAILED|ignored)\b")
SUMMARY = re.compile(r"^test result: (ok|FAILED)\.")
FILTERED = re.compile(r"(\d+) filtered out")
UNREACHED = re.compile(r"(\d+) unreached")
OUTPUT = re.compile(r"^---- (.+) stdout ----$")
# The line after a test binary that failed or crashed; what follows is not the binary's.
RERUN = re.compile(r"^error: test failed, to rerun pass ")
EXITED = re.compile(r"process didn't exit successfully: `([^`]*)` \((.*)\)")
LIBTEST_JSON = re.compile(r'^\{\s*"type":\s*"(?:test|suite)"')


class Refused(Exception):
    """A config, a log or a baseline that cannot be read, naming what and where."""


# ---------------------------------------------------------------------------------------------
# The results of a run, read from its log.


@dataclass
class Test:
    binary: str
    name: str
    verdict: Literal["ok", "failed", "ignored"]
    output: list[str] = field(default_factory=list[str])

    @property
    def key(self) -> str:
        return f"{self.binary}::{self.name}"


@dataclass
class Binary:
    name: str
    log: str
    finished: bool = False
    filtered: int = 0
    unreached: int = 0
    exit: str | None = None
    # The last lines said while it ran: what a crash said.
    lines: list[str] = field(default_factory=list[str])


@dataclass
class Results:
    tests: dict[str, Test] = field(default_factory=dict[str, Test])
    binaries: dict[str, Binary] = field(default_factory=dict[str, Binary])


def binary_name(text: str) -> str | None:
    """The test binary a `Running` line or a command names: its last `name-<hash>`."""
    found = BINARY.findall(text)
    return found[-1].replace("-", "_") if found else None


class Reader:
    """Reads one log line by line into `Results`."""

    KEEP = 200

    def __init__(self, results: Results, log: str) -> None:
        self.results, self.log = results, log
        self.binary: Binary | None = None
        self.capturing: Test | None = None

    def start(self, name: str) -> None:
        # A binary named twice (two logs, or a retry) is one: its last run stands.
        self.binary = self.results.binaries[name] = Binary(name, self.log)
        self.capturing = None

    def feed(self, raw: str) -> None:
        line = COLOR.sub("", raw.rstrip("\r\n"))
        if line.startswith('{"at_ns"'):
            # blaze's `--events json`: the build's events, not the tests'.
            return
        if LIBTEST_JSON.match(line):
            self.json(line)
            return
        if m := DOCTESTS.match(line):
            self.start(m.group(1).replace("-", "_"))
            return
        if RUNNING.match(line):
            # blaze names a doc test's executable `<lib>-doctests` after its `Doc-tests` line.
            name = binary_name(line)
            if name is not None:
                self.start(name)
            return
        if m := EXITED.search(line):
            name = binary_name(m.group(1))
            binary = self.results.binaries.get(name) if name else None
            if binary is not None:
                binary.exit = m.group(2)
            return
        binary = self.binary
        if binary is None:
            return
        if RERUN.match(line):
            self.binary = self.capturing = None
            return
        binary.lines.append(line)
        if len(binary.lines) > 2 * self.KEEP:
            del binary.lines[: self.KEEP]
        if m := RESULT.match(line):
            verdict = {"ok": "ok", "FAILED": "failed", "ignored": "ignored"}[m.group(2)]
            test = Test(binary.name, m.group(1), verdict)  # pyright: ignore[reportArgumentType]
            self.results.tests[test.key] = test
            return
        if m := OUTPUT.match(line):
            self.capturing = self.results.tests.get(f"{binary.name}::{m.group(1)}")
            return
        if line == "failures:":
            self.capturing = None
            return
        if SUMMARY.match(line):
            self.capturing = None
            binary.finished = True
            if m := FILTERED.search(line):
                binary.filtered = int(m.group(1))
            if m := UNREACHED.search(line):
                binary.unreached = int(m.group(1))
            return
        if self.capturing is not None:
            self.capturing.output.append(line)

    def json(self, line: str) -> None:
        try:
            event = json.loads(line)
        except json.JSONDecodeError as e:
            raise Refused(f"{self.log}: a libtest JSON line that is not JSON ({e}): {line[:200]}") from e
        binary = self.binary
        if binary is None:
            raise Refused(
                f"{self.log}: a libtest JSON line before any `Running` line names its binary: {line[:200]}"
            )
        kind, what = event.get("type"), event.get("event")
        if kind == "suite" and what in ("ok", "failed"):
            binary.finished = True
            binary.filtered = int(event.get("filtered_out", 0))
        elif kind == "test" and what in ("ok", "failed", "ignored"):
            test = Test(binary.name, str(event["name"]), what)
            test.output = str(event.get("stdout", "")).splitlines()
            self.results.tests[test.key] = test


def read_logs(paths: Iterable[Path]) -> Results:
    """The results of the logs `paths`."""
    results = Results()
    for path in paths:
        if not path.is_file():
            raise Refused(f"the log {path} does not exist")
        reader = Reader(results, str(path))
        with path.open(encoding="utf-8", errors="replace") as f:
            for line in f:
                reader.feed(line)
    return results


def _members(root: Path) -> list[tuple[Path, dict[str, Any]]]:
    """The workspace's member directories with their manifests."""
    manifest = tomllib.loads((root / "Cargo.toml").read_text())
    workspace = manifest.get("workspace", {})
    excluded = {os.path.normpath(it) for it in workspace.get("exclude", [])}
    members: list[tuple[Path, dict[str, Any]]] = []
    for pattern in workspace.get("members", []):
        for found in sorted(glob.glob(str(root / pattern))):
            rel = os.path.normpath(os.path.relpath(found, root))
            if rel not in excluded and (Path(found) / "Cargo.toml").is_file():
                members.append((Path(found), tomllib.loads((Path(found) / "Cargo.toml").read_text())))
    return members


def workspace_packages(root: Path = ROOT) -> list[str]:
    """The names of the workspace's packages, sorted: what `blaze test --workspace` tests."""
    names = [toml.get("package", {}).get("name") for _, toml in _members(root)]
    return sorted(name for name in names if isinstance(name, str))


# ---------------------------------------------------------------------------------------------
# The workspace's test binaries, by package: which crate a failing binary belongs to.


def _targets(member: Path, toml: dict[str, Any], kinds: tuple[tuple[str, str, str], ...]) -> list[str]:
    """The names (`-` as `_`) of `member`'s targets of `kinds` (`(table, auto key, directory)`),
    and its library's and `src/main.rs`'s; one whose table says `test = false` is left out."""
    package = toml["package"]["name"]
    names: list[str] = []

    def add(name: str) -> None:
        name = name.replace("-", "_")
        if name not in names:
            names.append(name)

    lib = toml.get("lib", {})
    if lib.get("test", True) and ((member / "src" / "lib.rs").is_file() or "path" in lib):
        add(lib.get("name", package))
    for kind, auto, directory in kinds:
        declared = toml.get(kind, [])
        off = {target["name"] for target in declared if "name" in target and not target.get("test", True)}
        for target in declared:
            if "name" in target and target["name"] not in off:
                add(target["name"])
        if toml.get("package", {}).get(auto, True):
            found = [path.stem for path in sorted((member / directory).glob("*.rs"))]
            found += [path.parent.name for path in sorted((member / directory).glob("*/main.rs"))]
            for name in found:
                if name not in off:
                    add(name)
    if (member / "src" / "main.rs").is_file() and package not in {
        target["name"] for target in toml.get("bin", []) if "name" in target and not target.get("test", True)
    }:
        add(package)
    return names


TESTED = (("test", "autotests", "tests"), ("bin", "autobins", "src/bin"))
BUILT = (*TESTED, ("bench", "autobenches", "benches"))


def workspace_targets(root: Path = ROOT) -> dict[str, list[str]]:
    """Each test binary name (`-` as `_`) the workspace's members build, to their packages."""
    targets: dict[str, list[str]] = {}
    for member, toml in _members(root):
        package = toml.get("package", {}).get("name")
        if not isinstance(package, str):
            continue
        names = _targets(member, toml, BUILT)
        # A library `test = false` still has doc tests, named after it.
        lib = str(toml.get("lib", {}).get("name", package)).replace("-", "_")
        for name in [*names, lib]:
            owners = targets.setdefault(name, [])
            if package not in owners:
                owners.append(package)
    return targets


def test_binaries(root: Path = ROOT) -> dict[str, list[str]]:
    """Each package's test binaries as `blaze test` runs them (its library's unit tests, its
    binaries' and its integration tests'): a package none of these ran for was not tested."""
    binaries: dict[str, list[str]] = {}
    for member, toml in _members(root):
        package = toml.get("package", {}).get("name")
        if isinstance(package, str):
            binaries[package] = _targets(member, toml, TESTED)
    return binaries


# ---------------------------------------------------------------------------------------------
# The list.

OSES: tuple[Os, ...] = ("linux", "macos", "windows")
PROFILES: tuple[Profile, ...] = ("dev", "release")
ENTRY_KEYS = {"cause", "only"}
ONLY_KEYS = {"profile", "os"}
# `[crates.<package>] "*"`: no test binary of the package runs (its test build fails).
NOT_RUN = "*"
# `[crates.<package>] "<binary>::*"`: the binary does not finish.
UNFINISHED = "::*"


@dataclass(frozen=True)
class Context:
    profile: Profile
    os: Os


@dataclass(frozen=True)
class Entry:
    crate: str
    test: str
    cause: str
    # The profile it holds for; None is both.
    profile: Profile | None = None
    # The OSes it holds for, sorted; None is all.
    oses: tuple[Os, ...] | None = None

    @property
    def binary(self) -> str:
        return self.test.split("::", 1)[0]

    @property
    def not_run(self) -> bool:
        return self.test == NOT_RUN

    @property
    def unfinished(self) -> bool:
        return self.test.endswith(UNFINISHED) and self.test.count("::") == 1

    def applies(self, context: Context) -> bool:
        return self.profile in (None, context.profile) and (self.oses is None or context.os in self.oses)

    @property
    def only(self) -> str:
        said = ([f"profile {self.profile}"] if self.profile else []) + (
            [f"os {' or '.join(self.oses)}"] if self.oses is not None else []
        )
        return ", ".join(said)


def load_baseline(path: Path, targets: dict[str, list[str]]) -> list[Entry]:
    """The entries of `path`, every problem with it named at once."""
    try:
        data: dict[str, object] = tomllib.loads(path.read_text())
    except (OSError, tomllib.TOMLDecodeError) as e:
        raise Refused(f"{path}: {e}") from e
    problems: list[str] = []
    entries: list[Entry] = []
    unknown = sorted(set(data) - {"recorded", "crates"})
    if unknown:
        problems.append(f"top-level tables {', '.join(unknown)} are not `recorded` or `crates`")
    crates = data.get("crates", {})
    if not isinstance(crates, dict):
        raise Refused(f"{path}: `crates` is a table of crates")
    packages = {package for owners in targets.values() for package in owners}
    for crate, tests in sorted(cast(dict[str, object], crates).items()):
        if not isinstance(tests, dict):
            problems.append(f"[crates.{crate}] is a table of tests")
            continue
        if crate not in packages:
            problems.append(f"[crates.{crate}] is no package of the workspace")
        for test, raw in sorted(cast(dict[str, object], tests).items()):
            where = f'[crates.{crate}] "{test}"'
            if not isinstance(raw, dict):
                problems.append(f"{where} is an inline table {{ cause }}")
                continue
            entry = cast(dict[str, object], raw)
            if extra := sorted(set(entry) - ENTRY_KEYS):
                problems.append(f"{where} has {', '.join(extra)}, not one of {', '.join(sorted(ENTRY_KEYS))}")
            cause = entry.get("cause")
            if not isinstance(cause, str) or not cause.strip():
                problems.append(f"{where} has no cause")
            only_raw = entry.get("only", {})
            profile: Profile | None = None
            oses: tuple[Os, ...] | None = None
            if not isinstance(only_raw, dict):
                problems.append(f"{where}: `only` is a table")
            else:
                only = cast(dict[str, object], only_raw)
                if extra := sorted(set(only) - ONLY_KEYS):
                    problems.append(
                        f"{where}: `only` has {', '.join(extra)}, not one of {', '.join(sorted(ONLY_KEYS))}"
                    )
                if "profile" in only:
                    if only["profile"] == "dev":
                        profile = "dev"
                    elif only["profile"] == "release":
                        profile = "release"
                    else:
                        problems.append(
                            f"{where}: `only.profile = {only['profile']!r}` is not one of {PROFILES}"
                        )
                if "os" in only:
                    said = only["os"]
                    named = cast(list[object], said) if isinstance(said, list) else [said]
                    if named and all(it in OSES for it in named):
                        oses = tuple(sorted(set(cast(list[Os], named))))
                    else:
                        problems.append(f"{where}: `only.os = {only['os']!r}` is not one or a list of {OSES}")
            if test != NOT_RUN:
                if "::" not in test:
                    problems.append(f"{where} is not `<binary>::<test>`, `<binary>::*` or `*`")
                binary = test.split("::", 1)[0]
                if crate not in targets.get(binary, []):
                    owners = targets.get(binary)
                    said = (
                        f"belongs to {', '.join(owners)}" if owners else "is no test binary of the workspace"
                    )
                    problems.append(f"{where}: the binary `{binary}` {said}, not {crate}")
            entries.append(Entry(crate, test, str(cause), profile, oses))
    if problems:
        raise Refused(f"{path} does not hold together:\n  " + "\n  ".join(problems))
    return entries


HEADER = """\
# The Rust tests that may fail: the one exemption from the gate (docs/harness.md#ci).
#
# harness/failures.py holds a run to this list: a failing test that is not here fails the run,
# and so does a test here that passes, is ignored or no longer exists (take it out: the list only
# shrinks). harness/unittests.py, CI's test step, runs every test of the workspace and calls it
# on the log; a lane runs it on the crates it touched:
#
#   uv run --frozen harness/failures.py '{"run": {"packages": ["hir-def"]}}'
#
# One table a crate, `[crates.<package>]`, its entries sorted. An entry is one of:
#   "<binary>::<path>"  a test, by its full name as libtest names it under the binary the
#                       `Running` line names (`-` as `_`);
#   "<binary>::*"       the test binary does not finish (it crashes or aborts);
#   "*"                 no test binary of the package runs (its test build fails).
# Each has:
#   cause  one line: what fails and why, as far as it is known. `unittests.py` with
#          `{"record": true}` writes the first line of the failure where the entry is new, and
#          keeps the cause of an entry that was here;
#   only   optional, { profile = "dev"|"release", os = "linux"|"macos"|"windows" or a list of them }:
#          the entry holds only for such a run, and is unchecked by others.
# `[recorded]` is the last run this list was written from: python3 harness/unittests.py '{"record": true}'
"""


def _toml_string(text: str) -> str:
    return json.dumps(text, ensure_ascii=False)


def render(entries: Iterable[Entry], recorded: dict[str, str]) -> str:
    """The list as its file: the header, `[recorded]`, then each crate's entries, all sorted, so
    that the same entries are the same bytes."""
    out = [HEADER]
    if recorded:
        out.append("[recorded]")
        out += [f"{key} = {_toml_string(value)}" for key, value in sorted(recorded.items())]
        out.append("")
    by_crate: dict[str, list[Entry]] = {}
    for entry in entries:
        by_crate.setdefault(entry.crate, []).append(entry)
    for crate in sorted(by_crate):
        out.append(f"[crates.{crate}]")
        for entry in sorted(by_crate[crate], key=lambda it: it.test):
            only: list[str] = []
            if entry.profile is not None:
                only.append(f"profile = {_toml_string(entry.profile)}")
            if entry.oses is not None:
                oses = [_toml_string(it) for it in entry.oses]
                only.append(f"os = {oses[0]}" if len(oses) == 1 else f"os = [{', '.join(oses)}]")
            said = f"cause = {_toml_string(entry.cause)}" + (
                f", only = {{ {', '.join(only)} }}" if only else ""
            )
            out.append(f"{_toml_string(entry.test)} = {{ {said} }}")
        out.append("")
    return "\n".join(out)


# ---------------------------------------------------------------------------------------------
# The comparison.


@dataclass
class Verdict:
    new: list[Test] = field(default_factory=list[Test])
    fixed: list[tuple[Entry, str]] = field(default_factory=list[tuple[Entry, str]])
    gone: list[Entry] = field(default_factory=list[Entry])
    crashed: list[Binary] = field(default_factory=list[Binary])
    # Packages asked for, none of whose test binaries ran.
    not_run: list[str] = field(default_factory=list[str])
    known: list[Entry] = field(default_factory=list[Entry])
    unchecked: dict[str, list[Entry]] = field(default_factory=dict[str, list[Entry]])

    @property
    def failed(self) -> bool:
        return bool(self.new or self.fixed or self.gone or self.crashed or self.not_run)


def packages_not_run(results: Results, asked: Iterable[str], binaries: dict[str, list[str]]) -> list[str]:
    """The packages of `asked` that have test binaries, none of which ran."""
    return sorted(
        package
        for package in asked
        if binaries.get(package) and not any(name in results.binaries for name in binaries[package])
    )


def compare(
    entries: list[Entry],
    results: Results,
    context: Context,
    asked: Iterable[str] | None = None,
    binaries: dict[str, list[str]] | None = None,
) -> Verdict:
    """`results` held to `entries`. `asked` is the packages the run was of (None: not known, so
    no package is said not to have run), `binaries` each package's test binaries."""
    verdict = Verdict()
    binaries = binaries if binaries is not None else {}
    not_run = packages_not_run(results, asked, binaries) if asked is not None else []
    # A failure is excused only by an entry that holds for this run: one `only` for another OS
    # or profile excuses nothing here.
    listed = {entry.test for entry in entries if not entry.not_run and entry.applies(context)}
    excused_crashes: set[str] = set()
    excused_packages: set[str] = set()
    for entry in entries:
        if not entry.applies(context):
            verdict.unchecked.setdefault(f"only for {entry.only}", []).append(entry)
            continue
        if entry.not_run:
            if asked is None or entry.crate not in set(asked):
                verdict.unchecked.setdefault("its crate was not asked for", []).append(entry)
            elif entry.crate in not_run:
                excused_packages.add(entry.crate)
                verdict.known.append(entry)
            else:
                verdict.fixed.append((entry, "has test binaries that run"))
            continue
        binary = results.binaries.get(entry.binary)
        if entry.unfinished:
            if binary is None:
                verdict.unchecked.setdefault("its crate's binary did not run", []).append(entry)
            elif binary.finished:
                verdict.fixed.append((entry, "finishes"))
            else:
                excused_crashes.add(entry.binary)
                verdict.known.append(entry)
            continue
        test = results.tests.get(entry.test)
        if test is None:
            if binary is None:
                reason = "its crate's binary did not run"
            elif not binary.finished:
                reason = "its binary did not finish"
            elif binary.filtered:
                reason = "filtered out"
            elif binary.unreached:
                reason = "unreached (blaze ran only the tests the change reached)"
            else:
                verdict.gone.append(entry)
                continue
            verdict.unchecked.setdefault(reason, []).append(entry)
        elif test.verdict == "failed":
            verdict.known.append(entry)
        else:
            verdict.fixed.append((entry, "passes" if test.verdict == "ok" else "is ignored"))
    verdict.new = sorted(
        (t for t in results.tests.values() if t.verdict == "failed" and t.key not in listed),
        key=lambda t: t.key,
    )
    verdict.crashed = sorted(
        (b for b in results.binaries.values() if not b.finished and b.name not in excused_crashes),
        key=lambda b: b.name,
    )
    verdict.not_run = [package for package in not_run if package not in excused_packages]
    return verdict


def crate_of(binary: str, targets: dict[str, list[str]]) -> str:
    owners = targets.get(binary)
    if not owners:
        return f"(no workspace package builds a test binary `{binary}`)"
    return " or ".join(owners)


def tail(lines: list[str], count: int) -> list[str]:
    """The last `count` lines, without backtrace frames: the panic's message stays in view."""
    kept: list[str] = []
    frames = False
    for line in lines:
        if line.startswith("stack backtrace:"):
            frames = True
            kept.append("stack backtrace: (frames left out)")
        elif frames and (line.startswith("note: Some details are omitted") or not line.startswith(" ")):
            frames = False
            if not line.startswith("note: Some details are omitted"):
                kept.append(line)
        elif not frames:
            kept.append(line)
    lines = kept
    while lines and not lines[-1].strip():
        lines = lines[:-1]
    return lines[-count:] if lines else ["(it said nothing)"]


PANICKED = re.compile(r"^thread '.*' (?:\(\d+\) )?panicked at (.+?)(?::\d+:\d+)?:?$")
CAUSE_MOST = 240


def first_line(lines: list[str]) -> str:
    """The first line of a failure, for a new entry's cause: the panic's place (its file, no line
    or column, which every edit above it moves) and its message's first line, or the first thing
    the test said."""
    said = [line.strip() for line in lines if line.strip()]
    for at, line in enumerate(said):
        if m := PANICKED.match(line):
            message = said[at + 1] if at + 1 < len(said) else "(no message)"
            return f"panics at {m.group(1)}: {message}"[:CAUSE_MOST]
    return (said[0] if said else "fails, and says nothing")[:CAUSE_MOST]


def say(
    verdict: Verdict,
    results: Results,
    targets: dict[str, list[str]],
    context: Context,
    count: int,
    baseline: Path,
) -> list[str]:
    """The report, one line a finding with its output's tail indented under it; `::error::`
    prefixes on GitHub's runners."""
    ci = os.environ.get("GITHUB_ACTIONS") == "true"
    error = "::error::" if ci else "error: "
    out: list[str] = []
    rel = os.path.relpath(baseline, ROOT)

    def block(lines: list[str]) -> None:
        out.extend("    | " + line for line in tail(lines, count))

    for test in verdict.new:
        out.append(f"{error}new failure: {crate_of(test.binary, targets)}: {test.key}, not in {rel}")
        block(test.output)
    for entry, how in verdict.fixed:
        out.append(f"{error}fixed: {entry.crate}: {entry.test} {how} now: take it out of {rel}")
    for entry in verdict.gone:
        out.append(
            f"{error}gone: {entry.crate}: {entry.test} did not run though its binary ran in full: take it out of {rel}"
        )
    for binary in verdict.crashed:
        how = binary.exit or "its log ends before its `test result:` line"
        out.append(
            f"{error}did not finish: {crate_of(binary.name, targets)}: test binary {binary.name} ({how}); "
            f"its tests after that are unchecked, and `{binary.name}{UNFINISHED}` is not in {rel}"
        )
        block(binary.lines)
    for package in verdict.not_run:
        out.append(
            f"{error}not run: {package}: none of its test binaries ran (its test build failed, most likely: "
            f"see the log), and `{NOT_RUN}` is not under [crates.{package}] in {rel}"
        )
    failed = sum(1 for t in results.tests.values() if t.verdict == "failed")
    out.append(
        f"{len(results.tests)} tests in {len(results.binaries)} binaries ({context.profile}, {context.os}); "
        f"{failed} failed: {len(verdict.known)} entries known, {len(verdict.new)} new; {len(verdict.fixed)} listed now pass, "
        f"{len(verdict.gone)} listed are gone, {len(verdict.crashed)} binaries did not finish, "
        f"{len(verdict.not_run)} packages did not run"
    )
    for reason, entries in sorted(verdict.unchecked.items()):
        crates: dict[str, int] = {}
        for entry in entries:
            crates[entry.crate] = crates.get(entry.crate, 0) + 1
        said = ", ".join(f"{crate} {n}" for crate, n in sorted(crates.items()))
        out.append(f"unchecked, {reason}: {len(entries)} listed ({said})")
    out.append(
        "FAILED" if verdict.failed else "ok: every failure is listed and every listed test still fails"
    )
    return out


def report_json(verdict: Verdict, targets: dict[str, list[str]], count: int) -> dict[str, object]:
    return {
        "failed": verdict.failed,
        "new": [
            {"crate": crate_of(t.binary, targets), "test": t.key, "tail": tail(t.output, count)}
            for t in verdict.new
        ],
        "fixed": [{"crate": e.crate, "test": e.test, "now": how} for e, how in verdict.fixed],
        "gone": [{"crate": e.crate, "test": e.test} for e in verdict.gone],
        "crashed": [
            {
                "crate": crate_of(b.name, targets),
                "binary": b.name,
                "exit": b.exit,
                "log": b.log,
                "tail": tail(b.lines, count),
            }
            for b in verdict.crashed
        ],
        "not_run": verdict.not_run,
        "known": [{"crate": e.crate, "test": e.test} for e in verdict.known],
        "unchecked": {
            reason: [e.test for e in entries] for reason, entries in sorted(verdict.unchecked.items())
        },
    }


def check(
    logs: list[Path],
    context: Context,
    *,
    count: int = 20,
    report: Path | None = None,
    baseline: Path = BASELINE,
    asked: Iterable[str] | None = None,
) -> bool:
    """[`checked`], as whether the run holds."""
    return not checked(logs, context, count=count, report=report, baseline=baseline, asked=asked).failed


def checked(
    logs: list[Path],
    context: Context,
    *,
    count: int = 20,
    report: Path | None = None,
    baseline: Path = BASELINE,
    asked: Iterable[str] | None = None,
) -> Verdict:
    """Compares `logs` to the list and prints the report. `asked`: the packages the run was of,
    each of which has to have run a test binary."""
    targets = workspace_targets()
    entries = load_baseline(baseline, targets)
    results = read_logs(logs)
    verdict = compare(entries, results, context, asked, test_binaries())
    print("\n".join(say(verdict, results, targets, context, count, baseline)), flush=True)
    if report is not None:
        report.parent.mkdir(parents=True, exist_ok=True)
        report.write_text(json.dumps(report_json(verdict, targets, count), indent=1) + "\n")
    return verdict


# ---------------------------------------------------------------------------------------------
# Recording: the list written again from a run.


def recorded(
    entries: list[Entry],
    results: Results,
    context: Context,
    asked: Iterable[str],
    targets: dict[str, list[str]],
    binaries: dict[str, list[str]],
) -> list[Entry]:
    """The list after a run of the packages `asked` under `context`: what the run says of that
    context, and what the list said of every other one.

    Of an entry the run could check (its package was asked for, it applies to the profile), the
    run decides whether it holds on this OS: it stays for this OS when the test still fails (its
    binary still does not finish, its package still does not run) and leaves it otherwise; what
    it said of the other OSes stays as it was. A failure the list did not have joins it for this
    OS alone, with the first line of what it said as its cause. An entry the run could not check
    (another profile's, a package not asked for, a test filtered out or unreached, a test of a
    binary that did not finish or run) is kept as it is. An entry of a package the workspace no
    longer has is dropped."""
    verdict = compare(entries, results, context, sorted(set(asked)), binaries)
    unchecked = {id(entry) for group in verdict.unchecked.values() for entry in group}
    failing = {id(entry) for entry in verdict.known}
    packages = {package for owners in targets.values() for package in owners}
    out: dict[tuple[str, str], Entry] = {}

    def keep(entry: Entry) -> None:
        out[(entry.crate, entry.test)] = entry

    for entry in entries:
        if entry.crate not in packages:
            continue
        if id(entry) in unchecked:
            keep(entry)
            continue
        after: set[Os] = set(entry.oses if entry.oses is not None else OSES) - {context.os}
        if id(entry) in failing:
            after.add(context.os)
        if after:
            oses = None if after == set(OSES) else tuple(sorted(after))
            keep(Entry(entry.crate, entry.test, entry.cause, entry.profile, oses))

    def new(crate: str, test: str, cause: str) -> None:
        had = out.get((crate, test))
        if had is None:
            keep(Entry(crate, test, cause, None, (context.os,)))
            return
        # The list has it for another OS or profile: it holds for this run's too.
        profile = had.profile if had.profile == context.profile else None
        named = {*(had.oses if had.oses is not None else OSES), context.os}
        oses = None if named == set(OSES) else cast(tuple[Os, ...], tuple(sorted(named)))
        keep(Entry(crate, test, had.cause, profile, oses))

    for test in verdict.new:
        owners = targets.get(test.binary, [])
        if len(owners) != 1:
            raise Refused(
                f"the failing test {test.key} is of the binary `{test.binary}`, which "
                + (f"{' and '.join(owners)} both build" if owners else "no package of the workspace builds")
                + ": the list files a test under one crate"
            )
        new(owners[0], test.key, first_line(test.output))
    for binary in verdict.crashed:
        owners = targets.get(binary.name, [])
        if len(owners) != 1:
            raise Refused(
                f"the test binary `{binary.name}` did not finish, and "
                + (
                    f"{' and '.join(owners)} both build it"
                    if owners
                    else "no package of the workspace builds it"
                )
            )
        how = binary.exit or "its log ends before its `test result:` line"
        new(
            owners[0],
            binary.name + UNFINISHED,
            f"does not finish ({how}): {first_line(tail(binary.lines, 20))}"[:CAUSE_MOST],
        )
    for package in verdict.not_run:
        new(
            package,
            NOT_RUN,
            "none of its test binaries ran: its test build fails (see the run's log for the error)",
        )
    return sorted(out.values(), key=lambda it: (it.crate, it.test))


def tree_of(root: Path = ROOT) -> str:
    """The git tree of the checkout's HEAD, and whether the checkout differs from it: what a
    recorded list names as what it was measured on (AGENTS.md rule measure-the-exact-tree)."""

    def git(*args: str) -> str:
        return subprocess.run(["git", *args], cwd=root, check=True, capture_output=True, text=True).stdout

    tree = git("rev-parse", "HEAD^{tree}").strip()
    # The list itself is what a record rewrites: it does not make the tree another one.
    changed = [
        line
        for line in git("status", "--porcelain").splitlines()
        if not line.endswith("harness/known-failures.toml")
    ]
    return tree + (" with uncommitted changes" if changed else "")


def record(
    logs: list[Path],
    context: Context,
    asked: Iterable[str],
    command: str,
    *,
    baseline: Path = BASELINE,
) -> list[str]:
    """Writes `baseline` again from `logs`, a run of the packages `asked` ([`recorded`]), and
    says what moved, a line each."""
    targets = workspace_targets()
    before = load_baseline(baseline, targets)
    results = read_logs(logs)
    after = recorded(before, results, context, asked, targets, test_binaries())
    said = {"command": command, "os": context.os, "profile": context.profile, "tree": tree_of()}
    baseline.write_text(render(after, said))
    # What is written reads back as it was meant.
    assert load_baseline(baseline, targets) == after, f"{baseline} does not read back as it was written"
    was = {(e.crate, e.test): e for e in before}
    now = {(e.crate, e.test): e for e in after}
    out = [
        f"added: {e.crate}: {e.test}: {e.cause}"
        for key, e in sorted(now.items(), key=lambda it: it[1].test)
        if key not in was
    ]
    out += [
        f"removed: {e.crate}: {e.test}"
        for key, e in sorted(was.items(), key=lambda it: it[1].test)
        if key not in now
    ]
    out += [
        f"moved: {e.crate}: {e.test}: {was[key].only or 'every OS'} -> {e.only or 'every OS'}"
        for key, e in sorted(now.items(), key=lambda it: it[1].test)
        if key in was and was[key].oses != e.oses
    ]
    failed = sum(1 for t in results.tests.values() if t.verdict == "failed")
    out.append(
        f"{os.path.relpath(baseline, ROOT)}: {len(after)} entries ({len(before)} before), from {len(results.tests)} tests "
        f"in {len(results.binaries)} binaries, {failed} failed ({context.profile}, {context.os})"
    )
    return out


def this_os() -> Os:
    if sys.platform.startswith("linux"):
        return "linux"
    if sys.platform == "darwin":
        return "macos"
    if sys.platform in ("win32", "cygwin"):
        return "windows"
    raise Refused(f"failures.py: no OS of the list's is {sys.platform}")


# ---------------------------------------------------------------------------------------------
# The command line.


@dataclass(frozen=True)
class Run:
    packages: tuple[str, ...] = ()
    # Every package of the workspace (`blaze test --workspace`), in place of `packages`.
    workspace: bool = False
    release: bool = False
    args: tuple[str, ...] = ()
    timeout_s: int = 3600


@dataclass(frozen=True)
class Config:
    results: tuple[str, ...] = ()
    profile: Profile | None = None
    os: Os | None = None
    run: Run | None = None
    tail: int = 20
    report: str = ""
    baseline: str = ""


async def run_tests(run: Run, log: Path) -> int:
    """Runs the tests into `log`, each line passed on as it comes; their exit status."""
    packages = ["--workspace"] if run.workspace else [arg for p in run.packages for arg in ("-p", p)]
    command = [
        "blaze",
        "test",
        "--no-fail-fast",
        *packages,
        *(["--release"] if run.release else []),
        *run.args,
    ]
    log.parent.mkdir(parents=True, exist_ok=True)
    print(" ".join(command), f"> {log}", flush=True)
    proc = await asyncio.create_subprocess_exec(
        *command, cwd=ROOT, stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.STDOUT
    )
    assert proc.stdout is not None
    began = time.monotonic()
    try:
        with log.open("wb") as out:
            async with asyncio.timeout(run.timeout_s):
                async for line in proc.stdout:
                    out.write(line)
                    sys.stdout.buffer.write(line)
                    sys.stdout.flush()
                return await proc.wait()
    except TimeoutError:
        proc.kill()
        await proc.wait()
        said = log.read_text(errors="replace").splitlines()[-20:]
        raise Refused(
            f"step `test`: {' '.join(command)} ran past timeout_s {run.timeout_s} "
            f"({time.monotonic() - began:.0f} s) and was killed; the end of {log}:\n  " + "\n  ".join(said)
        ) from None


def selftest() -> None:
    """The comparison, the recording and the file's round trip on made-up results; no blaze runs."""
    targets = {"a": ["pkg-a"], "a_it": ["pkg-a"], "b": ["pkg-b"], "c": ["pkg-c"]}
    binaries = {"pkg-a": ["a", "a_it"], "pkg-b": ["b"], "pkg-c": ["c"], "pkg-d": []}
    linux, macos = Context("release", "linux"), Context("release", "macos")

    def results(tests: dict[str, str], unfinished: tuple[str, ...] = ()) -> Results:
        made = Results()
        for key, verdict in tests.items():
            binary, name = key.split("::", 1)
            made.binaries.setdefault(binary, Binary(binary, "log", finished=binary not in unfinished))
            made.tests[key] = Test(binary, name, verdict, ["thread 'x' panicked at src/lib.rs:3:5:", "boom"])  # pyright: ignore[reportArgumentType]
        for binary in unfinished:
            made.binaries.setdefault(binary, Binary(binary, "log", exit="signal: 6"))
        return made

    listed = [Entry("pkg-a", "a::fails", "known"), Entry("pkg-a", "a::mac", "mac's", None, ("macos",))]
    ran = results({"a::fails": "failed", "a::ok": "ok", "a::mac": "ok", "b::t": "ok", "c::t": "ok"})
    asked = ["pkg-a", "pkg-b", "pkg-c", "pkg-d"]
    held = compare(listed, ran, linux, asked, binaries)
    assert not held.failed and [e.test for e in held.known] == ["a::fails"], held
    # A new failure, a listed test that passes, a crash and a package that did not run each fail.
    assert [
        t.key
        for t in compare(
            listed,
            results({"a::fails": "failed", "a::new": "failed", "b::t": "ok", "c::t": "ok"}),
            linux,
            asked,
            binaries,
        ).new
    ] == ["a::new"]
    assert compare(
        listed, results({"a::fails": "ok", "b::t": "ok", "c::t": "ok"}), linux, asked, binaries
    ).fixed
    assert compare(listed, ran, macos, asked, binaries).fixed, "a::mac passes on macOS: take it out"
    crashed = results({"a::fails": "failed", "c::t": "ok"}, unfinished=("b",))
    assert [b.name for b in compare(listed, crashed, linux, asked, binaries).crashed] == ["b"]
    assert compare(listed, results({"a::fails": "failed", "b::t": "ok"}), linux, asked, binaries).not_run == [
        "pkg-c"
    ]
    assert not compare(listed, results({"a::fails": "failed"}), linux, None, binaries).not_run, (
        "a run of unknown packages"
    )
    # Recording: the crash and the package that did not run are listed, and then the same run holds.
    broken = results({"a::fails": "failed", "a::new": "failed", "a::mac": "ok"}, unfinished=("b",))
    after = recorded(listed, broken, linux, asked, targets, binaries)
    assert [(e.crate, e.test, e.oses) for e in after] == [
        ("pkg-a", "a::fails", None),
        ("pkg-a", "a::mac", ("macos",)),
        ("pkg-a", "a::new", ("linux",)),
        ("pkg-b", "b::*", ("linux",)),
        ("pkg-c", "*", ("linux",)),
    ], after
    assert after[2].cause == "panics at src/lib.rs: boom", after[2].cause
    again = compare(after, broken, linux, asked, binaries)
    assert not again.failed and len(again.known) == 4, again
    assert recorded(after, broken, linux, asked, targets, binaries) == after, (
        "recording twice is the same list"
    )
    # A listed failure that passes here leaves the list for this OS alone; one fixed everywhere leaves it.
    fixed = recorded(
        after,
        results({"a::fails": "ok", "a::new": "ok", "a::mac": "ok", "b::t": "ok", "c::t": "ok"}),
        linux,
        asked,
        targets,
        binaries,
    )
    assert [(e.test, e.oses) for e in fixed] == [
        ("a::fails", ("macos", "windows")),
        ("a::mac", ("macos",)),
    ], fixed
    # The same failure recorded on the next OS is one entry for both.
    both = recorded(after, broken, macos, asked, targets, binaries)
    assert next(e for e in both if e.test == "a::new").oses == ("linux", "macos"), both
    assert not any(e.test == "a::mac" for e in both), "a::mac passes on macOS now"
    # The file reads back as it was written, byte for byte the same when written again.
    import tempfile

    with tempfile.TemporaryDirectory() as tmp:
        path = Path(tmp) / "known-failures.toml"
        text = render(after, {"tree": "t", "os": "linux"})
        path.write_text(text)
        assert load_baseline(path, targets) == after
        assert render(load_baseline(path, targets), {"os": "linux", "tree": "t"}) == text
    assert first_line(
        ["", "thread 'a::b' (123) panicked at crates/x/src/lib.rs:10:5:", "assertion failed: x"]
    ) == ("panics at crates/x/src/lib.rs: assertion failed: x")
    assert first_line([]) == "fails, and says nothing"
    # The checked-in list holds together and is in the form `render` writes.
    real = load_baseline(BASELINE, workspace_targets())
    data = tomllib.loads(BASELINE.read_text())
    assert render(real, cast(dict[str, str], data.get("recorded", {}))) == BASELINE.read_text(), (
        f"{BASELINE} is not as `render` writes it: write it with unittests.py's record, not by hand"
    )
    print(f"failures.py selftest: ok ({len(real)} entries in {os.path.relpath(BASELINE, ROOT)})")


def main() -> None:
    # Only the command line needs the workspace package; unittests.py imports the rest under a
    # plain python3.
    from typed_input import fail, parse

    if len(sys.argv) != 2:
        fail(
            "failures.py",
            "usage: failures.py CONFIG, one JSON object (see the top of the file), or `selftest`",
        )
    if sys.argv[1] == "selftest":
        selftest()
        return
    config = parse(Config, sys.argv[1], "failures.py")
    baseline = Path(config.baseline) if config.baseline else BASELINE
    report = Path(config.report) if config.report else None
    try:
        if config.run is not None:
            if config.results or config.profile or config.os:
                fail("failures.py", "`run` writes its own log: give no `results`, `profile` or `os`")
            run = config.run
            if bool(run.packages) == run.workspace:
                fail("failures.py", "`run` names `packages` or says `workspace`: one of them")
            asked = workspace_packages() if run.workspace else list(run.packages)
            target = Path(os.environ.get("CARGO_TARGET_DIR", ROOT / "target"))
            log = target / "known-failures" / f"blaze-{time.strftime('%Y%m%dT%H%M%S')}.log"
            status = asyncio.run(run_tests(run, log))
            context = Context("release" if run.release else "dev", this_os())
            verdict = checked(
                [log], context, count=config.tail, report=report, baseline=baseline, asked=asked
            )
            held = not verdict.failed
            if status and held and not verdict.known:
                print(
                    f"error: the run exited {status} with every failure listed: the build, most likely (see {log})"
                )
                sys.exit(1)
            sys.exit(0 if held else 1)
        if not config.results:
            fail("failures.py", "give `results` (logs to compare) or `run` (tests to run)")
        if config.profile is None or config.os is None:
            fail("failures.py", "`results` needs `profile` and `os`: what produced the logs")
        context = Context(config.profile, config.os)
        held = check(
            [Path(p) for p in config.results], context, count=config.tail, report=report, baseline=baseline
        )
        sys.exit(0 if held else 1)
    except Refused as e:
        fail("failures.py", str(e))


if __name__ == "__main__":
    main()
