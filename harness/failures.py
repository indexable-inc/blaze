#!/usr/bin/env python3
"""failures.py: a test run held to harness/known-failures.toml, the failures known and owned.

usage: uv run --frozen harness/failures.py CONFIG

CONFIG is one JSON object, inline, the path of a file, or `-` for standard input
(AGENTS.md rule scripts-take-typed-input). Either it compares logs already written:

    {"results": ["<log>", ...], "runner": "cargo" | "blaze", "profile": "dev" | "release",
     "os": "linux" | "macos" | "windows", "tail": 20, "report": "<path>"}

or it runs the tests of the packages named, writes their log, and compares that:

    {"run": {"runner": "cargo" | "blaze", "packages": ["hir-def", ...], "release": false,
             "args": [], "timeout_s": 3600}, "tail": 20, "report": "<path>"}

A log is what `cargo test` or `blaze test` print, stdout and stderr together, in libtest's text
format or its JSON one (`-- -Z unstable-options --format json`), or a `.json` result of the lanes'
shared runner (/tmp/blaze-lanes/runner/README.md, `results/<id>.json`). A test is named
`<binary>::<path>`, the binary as cargo's `Running` line names it with `-` as `_`.

The run fails on:
- a test that fails and is not listed (a new failure);
- a listed test that passes, is ignored, or no longer exists in a binary that ran in full (the
  list only shrinks: take it out);
- a test binary that did not finish (a crash, an abort, a stack overflow): whatever it had left
  to run is unchecked, so it never passes quietly;
- a baseline that does not hold together (an entry without an owner or a cause, a test of a
  binary its crate does not have).
Every failure names the crate, the test and the tail of its output. Entries owned by
"unowned" are listed as such, without failing the run. Listed tests this run could not check
(their crate did not run, they were filtered out or unreached, or the entry is `only` for
another runner, profile or OS) are counted, so a partial run never reads as clean.

The comparison is stdlib-only, so harness/unittests.py imports it under CI's plain python3.
"""

import asyncio
import glob
import json
import os
import re
import sys
import time
import tomllib
from collections.abc import Iterable
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Literal, cast

ROOT = Path(__file__).resolve().parent.parent
BASELINE = ROOT / "harness" / "known-failures.toml"
UNOWNED = "unowned"

type Runner = Literal["cargo", "blaze"]
type Profile = Literal["dev", "release"]
type Os = Literal["linux", "macos", "windows"]

COLOR = re.compile(r"\x1b\[[0-9;]*m")
# The binary of a `Running` line: cargo's `Running unittests src/lib.rs (target/debug/deps/x-<hash>)`,
# its second layout's `build/<package>/<hash>/out/x-<hash>`, and blaze's `Running target/debug/deps/x-<hash>`.
BINARY = re.compile(r"[/\\]([\w-]+?)-[0-9a-f]{8,}(?:\.exe)?\b")
RUNNING = re.compile(r"^\s+Running\s")
DOCTESTS = re.compile(r"^\s+Doc-tests (\S+)")
RESULT = re.compile(r"^test (.+?) \.\.\. (ok|FAILED|ignored)\b")
SUMMARY = re.compile(r"^test result: (ok|FAILED)\.")
FILTERED = re.compile(r"(\d+) filtered out")
UNREACHED = re.compile(r"(\d+) unreached")
OUTPUT = re.compile(r"^---- (.+) stdout ----$")
# cargo's line after a test binary that failed or crashed; what follows is cargo's, not the binary's.
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
    """The results of `paths`: logs, or the shared runner's result files (`<id>.json`)."""
    results = Results()
    for path in paths:
        if not path.is_file():
            raise Refused(f"the log {path} does not exist")
        if path.suffix == ".json":
            read_runner(results, path, lib_names())
            continue
        reader = Reader(results, str(path))
        with path.open(encoding="utf-8", errors="replace") as f:
            for line in f:
                reader.feed(line)
    return results


def _strings(value: object, where: str) -> list[str]:
    if not isinstance(value, list) or not all(isinstance(it, str) for it in cast(list[object], value)):
        raise Refused(f"{where} is a list of names")
    return cast(list[str], value)


def _records(value: object, where: str) -> list[dict[str, object]]:
    if not isinstance(value, list) or not all(isinstance(it, dict) for it in cast(list[object], value)):
        raise Refused(f"{where} is a list of objects")
    return cast(list[dict[str, object]], value)


def read_runner(results: Results, path: Path, libs: dict[str, str]) -> None:
    """A result of the lanes' shared runner (/tmp/blaze-lanes/runner/README.md): its tests by
    target label, `<package>/lib` or `<package>/<kind>:<name>`, as the binaries a log names."""
    try:
        data = json.loads(path.read_text())
    except (OSError, json.JSONDecodeError) as e:
        raise Refused(f"{path}: {e}") from e
    if not isinstance(data, dict) or "tests" not in data or "tree" not in data:
        raise Refused(f"{path} is not a runner result (no `tests` and `tree`)")
    result = cast(dict[str, object], data)
    request = result.get("request")
    filtered = isinstance(request, dict) and cast(dict[str, object], request).get("test_filter") not in (
        None,
        [],
        "",
    )
    tests = result["tests"]
    if not isinstance(tests, dict):
        raise Refused(f"{path}: `tests` is an object")
    tally = cast(dict[str, object], tests)

    # Several results combine as several logs do: a binary another result ran is replaced by this
    # result's run of it, and never by this result's "not built".
    mine: set[str] = set()

    def name_of(label: str) -> str:
        package, _, target = label.partition("/")
        if target == "lib":
            if package not in libs:
                raise Refused(f"{path}: the target {label} names no workspace package")
            return libs[package]
        return target.partition(":")[2].replace("-", "_")

    def binary(label: str) -> Binary:
        name = name_of(label)
        if name not in mine:
            mine.add(name)
            for key in [k for k, t in results.tests.items() if t.binary == name]:
                del results.tests[key]
            results.binaries[name] = Binary(name, str(path), finished=True, filtered=int(filtered))
        return results.binaries[name]

    for verdict in ("passed", "ignored"):
        groups = tally.get(verdict, {})
        if not isinstance(groups, dict):
            raise Refused(f"{path}: `tests.{verdict}` is an object")
        for label, names in sorted(cast(dict[str, object], groups).items()):
            b = binary(label)
            for name in _strings(names, f"{path}: tests.{verdict}.{label}"):
                test = Test(b.name, name, "ok" if verdict == "passed" else "ignored")
                results.tests[test.key] = test
    for failed in _records(tally.get("failed", []), f"{path}: tests.failed"):
        b = binary(str(failed["binary"]))
        test = Test(b.name, str(failed["name"]), "failed", str(failed.get("tail", "")).splitlines())
        results.tests[test.key] = test
    # Every other list of tests (crashed, timed_out, memory_exceeded, cpu_budget_exceeded, blocked,
    # and whatever the runner adds) is of tests that did not finish: unknown kinds fail closed.
    finished_kinds = {"counts", "passed", "ignored", "failed", "binaries"}
    for how in sorted(set(tally) - finished_kinds):
        for item in _records(tally.get(how, []), f"{path}: tests.{how}"):
            b = binary(str(item["binary"]))
            b.finished = False
            said = f"{how.replace('_', ' ')} in {item['name']}: {item.get('outcome', '')}".rstrip(": ")
            b.exit = f"{b.exit}; {said}" if b.exit else said
            b.lines = str(item.get("tail", "")).splitlines()
    # A binary that ended other than by libtest's own verdict (a signal, the runner's memory or CPU
    # guard) did not finish, whether or not a test was named for it, unless the runner reran every
    # test it had left to an outcome of its own (`settled_by_reruns`: tests sharing the process
    # were over the bound together, none alone; one that is over alone is in `memory_exceeded`).
    # (A binary with no tests is named only here: it ran all the same.)
    for item in _records(tally.get("binaries", []), f"{path}: tests.binaries"):
        outcome = str(item.get("outcome", ""))
        b = binary(str(item["binary"]))
        if outcome not in ("exited 0", "exited 101") and item.get("settled_by_reruns") is not True:
            b.finished = False
            if outcome not in (b.exit or ""):
                b.exit = f"{b.exit}; {outcome}" if b.exit else outcome
    for item in _records(result.get("unchecked", []), f"{path}: unchecked"):
        name = name_of(str(item["target"]))
        earlier = results.binaries.get(name)
        if name not in mine and earlier is not None and not (earlier.exit or "").startswith("not built"):
            continue
        b = binary(str(item["target"]))
        b.finished = False
        b.exit = (
            f"not built, blocked by {', '.join(_strings(item.get('blocked_by', []), f'{path}: blocked_by'))}"
        )


def lib_names(root: Path = ROOT) -> dict[str, str]:
    """Each workspace package's library binary name (`-` as `_`)."""
    names: dict[str, str] = {}
    for _, toml in _members(root):
        package = toml.get("package", {}).get("name")
        if isinstance(package, str):
            names[package] = str(toml.get("lib", {}).get("name", package)).replace("-", "_")
    return names


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


# ---------------------------------------------------------------------------------------------
# The workspace's test binaries, by package: which crate a failing binary belongs to.


def workspace_targets(root: Path = ROOT) -> dict[str, list[str]]:
    """Each test binary name (`-` as `_`) the workspace's members build, to their packages."""
    targets: dict[str, list[str]] = {}

    def add(name: str, package: str) -> None:
        owners = targets.setdefault(name.replace("-", "_"), [])
        if package not in owners:
            owners.append(package)

    for member, toml in _members(root):
        package = toml.get("package", {}).get("name")
        if not isinstance(package, str):
            continue
        lib = toml.get("lib", {})
        add(lib.get("name", package), package)
        for kind, auto, directory in (
            ("test", "autotests", "tests"),
            ("bin", "autobins", "src/bin"),
            ("bench", "autobenches", "benches"),
        ):
            for target in toml.get(kind, []):
                if "name" in target:
                    add(target["name"], package)
            if toml.get("package", {}).get(auto, True):
                for path in sorted((member / directory).glob("*.rs")):
                    add(path.stem, package)
                for path in sorted((member / directory).glob("*/main.rs")):
                    add(path.parent.name, package)
        if (member / "src" / "main.rs").is_file():
            add(package, package)
    return targets


# ---------------------------------------------------------------------------------------------
# The baseline.


@dataclass(frozen=True)
class Context:
    runner: Runner
    profile: Profile
    os: Os


ONLY_KEYS = {"runner": ("cargo", "blaze"), "profile": ("dev", "release"), "os": ("linux", "macos", "windows")}
ENTRY_KEYS = {"owner", "cause", "only"}


@dataclass(frozen=True)
class Entry:
    crate: str
    test: str
    owner: str
    cause: str
    only: tuple[tuple[str, str], ...] = ()

    @property
    def binary(self) -> str:
        return self.test.split("::", 1)[0]

    def applies(self, context: Context) -> bool:
        return all(getattr(context, key) == value for key, value in self.only)


def load_baseline(path: Path, targets: dict[str, list[str]]) -> list[Entry]:
    """The entries of `path`, every problem with it named at once."""
    try:
        data: dict[str, object] = tomllib.loads(path.read_text())
    except (OSError, tomllib.TOMLDecodeError) as e:
        raise Refused(f"{path}: {e}") from e
    problems: list[str] = []
    entries: list[Entry] = []
    unknown = sorted(set(data) - {"measured", "crates"})
    if unknown:
        problems.append(f"top-level tables {', '.join(unknown)} are not `measured` or `crates`")
    crates = data.get("crates", {})
    if not isinstance(crates, dict):
        raise Refused(f"{path}: `crates` is a table of crates")
    for crate, tests in sorted(cast(dict[str, object], crates).items()):
        if not isinstance(tests, dict):
            problems.append(f"[crates.{crate}] is a table of tests")
            continue
        for test, raw in sorted(cast(dict[str, object], tests).items()):
            where = f'[crates.{crate}] "{test}"'
            if not isinstance(raw, dict):
                problems.append(f"{where} is an inline table {{ owner, cause }}")
                continue
            entry = cast(dict[str, object], raw)
            if extra := sorted(set(entry) - ENTRY_KEYS):
                problems.append(f"{where} has {', '.join(extra)}, not one of {', '.join(sorted(ENTRY_KEYS))}")
            owner, cause = entry.get("owner"), entry.get("cause")
            if not isinstance(owner, str) or not owner.strip():
                problems.append(f'{where} has no owner (a lane, an issue, or "{UNOWNED}")')
            if not isinstance(cause, str) or not cause.strip():
                problems.append(f"{where} has no cause")
            only_raw = entry.get("only", {})
            only: list[tuple[str, str]] = []
            if not isinstance(only_raw, dict):
                problems.append(f"{where}: `only` is a table")
            else:
                for key, value in sorted(cast(dict[str, object], only_raw).items()):
                    if key not in ONLY_KEYS or value not in ONLY_KEYS[key]:
                        problems.append(f"{where}: `only.{key} = {value!r}` is not one of {ONLY_KEYS}")
                    else:
                        only.append((key, str(value)))
            if "::" not in test:
                problems.append(f"{where} is not `<binary>::<test>`")
            binary = test.split("::", 1)[0]
            if crate not in targets.get(binary, []):
                owners = targets.get(binary)
                said = f"belongs to {', '.join(owners)}" if owners else "is no test binary of the workspace"
                problems.append(f"{where}: the binary `{binary}` {said}, not {crate}")
            entries.append(Entry(crate, test, str(owner), str(cause), tuple(only)))
    if problems:
        raise Refused(f"{path} does not hold together:\n  " + "\n  ".join(problems))
    return entries


# ---------------------------------------------------------------------------------------------
# The comparison.


@dataclass
class Verdict:
    new: list[Test] = field(default_factory=list[Test])
    fixed: list[tuple[Entry, str]] = field(default_factory=list[tuple[Entry, str]])
    gone: list[Entry] = field(default_factory=list[Entry])
    crashed: list[Binary] = field(default_factory=list[Binary])
    known: list[Entry] = field(default_factory=list[Entry])
    unowned: list[Entry] = field(default_factory=list[Entry])
    unchecked: dict[str, list[Entry]] = field(default_factory=dict[str, list[Entry]])

    @property
    def failed(self) -> bool:
        return bool(self.new or self.fixed or self.gone or self.crashed)


def compare(entries: list[Entry], results: Results, context: Context) -> Verdict:
    verdict = Verdict()
    listed = {entry.test: entry for entry in entries}
    for entry in entries:
        if entry.owner == UNOWNED:
            verdict.unowned.append(entry)
        if not entry.applies(context):
            reason = "only for " + ", ".join(f"{k} {v}" for k, v in entry.only)
            verdict.unchecked.setdefault(reason, []).append(entry)
            continue
        binary = results.binaries.get(entry.binary)
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
    verdict.crashed = sorted((b for b in results.binaries.values() if not b.finished), key=lambda b: b.name)
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
    warning = "::warning::" if ci else "warning: "
    out: list[str] = []
    rel = os.path.relpath(baseline, ROOT)

    def block(lines: list[str]) -> None:
        out.extend("    | " + line for line in tail(lines, count))

    for test in verdict.new:
        out.append(f"{error}new failure: {crate_of(test.binary, targets)}: {test.key}, not in {rel}")
        block(test.output)
    for entry, how in verdict.fixed:
        out.append(
            f"{error}fixed: {entry.crate}: {entry.test} {how} now: take it out of {rel} (owner {entry.owner})"
        )
    for entry in verdict.gone:
        out.append(
            f"{error}gone: {entry.crate}: {entry.test} did not run though its binary ran in full: take it out of {rel}"
        )
    for binary in verdict.crashed:
        how = binary.exit or "its log ends before its `test result:` line"
        left = (
            "none of its tests ran" if how.startswith("not built") else "its tests after that are unchecked"
        )
        out.append(
            f"{error}did not finish: {crate_of(binary.name, targets)}: test binary {binary.name} ({how}); {left}"
        )
        block(binary.lines)
    for entry in verdict.unowned:
        out.append(f"{warning}unowned: {entry.crate}: {entry.test}: {entry.cause}")
    failed = sum(1 for t in results.tests.values() if t.verdict == "failed")
    out.append(
        f"{len(results.tests)} tests in {len(results.binaries)} binaries ({context.runner}, {context.profile}, {context.os}); "
        f"{failed} failed: {len(verdict.known)} known, {len(verdict.new)} new; {len(verdict.fixed)} listed now pass, "
        f"{len(verdict.gone)} listed are gone, {len(verdict.crashed)} binaries did not finish; {len(verdict.unowned)} entries unowned"
    )
    for reason, entries in sorted(verdict.unchecked.items()):
        crates: dict[str, int] = {}
        for entry in entries:
            crates[entry.crate] = crates.get(entry.crate, 0) + 1
        said = ", ".join(f"{crate} {n}" for crate, n in sorted(crates.items()))
        out.append(f"unchecked, {reason}: {len(entries)} listed ({said})")
    out.append("FAILED" if verdict.failed else "ok: every failure is known and every listed test still fails")
    return out


def report_json(verdict: Verdict, targets: dict[str, list[str]], count: int) -> dict[str, object]:
    return {
        "failed": verdict.failed,
        "new": [
            {"crate": crate_of(t.binary, targets), "test": t.key, "tail": tail(t.output, count)}
            for t in verdict.new
        ],
        "fixed": [
            {"crate": e.crate, "test": e.test, "now": how, "owner": e.owner} for e, how in verdict.fixed
        ],
        "gone": [{"crate": e.crate, "test": e.test, "owner": e.owner} for e in verdict.gone],
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
        "known": [{"crate": e.crate, "test": e.test, "owner": e.owner} for e in verdict.known],
        "unowned": [{"crate": e.crate, "test": e.test, "cause": e.cause} for e in verdict.unowned],
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
) -> bool:
    """Compares `logs` to the baseline, prints the report, and says whether the run holds."""
    targets = workspace_targets()
    entries = load_baseline(baseline, targets)
    results = read_logs(logs)
    verdict = compare(entries, results, context)
    print("\n".join(say(verdict, results, targets, context, count, baseline)), flush=True)
    if report is not None:
        report.parent.mkdir(parents=True, exist_ok=True)
        report.write_text(json.dumps(report_json(verdict, targets, count), indent=1) + "\n")
    return not verdict.failed


def this_os() -> Os:
    if sys.platform.startswith("linux"):
        return "linux"
    if sys.platform == "darwin":
        return "macos"
    if sys.platform in ("win32", "cygwin"):
        return "windows"
    raise Refused(f"failures.py: no OS of the baseline's is {sys.platform}")


# ---------------------------------------------------------------------------------------------
# The command line.


@dataclass(frozen=True)
class Run:
    runner: Runner
    packages: tuple[str, ...]
    release: bool = False
    args: tuple[str, ...] = ()
    timeout_s: int = 3600


@dataclass(frozen=True)
class Config:
    results: tuple[str, ...] = ()
    runner: Runner | None = None
    profile: Profile | None = None
    os: Os | None = None
    run: Run | None = None
    tail: int = 20
    report: str = ""
    baseline: str = ""


async def run_tests(run: Run, log: Path) -> int:
    """Runs the tests into `log`, each line passed on as it comes; their exit status."""
    toolchain = (ROOT / "harness" / "toolchain").read_text().strip()
    packages = [arg for p in run.packages for arg in ("-p", p)]
    head = ["cargo", f"+{toolchain}"] if run.runner == "cargo" else ["blaze"]
    command = [*head, "test", "--no-fail-fast", *packages, *(["--release"] if run.release else []), *run.args]
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


def main() -> None:
    # Only the command line needs the workspace package; unittests.py imports the rest under a
    # plain python3.
    from typed_input import fail, parse

    if len(sys.argv) != 2:
        fail("failures.py", "usage: failures.py CONFIG, one JSON object (see the top of the file)")
    config = parse(Config, sys.argv[1], "failures.py")
    baseline = Path(config.baseline) if config.baseline else BASELINE
    report = Path(config.report) if config.report else None
    try:
        if config.run is not None:
            if config.results or config.runner or config.profile or config.os:
                fail(
                    "failures.py", "`run` writes its own log: give no `results`, `runner`, `profile` or `os`"
                )
            run = config.run
            if not run.packages:
                fail("failures.py", "`run.packages` names at least one package")
            target = Path(os.environ.get("CARGO_TARGET_DIR", ROOT / "target"))
            log = target / "known-failures" / f"{run.runner}-{time.strftime('%Y%m%dT%H%M%S')}.log"
            status = asyncio.run(run_tests(run, log))
            context = Context(run.runner, "release" if run.release else "dev", this_os())
            held = check([log], context, count=config.tail, report=report, baseline=baseline)
            if status and held:
                print(
                    f"error: the run exited {status} with every failure known: the build, most likely (see {log})"
                )
                sys.exit(1)
            sys.exit(0 if held else 1)
        if not config.results:
            fail("failures.py", "give `results` (logs to compare) or `run` (tests to run)")
        if config.runner is None or config.profile is None or config.os is None:
            fail("failures.py", "`results` needs `runner`, `profile` and `os`: what produced the logs")
        context = Context(config.runner, config.profile, config.os)
        held = check(
            [Path(p) for p in config.results], context, count=config.tail, report=report, baseline=baseline
        )
        sys.exit(0 if held else 1)
    except Refused as e:
        fail("failures.py", str(e))


if __name__ == "__main__":
    main()
