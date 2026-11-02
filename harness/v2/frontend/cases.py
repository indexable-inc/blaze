"""cases.py: the front end's cases, each built by blaze and held to what its header says
(docs/v2/plan/lanes/frontend/fast-diff.md; section 9 of docs/v2/plan/lanes/frontend/fast.md).

    uv run --frozen python harness/v2/frontend/cases.py '{"engine": {"kind": "v2", "blaze": "target/release/blaze"}, "out": "/tmp/cases.json"}'
    uv run --frozen python harness/v2/frontend/cases.py '{"engine": {...}, "out": "...", "areas": ["nameres"], "filter": "e0425"}'
    uv run --frozen python harness/v2/frontend/cases.py selftest

A case is a `.rs` file directly under `cases/<area>/` (the modules it names with `mod` go beside
it, `m.rs` or `m/mod.rs`), whose first line is one of

    //@ expect: accept
    //@ expect: reject E0425, E0433

naming each error Rust gives it: its code, a denied lint by name, or an error with no code by the
start of its message in double quotes. These are the case's own expectations, written from what
the language says of the program. Later lines may carry these directives, and no others:

    //@ edition: 2018          the package's edition (else 2015)
    //@ compile-flags: --test  checked as a test harness (`blaze check --tests`)
    //@ crate: lib | bin       what `#![crate_type = "lib"]` (or its absence) makes it, said again
    //@ run: panic             an accepted program panics: it exits 101 with a panic
    //@ aux-crate: dep=dep.rs  a dependency, below
    //@ refused: dep[, dep2]   bodies-cache only, below
    //@ issue: #1440           the issue it stands for, shown with its outcome

`#![crate_type = "lib"]` makes the case a library, else it is a program with `fn main`. An
accepted program is run (RUN_SECS at most): it exits 0 and does not panic (or, with `//@ run:
panic`, exits 101 with a panic), and when a file `<case>.stdout` is beside it, prints exactly it.

A case may depend on crates: `//@ aux-crate: dep=dep.rs` names the file `auxiliary/dep.rs`
beside it, which may name its own the same way and take `//@ edition:` (else 2015) and
`#![crate_type = "proc-macro"]`. blaze gets each as a crates.io package vendored in the
workspace (a `directory` source replacing crates.io), so it is a registry crate, as
`hir_def::deps::eligible` wants of a crate it serves.

The cases of `bodies-cache/` are built by blaze twice, with one store and a fresh daemon,
workspace and target: the first checks the dependencies and keeps what it can, the second must
serve each from the store, which it says on stdout with a line per dependency,
`{"reason": "blaze-served", "package": P, "outcome": W, "bodies": W, "why": null | "..."}`, W one
of `served`, `checked`, `refused`. Both builds are held to the header; then each dependency must
have both served, but those a `//@ refused: dep[, dep2]` names, of which one must be `refused`
(the edges where the cache hands over to a check).

The cases are written as packages, up to `batch` of a kind in a workspace, `blaze check
--message-format=json` of a library or test and `blaze build` of a program, each under the
workspace's one daemon, with `RUSTFLAGS` allowing the lints compiletest allows (`COMMON`). An
error is keyed by its code, or, with no code, by the start of its message with what is in
backticks taken out. A case:

    pass   blaze does what the header says: rejects it with exactly the errors named, or
           accepts it, and a program runs as the case says
    fail   blaze accepts what the case rejects, rejects what it accepts, gives other errors, or
           its program runs otherwise; or the check was not blaze's; or a second build did not
           serve a dependency it should (or served one it should not)
    error  the case is wrong ("case wrong"), or the harness could not say (a timeout, a
           program blaze said it built and did not)

The input is one JSON object (inline, a file's path, or `-` for stdin):

    engine    the blaze under test (engine.EngineSpec)
    out       the path the result JSON is written to (report.py)
    work      the directory everything is built in; empty: a new one under /tmp
    dir       the cases' directory ("": cases/ beside this file)
    areas     the areas (directories of `dir`) to run; empty: every one
    filter    only the cases whose name (`<area>/<file>`) has this in it
    batch     how many cases one workspace has (100)
    whole     one `blaze check --workspace` (or `build`) a workspace, not one a case (false)
    jobs      how many workspaces at once (4)
    keep      keep every workspace, not only those of cases that did not pass
"""

import concurrent.futures
import hashlib
import itertools
import json
import re
import shutil
import sys
import tempfile
from collections import Counter
from collections.abc import Sequence
from dataclasses import dataclass, field, replace
from pathlib import Path
from typing import Any, Literal, cast

HERE = Path(__file__).resolve().parent
sys.path[:0] = [str(HERE), str(HERE.parent), str(HERE.parent.parent)]
from typed_input import fail, parse  # noqa: E402
import engine, report  # noqa: E402
from corpus import PANIC  # noqa: E402

SCRIPT = "cases.py"
CASES = HERE / "cases"
Kind = Literal["lib", "bin", "test"]
DIRECTIVE = re.compile(r"^\s*//@\s*(?P<name>[\w-]+)(?: *: *| +|$)(?P<value>.*)$", re.M)
EXPECT = re.compile(r"//@ expect: *(accept|reject +(\S.*))")
TOKEN = re.compile(r'"([^"]+)"|([^\s,"]+)')
CRATE_TYPE = re.compile(r'#!\[crate_type *= *"([\w-]+)"\]')
MODULE = re.compile(r"^\s*(?:pub(?:\([\w: ]+\))? )?mod (?P<name>\w+);", re.M)
UNCOPIED = re.compile(r'#\[path *=|\binclude(?:_str|_bytes)?!')
AUX_VALUE = re.compile(r"([A-Za-z_][A-Za-z0-9_]*)=([^\s=]+\.rs)")
EDITIONS = ("2015", "2018", "2021", "2024")
# What compiletest allows of a ui test that is not run, and what the package's own `--check-cfg`
# would warn of: the headers were written with these allowed.
COMMON = ("-A", "unused", "-A", "internal_features", "-A", "unexpected_cfgs")
KIND_ARGS: dict[Kind, tuple[str, ...]] = {"lib": ("--lib",), "bin": ("--bins",), "test": ("--tests",)}
CASE_DIRECTIVES = frozenset({"expect", "edition", "compile-flags", "crate", "run", "aux-crate", "refused", "issue"})
DEP_DIRECTIVES = frozenset({"edition", "aux-crate"})
# compiletest's directives of an auxiliary crate that change nothing here.
AUX_INERT = frozenset({"force-host", "no-prefer-dynamic"})
MESSAGE = "message: "
MESSAGE_START = 60
PANICKED = 101
RUN_SECS = 10
CHECK_SECS = 1800
AUXILIARY = "auxiliary"
# The areas whose cases blaze builds twice, the second served from the store the first filled.
TWICE = frozenset({"bodies-cache"})
VERSION = "0.1.0"
SOURCE = "blaze-cases"
CONFIG = f'[source.crates-io]\nreplace-with = "{SOURCE}"\n\n[source.{SOURCE}]\ndirectory = "vendor"\n'
CHECKSUMS = ".cargo-checksum.json"
WORKSPACE = '[workspace]\nresolver = "2"\nmembers = ["crates/*"]\n'
SERVED = "blaze-served"
WAYS = ("served", "checked", "refused")

Category = Literal["as expected", "case wrong", "accepted what the case rejects", "rejected what the case accepts",
                   "other codes", "other run", "not blaze's", "not served", "served what is refused", "error"]


@dataclass(frozen=True)
class Input:
    engine: engine.EngineSpec
    out: str
    work: str = ""
    dir: str = ""
    areas: tuple[str, ...] = ()
    filter: str = ""
    batch: int = 100
    whole: bool = False
    jobs: int = 4
    keep: bool = False


@dataclass(frozen=True)
class Expect:
    """A case's header: accepted, or rejected with exactly these keys (`tokens_of`)."""

    accept: bool
    tokens: tuple[str, ...] = ()

    def __str__(self) -> str:
        return "accept" if self.accept else "reject " + ", ".join(shown_key(t) for t in self.tokens)


@dataclass(frozen=True)
class Dep:
    """A crate a case (or a dependency) names with `//@ aux-crate: NAME=FILE`, FILE under
    `auxiliary/` beside the case: its extern name (and crate name), its root file, its edition,
    whether it is a proc-macro crate, and the crates it names in turn."""

    name: str
    path: str
    edition: str
    proc_macro: bool = False
    deps: tuple["Dep", ...] = ()


@dataclass(frozen=True)
class Case:
    """A case: its name (`<area>/<file>`), root file, kind and edition; what its header expects,
    whether its program panics, what it prints (its `.stdout`, None when there is none), the
    crates it depends on, its issue, and, for an area blaze builds twice (`twice`), the
    dependencies the second build refuses rather than serves."""

    name: str
    path: str
    kind: Kind
    edition: str
    expect: Expect
    panics: bool = False
    stdout: str | None = None
    deps: tuple[Dep, ...] = ()
    issue: str = ""
    refused: frozenset[str] = frozenset()
    twice: bool = False


@dataclass(frozen=True)
class Run:
    """A program's run: its exit status, its stdout and its panics' first lines; or why it did
    not finish (`error`), and whether there was a program at all (`started`)."""

    status: int = 0
    stdout: str = ""
    panics: tuple[str, ...] = ()
    error: str | None = None
    started: bool = True


@dataclass(frozen=True)
class Said:
    """What blaze said of a case: the keys of its errors (none: it accepted the case), or why
    nothing can be read from it, and who did the check when it was not blaze."""

    keys: frozenset[str] = frozenset()
    error: str | None = None
    who: str | None = None


@dataclass(frozen=True)
class Batch:
    """What came of the workspace `ws`: what blaze said of each case (by name), the cases that
    could not be laid out as a package (and why), each case's package directory, a daemon that
    would not stop, and what the run that checked each case printed on stdout."""

    ws: str
    said: dict[str, Said] = field(default_factory=dict[str, Said])
    out: dict[str, str] = field(default_factory=dict[str, str])
    packages: dict[str, str] = field(default_factory=dict[str, str])
    stopped: str | None = None
    stdout: dict[str, str] = field(default_factory=dict[str, str])


@dataclass(frozen=True)
class Judged:
    case: Case
    verdict: report.Verdict
    category: Category
    detail: str
    package: str = ""


def key_of(code: str | None, message: str) -> str:
    """What an error is matched by: its code, or the start of its message with what is in
    backticks taken out (names and paths, which blaze writes its own way)."""
    if code:
        return code
    first = message.splitlines()[0] if message else ""
    return MESSAGE + re.sub(r"`[^`]*`", "`_`", first)[:MESSAGE_START]


def shown_key(key: str) -> str:
    """A key as a header writes it: a code or lint as it is, a message's start in quotes."""
    return f'"{key[len(MESSAGE):]}"' if key.startswith(MESSAGE) else key


def tokens_of(value: str) -> tuple[str, ...]:
    """A header's error list as keys: a quoted message start as its key."""
    return tuple(key_of(None, m[1]) if m[1] else m[2] for m in TOKEN.finditer(value))


def covers(token: str, key: str) -> bool:
    if token == key:
        return True
    return token.startswith(MESSAGE) and key.startswith(MESSAGE) and token[len(MESSAGE):] in key[len(MESSAGE):]


def header_holds(tokens: Sequence[str], keys: frozenset[str]) -> bool:
    """Whether the header names every error blaze gave, and nothing else."""
    return bool(keys) and all(any(covers(t, k) for k in keys) for t in tokens) and all(
        any(covers(t, k) for t in tokens) for k in keys)


def expect_of(first_line: str) -> Expect | str:
    """The header of a case's first line, or what is wrong with it."""
    m = EXPECT.fullmatch(first_line.rstrip())
    if not m:
        return "its first line is not `//@ expect: accept` or `//@ expect: reject E0xxx[, E0yyy]`"
    if m[1] == "accept":
        return Expect(True)
    tokens = tokens_of(m[2])
    if not tokens:
        return f"`//@ expect: {m[1]}` names no error"
    return Expect(False, tokens)


def directives(text: str) -> list[tuple[str, str]]:
    return [(m["name"], m["value"].strip()) for m in DIRECTIVE.finditer(text)]


def edition_of(value: str) -> str | None:
    return value if value in EDITIONS else None


def aux_pairs(value: str) -> tuple[str, str] | str:
    got = AUX_VALUE.fullmatch(value)
    if got is None:
        return f"`//@ aux-crate: {value}` is not `//@ aux-crate: NAME=FILE.rs`"
    return got[1], got[2]


def deps_of(pairs: Sequence[tuple[str, str]], aux: Path, seen: tuple[str, ...] = ()) -> tuple[Dep, ...] | str:
    """The crates `pairs` name, each read from `aux` (with those it names), or what is wrong."""
    found: list[Dep] = []
    for name, file in pairs:
        if file.startswith(("/", "..")) or "/../" in file or "\\" in file:
            return f"the dependency {name}={file} is outside {AUXILIARY}/"
        if any(d.name == name for d in found):
            return f"two dependencies are named {name}"
        path = aux / file
        if str(path) in seen:
            return f"its dependencies form a cycle through {file}"
        if not path.is_file():
            return f"names the dependency {name}={file}, and there is no {path}"
        text = path.read_text(errors="replace")
        edition = "2015"
        inner: list[tuple[str, str]] = []
        for directive, value in directives(text):
            if directive in AUX_INERT:
                continue
            if directive not in DEP_DIRECTIVES:
                return f"{AUXILIARY}/{file}: a dependency takes `//@ edition:` and `//@ aux-crate:` alone, not `{directive}`"
            if directive == "edition":
                year = edition_of(value)
                if year is None:
                    return f"{AUXILIARY}/{file}: edition `{value}`"
                edition = year
            else:
                pair = aux_pairs(value)
                if isinstance(pair, str):
                    return f"{AUXILIARY}/{file}: {pair}"
                inner.append(pair)
        attribute = CRATE_TYPE.search(text)
        kind = attribute[1] if attribute else "lib"
        if kind not in ("lib", "rlib", "proc-macro"):
            return f"{AUXILIARY}/{file}: a dependency of crate type {kind}"
        below = deps_of(inner, aux, (*seen, str(path)))
        if isinstance(below, str):
            return below
        found.append(Dep(name, str(path), edition, kind == "proc-macro", below))
    return tuple(found)


def ordered(deps: Sequence[Dep]) -> list[Dep]:
    """Every crate `deps` names, transitively, each once, each after the crates it names."""
    out: list[Dep] = []

    def visit(dep: Dep) -> None:
        for inner in dep.deps:
            visit(inner)
        if dep not in out:
            out.append(dep)

    for dep in deps:
        visit(dep)
    return out


def case_of(name: str, path: Path, text: str) -> Case | str:
    """The case at `path` named `name`, or what is wrong with it (the case's fault)."""
    expect = expect_of(text.split("\n", 1)[0])
    if isinstance(expect, str):
        return expect
    edition, test, panics, issue = "2015", False, False, ""
    said_crate: str | None = None
    pairs: list[tuple[str, str]] = []
    refused: list[str] = []
    for directive, value in directives(text)[1:]:
        if directive not in CASE_DIRECTIVES:
            return f"`//@ {directive}` is not a directive of cases.py ({', '.join(sorted(CASE_DIRECTIVES))})"
        if directive == "expect":
            return "two `//@ expect:` lines"
        if directive == "edition":
            year = edition_of(value)
            if year is None:
                return f"edition `{value}`"
            edition = year
        elif directive == "compile-flags":
            if value != "--test":
                return f"`//@ compile-flags: {value}`: a case takes `--test` alone"
            test = True
        elif directive == "crate":
            if value not in ("lib", "bin"):
                return f"`//@ crate: {value}`: lib or bin"
            said_crate = value
        elif directive == "run":
            if value != "panic":
                return f"`//@ run: {value}`: `//@ run: panic` is the one run directive"
            panics = True
        elif directive == "aux-crate":
            pair = aux_pairs(value)
            if isinstance(pair, str):
                return pair
            pairs.append(pair)
        elif directive == "refused":
            refused += [word for word in re.split(r"[\s,]+", value) if word]
        else:
            issue = value
    attribute = CRATE_TYPE.search(text)
    crate_type = attribute[1] if attribute else "bin"
    if crate_type not in ("lib", "rlib", "bin"):
        return f"crate type {crate_type}"
    kind: Kind = "test" if test else "lib" if crate_type in ("lib", "rlib") else "bin"
    if said_crate is not None and said_crate != ("bin" if crate_type == "bin" else "lib"):
        return f"`//@ crate: {said_crate}` and its crate type, {crate_type}, disagree"
    if panics and not (expect.accept and kind == "bin"):
        return "`//@ run: panic` is for a program the header accepts"
    if UNCOPIED.search(text):
        return "names a file with `#[path]` or `include!`, which cases.py does not copy"
    deps = deps_of(pairs, path.parent / AUXILIARY)
    if isinstance(deps, str):
        return deps
    every = ordered(deps)
    if len({d.name for d in every}) != len(every):
        return "two of its dependencies have one name"
    area = name.split("/", 1)[0]
    twice = area in TWICE
    if twice and not deps:
        return f"a case of {area} names a dependency (`//@ aux-crate:`) for blaze to serve"
    if refused and not twice:
        return f"`//@ refused:` is for the areas blaze builds twice ({', '.join(sorted(TWICE))})"
    if unknown := sorted(set(refused) - {d.name for d in every}):
        return f"`//@ refused:` names {', '.join(unknown)}, which it does not depend on"
    printed = path.with_suffix(".stdout")
    stdout = printed.read_text() if printed.is_file() else None
    if stdout is not None and not (expect.accept and kind == "bin"):
        return f"{printed.name} is beside a case that is not a program the header accepts"
    return Case(name, str(path), kind, edition, expect, panics, stdout, deps, issue, frozenset(refused), twice)


def discover(root: Path, areas: Sequence[str], needle: str) -> tuple[list[Case], list[tuple[str, str]]]:
    """The cases under `root` (of `areas`, every one when empty, whose name has `needle`), and
    each file that is not a case with why."""
    cases: list[Case] = []
    wrong: list[tuple[str, str]] = []
    for area in sorted(p for p in root.iterdir() if p.is_dir() and not p.name.startswith((".", "_"))):
        if areas and area.name not in areas:
            continue
        for path in sorted(area.glob("*.rs")):
            name = f"{area.name}/{path.name}"
            if needle not in name:
                continue
            got = case_of(name, path, path.read_text(errors="replace"))
            if isinstance(got, str):
                wrong.append((name, got))
            else:
                cases.append(got)
        for printed in sorted(area.glob("*.stdout")):
            if not printed.with_suffix(".rs").is_file():
                wrong.append((f"{area.name}/{printed.name}", "no case beside it"))
    missing = sorted(set(areas) - {p.name for p in root.iterdir() if p.is_dir()}) if areas else []
    wrong += [(f"{area}/", f"no area {area} in {root}") for area in missing]
    return cases, wrong


def copy_modules(source: Path, dest: Path, root: bool) -> str | None:
    """Copies the files `source` names with `mod NAME;` to the same place beside `dest` (a root
    file's or `mod.rs`'s modules beside it, another file's in the directory named after it), and
    theirs in turn; the reason when one is not there."""
    text = source.read_text(errors="replace")
    inner = "" if root or source.name == "mod.rs" else source.stem
    for m in MODULE.finditer(text):
        found = [p for p in (Path(inner) / f"{m['name']}.rs", Path(inner) / m["name"] / "mod.rs")
                 if (source.parent / p).is_file()]
        if not found:
            return f"names the module `{m['name']}`, and there is no {m['name']}.rs or {m['name']}/mod.rs"
        origin, copy = source.parent / found[0], dest.parent / found[0]
        if copy.exists():
            continue
        copy.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(origin, copy)
        if why := copy_modules(origin, copy, False):
            return why
    return None


def root_of(ws: Path, member: str, case: Case) -> Path:
    """The root file of the package `member` of `case` in the workspace `ws`."""
    return ws / "crates" / member / "src" / ("main.rs" if case.kind == "bin" else "lib.rs")


def manifest(member: str, case: Case) -> str:
    target = (f'[[bin]]\nname = "{member}"\npath = "src/main.rs"\n' if case.kind == "bin"
              else '[lib]\npath = "src/lib.rs"\ndoctest = false\n')
    return (f'[package]\nname = "{member}"\nversion = "0.0.0"\nedition = "{case.edition}"\nautobins = false\n'
            f"autoexamples = false\nautotests = false\nautobenches = false\n\n{target}")


def package_of(member: str, dep: Dep) -> str:
    """The vendored package of `dep` of the member `member`: one a member, as no two cases share one."""
    return f"{member}_{dep.name}"


def checksums(package: Path) -> dict[str, str]:
    """The sha256 of each file of a vendored package but its checksum file, as cargo's
    `DirectorySource` checks them."""
    return {path.relative_to(package).as_posix(): hashlib.sha256(path.read_bytes()).hexdigest()
            for path in sorted(package.rglob("*")) if path.is_file() and path.name != CHECKSUMS}


def dependencies(member: str, deps: Sequence[Dep]) -> str:
    """The `[dependencies]` of a member or a vendored package: each by its extern name, the
    package it is renamed from pinned."""
    if not deps:
        return ""
    return "\n[dependencies]\n" + "".join(
        f'{d.name} = {{ package = "{package_of(member, d)}", version = "={VERSION}" }}\n' for d in deps)


def dep_manifest(member: str, dep: Dep) -> str:
    lib = f'[lib]\nname = "{dep.name}"\npath = "src/lib.rs"\ndoctest = false\n'
    if dep.proc_macro:
        lib += "proc-macro = true\n"
    return (f'[package]\nname = "{package_of(member, dep)}"\nversion = "{VERSION}"\nedition = "{dep.edition}"\n'
            f"autobins = false\nautoexamples = false\nautotests = false\nautobenches = false\n\n{lib}"
            + dependencies(member, dep.deps))


def vendor(ws: Path, member: str, case: Case) -> str | None:
    """Each dependency of `case` written as a crates.io package vendored in `ws/vendor` (a
    `directory` source replacing crates.io: a registry package, never a member or a path
    package), and the member's `[dependencies]`; why when one cannot be laid out."""
    for dep in ordered(case.deps):
        package = ws / "vendor" / package_of(member, dep)
        (package / "src").mkdir(parents=True)
        root = package / "src" / "lib.rs"
        shutil.copyfile(dep.path, root)
        if why := copy_modules(Path(dep.path), root, True):
            return f"its dependency {dep.name} {why}"
        (package / "Cargo.toml").write_text(dep_manifest(member, dep))
        (package / CHECKSUMS).write_text(json.dumps({"files": checksums(package), "package": None}, sort_keys=True))
    with open(ws / "crates" / member / "Cargo.toml", "a") as written:
        written.write(dependencies(member, case.deps))
    (ws / ".cargo").mkdir(exist_ok=True)
    (ws / ".cargo" / "config.toml").write_text(CONFIG)
    return None


def write_workspace(ws: Path, index: int, cases: Sequence[Case]) -> tuple[dict[str, Case], dict[str, str]]:
    """Writes a package of each case under `ws/crates` (and its dependencies under `ws/vendor`):
    the members by name, and the cases that cannot be laid out, with why."""
    members: dict[str, Case] = {}
    out: dict[str, str] = {}
    for i, case in enumerate(cases):
        member = f"u{index}_{i}"
        crate = ws / "crates" / member
        (crate / "src").mkdir(parents=True)
        root = root_of(ws, member, case)
        shutil.copyfile(case.path, root)
        (crate / "Cargo.toml").write_text(manifest(member, case))
        why = copy_modules(Path(case.path), root, True)
        if why is None and case.deps:
            why = vendor(ws, member, case)
        if why:
            shutil.rmtree(crate)
            out[case.name] = why
            continue
        members[member] = case
    (ws / "Cargo.toml").write_text(WORKSPACE)
    return members, out


def json_line(line: str) -> dict[str, Any] | None:
    if not line.startswith("{"):
        return None
    try:
        value: Any = json.loads(line)
    except ValueError:
        return None
    return cast(dict[str, Any], value) if isinstance(value, dict) else None


def error_key(diagnostic: Any) -> str | None:
    """The key of a diagnostic (rustc's JSON shape) that is an error; None for anything else."""
    if not isinstance(diagnostic, dict):
        return None
    d = cast(dict[str, Any], diagnostic)
    message = d.get("message")
    if d.get("level") != "error" or not isinstance(message, str) or message.startswith("aborting due to"):
        return None
    code = d.get("code")
    name: Any = cast(dict[str, Any], code).get("code") if isinstance(code, dict) else None
    return key_of(name if isinstance(name, str) else None, message)


@dataclass(frozen=True)
class Messages:
    """Of `blaze check --message-format=json`'s lines: the keys of the errors of each target (by
    its name), the keys of every error, and the targets checked (a `compiler-artifact`)."""

    errors: dict[str, frozenset[str]]
    every: frozenset[str]
    checked: frozenset[str]


def messages_of(stdout: str) -> Messages:
    errors: dict[str, set[str]] = {}
    every: set[str] = set()
    checked: set[str] = set()
    for line in stdout.splitlines():
        m = json_line(line)
        if m is None:
            continue
        target: Any = m.get("target")
        name: Any = cast(dict[str, Any], target).get("name") if isinstance(target, dict) else None
        if m.get("reason") == "compiler-message" and (key := error_key(m.get("message"))):
            every.add(key)
            if isinstance(name, str):
                errors.setdefault(name, set()).add(key)
        elif m.get("reason") == "compiler-artifact" and isinstance(name, str):
            checked.add(name)
    return Messages({k: frozenset(v) for k, v in errors.items()}, frozenset(every), frozenset(checked))


def said_alone(returncode: int, stdout: str, tail: str, who: str | None, command: str = "check") -> Said:
    """What a `blaze check -p` (or `build -p`) of one case's package says of it: every error of
    the run is its."""
    m = messages_of(stdout)
    if m.every:
        return Said(m.every, who=who)
    if returncode == 0:
        return Said(who=who)
    return Said(error=f"`blaze {command}` exited {returncode} with no error of the case:\n{tail}")


def said_together(stdout: str, members: Sequence[str], tail: str, who: str | None,
                  command: str = "check") -> dict[str, Said]:
    """What one `blaze check --workspace` says of each member: its errors; accepted only when
    it was checked (a `compiler-artifact` for it), never for having said nothing."""
    m = messages_of(stdout)
    said: dict[str, Said] = {}
    for member in members:
        if m.errors.get(member):
            said[member] = Said(m.errors[member], who=who)
        elif member in m.checked:
            said[member] = Said(who=who)
        else:
            said[member] = Said(error=f"`blaze {command}` said nothing of {member}: no error, no artifact:\n{tail}")
    return said


def check_args(kind: Kind, member: str | None) -> list[str]:
    """`blaze check`'s (or `build`'s) arguments for one member (`-p`), or for the whole
    workspace (None)."""
    picked = ["-p", member] if member is not None else ["--workspace", "--keep-going"]
    return [*picked, *KIND_ARGS[kind], "--message-format=json"]


def batches_of(cases: Sequence[Case], size: int) -> list[list[Case]]:
    """The workspaces: cases of one kind built as often (once, or `twice`), `size` at most."""
    groups: dict[tuple[Kind, bool], list[Case]] = {}
    for case in cases:
        groups.setdefault((case.kind, case.twice), []).append(case)
    return [group[i:i + size] for group in groups.values() for i in range(0, len(group), size)]


def run_batch(eng: engine.Engine, index: int, cases: Sequence[Case], work: Path, store: Path, whole: bool) -> Batch:
    """`blaze check` (or `build`, of programs, to be run) of a workspace of `cases`, under its
    own daemon; the daemon is stopped at the end."""
    ws, home = work / "ws" / str(index), work / "h" / str(index)
    shutil.rmtree(ws, ignore_errors=True)
    members, out = write_workspace(ws, index, cases)
    packages = {case.name: str(ws / "crates" / member) for member, case in members.items()}
    if not members:
        return Batch(str(ws), out=out, packages=packages)
    kind = cases[0].kind
    command = "build" if kind == "bin" else "check"
    env = eng.env(home, store, extra={"RUSTFLAGS": " ".join(COMMON)})
    said: dict[str, Said] = {}
    printed: dict[str, str] = {}
    stopped: str | None = None
    try:
        if whole:
            ran = eng.build(f"blaze {command} of workspace {index}", ws, env, check_args(kind, None),
                            command=command, timeout=CHECK_SECS)
            got = said_together(ran.stdout, list(members), ran.said(600), eng.not_blazes(ran), command)
            for member, case in members.items():
                said[case.name] = got[member]
                printed[case.name] = ran.stdout
        else:
            for member, case in members.items():
                ran = eng.build(f"blaze {command} of {case.name}", ws, env, check_args(kind, member),
                                command=command, timeout=CHECK_SECS)
                said[case.name] = said_alone(ran.returncode, ran.stdout, ran.said(600), eng.not_blazes(ran), command)
                printed[case.name] = ran.stdout
    except (engine.Unmeasurable, engine.Failed) as e:
        why = f"unmeasured: {e}" if isinstance(e, engine.Unmeasurable) else str(e)
        for case in members.values():
            said.setdefault(case.name, Said(error=why))
    finally:
        try:
            eng.stop(ws, env)
        except engine.Failed as e:
            stopped = str(e)
    return Batch(str(ws), said, out, packages, stopped, printed)


def panics_of(stderr: str) -> tuple[str, ...]:
    """Each panic `stderr` reports, by thread, place and message, sorted: threads panic in any order."""
    found: list[str] = []
    for m in PANIC.finditer(stderr):
        thread, at, colon, message = m.groups()
        found.append(f"{thread} at {at}: {message}" if colon and message is not None else f"{thread} at {at}")
    return tuple(sorted(found))


def run_program(program: Path, cwd: Path, env: dict[str, str]) -> Run:
    """`program` run in `cwd` with no input."""
    if not program.is_file():
        return Run(error=f"no program at {program}", started=False)
    cwd.mkdir(parents=True, exist_ok=True)
    try:
        ran = engine.run(f"running {program}", [str(program)], cwd, env, RUN_SECS)
    except engine.Failed as e:
        return Run(error=str(e).splitlines()[0])
    return Run(ran.returncode, ran.stdout, panics_of(ran.stderr))


def run_wrong(case: Case, ran: Run) -> str | None:
    """None when the program ran as the case says; else how it did not."""
    if ran.error is not None:
        return f"its program does not finish: {ran.error}"
    if case.panics:
        if ran.status != PANICKED or not ran.panics:
            return f"the case says it panics; its program exits {ran.status} with {len(ran.panics)} panics"
    elif ran.status != 0 or ran.panics:
        return f"its program exits {ran.status}" + (f", panicking: {'; '.join(ran.panics[:3])}" if ran.panics else "")
    if case.stdout is not None and ran.stdout != case.stdout:
        want, got = case.stdout.splitlines(), ran.stdout.splitlines()
        for line, (x, y) in enumerate(zip(want, got), 1):
            if x != y:
                return f"stdout differs from its .stdout at line {line}: wants {x!r:.200}, prints {y!r:.200}"
        return f"stdout differs from its .stdout: wants {len(want)} lines, prints {len(got)}"
    return None


def judged(case: Case, said: Said, ran: Run | None) -> tuple[report.Verdict, Category, str]:
    """blaze's answer (and its program's run) against the case's header."""
    if said.error is not None:
        return "error", "error", said.error
    if said.who is not None:
        return "fail", "not blaze's", f"not blaze's: {said.who}"
    gave = ", ".join(shown_key(k) for k in sorted(said.keys))
    if not case.expect.accept:
        if not said.keys:
            return "fail", "accepted what the case rejects", f"accepted; the case says {case.expect}"
        if not header_holds(case.expect.tokens, said.keys):
            return "fail", "other codes", f"the case says {case.expect}; blaze gives {gave}"
        return "pass", "as expected", f"rejected with {gave}"
    if said.keys:
        return "fail", "rejected what the case accepts", f"rejected what the case accepts: blaze gives {gave}"
    if case.kind != "bin":
        return "pass", "as expected", "accepted"
    if ran is None or not ran.started:
        return "error", "error", f"blaze said it built the program and there is none: {ran.error if ran else '-'}"
    if (wrong := run_wrong(case, ran)) is not None:
        return "fail", "other run", wrong
    return "pass", "as expected", "accepted; its program " + ("panics" if case.panics else "exits 0")


def served_lines(stdout: str) -> dict[str, dict[str, Any]]:
    """The `blaze-served` lines of a run's stdout, by package."""
    found: dict[str, dict[str, Any]] = {}
    for line in stdout.splitlines():
        m = json_line(line)
        if m is not None and m.get("reason") == SERVED and isinstance(m.get("package"), str):
            found[m["package"]] = m
    return found


def serving(case: Case, member: str, stdout: str) -> tuple[report.Verdict, Category, str] | None:
    """None when the second build of `case` (the member `member`) served each of its
    dependencies but those its `//@ refused:` names, which it refused; else what it did."""
    lines = served_lines(stdout)
    for dep in ordered(case.deps):
        package = package_of(member, dep)
        line = lines.get(package)
        if line is None:
            return "error", "error", f"the second build printed no `{SERVED}` line of the dependency {dep.name} ({package})"
        outcome, bodies, why = line.get("outcome"), line.get("bodies"), line.get("why")
        if outcome not in WAYS or bodies not in WAYS or not (why is None or isinstance(why, str)):
            return "error", "error", f"a `{SERVED}` line of another shape: {line}"
        shown = f"outcome {outcome}, bodies {bodies}" + (f": {why}" if why else "")
        both = outcome == "served" and bodies == "served"
        if dep.name in case.refused:
            if both:
                return "fail", "served what is refused", f"the second build served {dep.name}, which the case says is refused"
            if "refused" not in (outcome, bodies):
                return "fail", "not served", f"the second build neither served nor refused {dep.name}: {shown}"
        elif not both:
            return "fail", "not served", f"the second build did not serve {dep.name}: {shown}"
    return None


class Runner:
    """Builds cases in workspaces of `batch` under `work`, `jobs` at once, and judges each."""

    def __init__(self, eng: engine.Engine, work: Path, jobs: int = 4, batch: int = 100, whole: bool = False,
                 keep: bool = False):
        self.eng, self.work, self.jobs, self.batch, self.whole, self.keep = eng, work, jobs, batch, whole, keep
        self.env = engine.scrubbed()
        self.indices = itertools.count()
        self.stopped: list[tuple[str, str]] = []

    def judge(self, cases: Sequence[Case]) -> list[Judged]:
        """Each case judged, in their order. Names are unique."""
        names = [c.name for c in cases]
        if len(set(names)) != len(names):
            raise engine.Failed(f"two cases of one name: {sorted(Counter(names) - Counter(set(names)))[:3]}")
        chunks = [(next(self.indices), chunk) for chunk in batches_of(cases, self.batch)]
        results: dict[str, Judged] = {}
        with concurrent.futures.ThreadPoolExecutor(self.jobs) as pool:
            running = [pool.submit(self.side, i, chunk) for i, chunk in chunks]
            # In the order the workspaces were made, not the order they end in: one report for one input.
            for done in running:
                for found in done.result():
                    results[found.case.name] = found
        return [results[name] for name in names]

    def side(self, index: int, cases: list[Case]) -> list[Judged]:
        """blaze on a workspace of `cases`, each judged; `twice` cases built a second time with a
        fresh daemon, workspace and target and the same store, judged again, and held to serve
        each case's dependencies (`serving`)."""
        batch = run_batch(self.eng, index, cases, self.work, self.work / "store", self.whole)
        found = self.judged_batch(batch, cases)
        if cases[0].twice:
            shutil.rmtree(self.work / "h" / str(index), ignore_errors=True)
            second = run_batch(self.eng, index, cases, self.work, self.work / "store", self.whole)
            again = {j.case.name: j for j in self.judged_batch(second, cases)}
            found = [self.second(first, again[first.case.name], second) for first in found]
            batch = second
        if not self.keep and all(f.verdict == "pass" for f in found):
            shutil.rmtree(batch.ws, ignore_errors=True)
        return found

    def second(self, first: Judged, again: Judged, batch: Batch) -> Judged:
        """A case built twice: the first build's verdict if it did not pass, else the second's,
        else whether the second served what it should."""
        if first.verdict != "pass":
            return replace(first, detail=f"first build: {first.detail}")
        if again.verdict != "pass":
            return replace(again, detail=f"second build: {again.detail}")
        name = again.case.name
        problem = serving(again.case, Path(batch.packages[name]).name, batch.stdout.get(name, ""))
        if problem is None:
            return replace(again, detail=f"{again.detail}; served on the second build")
        verdict, category, detail = problem
        return replace(again, verdict=verdict, category=category, detail=detail)

    def judged_batch(self, batch: Batch, cases: Sequence[Case]) -> list[Judged]:
        if batch.stopped is not None:
            self.stopped.append((batch.ws, batch.stopped))
        ws = Path(batch.ws)
        found: list[Judged] = []
        for case in cases:
            if case.name in batch.out:
                found.append(Judged(case, "error", "case wrong", f"cannot be laid out as a package: {batch.out[case.name]}"))
                continue
            said = batch.said.get(case.name, Said(error="blaze was never asked of it"))
            package = batch.packages[case.name]
            ran = None
            if case.kind == "bin" and case.expect.accept and not said.keys and said.error is None and said.who is None:
                member = Path(package).name
                ran = run_program(ws / "target" / "debug" / (member + engine.EXE), self.work / "cwd", self.env)
            verdict, category, detail = judged(case, said, ran)
            if case.issue:
                detail += f"; {case.issue}"
            found.append(Judged(case, verdict, category, detail, package))
        return found


def main(args: Input) -> None:
    for name, value in (("jobs", args.jobs), ("batch", args.batch)):
        if value < 1:
            fail(SCRIPT, f"{name}: at least 1, not {value}")
    root = Path(args.dir).resolve() if args.dir else CASES
    if not root.is_dir():
        fail(SCRIPT, f"dir: no directory {root}")
    try:
        eng = engine.Engine.of(args.engine)
    except engine.Failed as e:
        fail(SCRIPT, str(e))
    cases, wrong = discover(root, args.areas, args.filter)
    if not cases and not wrong:
        fail(SCRIPT, f"no case under {root} for areas {list(args.areas)} and filter {args.filter!r}")
    work = Path(args.work or tempfile.mkdtemp(prefix="bz-cases-", dir="/tmp")).resolve()
    work.mkdir(parents=True, exist_ok=True)
    print(f"work: {work}", flush=True)
    rep = report.Report(SCRIPT, args.engine, args.out)
    for name, why in wrong:
        rep.add(name, "error", f"case wrong: {why}")
    try:
        runner = Runner(eng, work, args.jobs, args.batch, args.whole, args.keep)
        found = runner.judge(cases)
    except engine.Failed as e:
        fail(SCRIPT, str(e))
    by_area: dict[str, Counter[Category]] = {}
    for j in found:
        by_area.setdefault(j.case.name.split("/", 1)[0], Counter())[j.category] += 1
        detail = j.detail if j.verdict == "pass" or not j.package else f"{j.detail}\npackage: {j.package}"
        rep.add(j.case.name, j.verdict, detail)
    for ws, why in runner.stopped:
        rep.add(f"stopping the daemon of {ws}", "error", why)
    rep.extra.update(work=str(work), dir=str(root), cases=len(cases), wrong=len(wrong),
                     areas={area: dict(counts) for area, counts in sorted(by_area.items())})
    if not args.keep:
        shutil.rmtree(work / "store", ignore_errors=True)
        shutil.rmtree(work / "h", ignore_errors=True)
    rep.finish()


def test_expect() -> None:
    assert expect_of("//@ expect: accept") == Expect(True)
    assert expect_of("//@ expect: reject E0425") == Expect(False, ("E0425",))
    assert expect_of("//@ expect: reject E0425, E0433\n") == Expect(False, ("E0425", "E0433"))
    assert expect_of('//@ expect: reject arithmetic_overflow, "cannot find macro"') == Expect(
        False, ("arithmetic_overflow", MESSAGE + "cannot find macro"))
    for bad in ("// expect: accept", "//@ expect: rejects E0425", "//@ expect: reject", "fn main() {}"):
        assert isinstance(expect_of(bad), str), bad
    assert str(Expect(False, ("E0425", MESSAGE + "cannot find macro"))) == 'reject E0425, "cannot find macro"'
    assert expect_of(f"//@ expect: {Expect(False, ('E0412', MESSAGE + 'x y'))}") == \
        Expect(False, ("E0412", MESSAGE + "x y")), "a header written from an Expect reads back"


def test_keys() -> None:
    assert key_of("E0277", "anything") == "E0277"
    assert key_of(None, "`AtomicU32::load` is not yet stable as a const fn") == \
        "message: `_` is not yet stable as a const fn"
    assert key_of(None, "x" * 100) == MESSAGE + "x" * MESSAGE_START
    tokens = tokens_of('E0277, "is not yet stable as a const fn"')
    assert tokens == ("E0277", "message: is not yet stable as a const fn"), tokens
    keys = frozenset({"E0277", "message: `_` is not yet stable as a const fn"})
    assert header_holds(tokens, keys)
    assert not header_holds(tokens[:1], keys), "a header that leaves out an error blaze gives holds"
    assert not header_holds(("E0277", "E0308"), frozenset({"E0277"})), "a header naming more holds"
    assert not header_holds(("E0277",), frozenset()), "a header holds of what blaze accepts"


def test_case_of() -> None:
    lib = case_of("a/l.rs", Path("/c/a/l.rs"), '//@ expect: accept\n//@ crate: lib\n#![crate_type = "lib"]\npub fn f() {}\n')
    assert isinstance(lib, Case) and lib.kind == "lib" and lib.name == "a/l.rs", lib
    old = case_of("a/o.rs", Path("/c/a/o.rs"), "//@ expect: reject E0433\n//@ edition: 2018\nfn main() {}\n")
    assert isinstance(old, Case) and old.edition == "2018" and old.kind == "bin", old
    test = case_of("a/t.rs", Path("/c/a/t.rs"), "//@ expect: reject E0063\n//@ compile-flags: --test\n//@ issue: #1522\n")
    assert isinstance(test, Case) and test.kind == "test" and test.issue == "#1522", test
    panics = case_of("a/p.rs", Path("/c/a/p.rs"), "//@ expect: accept\n//@ run: panic\nfn main() { panic!() }\n")
    assert isinstance(panics, Case) and panics.panics, panics
    for text in ("//@ expect: accept\n//@ revisions: a b\nfn main() {}\n",
                 "//@ expect: accept\n//@ aux-build: h.rs\nfn main() {}\n",
                 "fn main() {}\n//@ expect: accept\n",
                 "//@ expect: accept\n//@ compile-flags: -O\nfn main() {}\n",
                 "//@ expect: accept\n//@ crate: lib\nfn main() {}\n",
                 "//@ expect: reject E0425\n//@ run: panic\nfn main() {}\n",
                 '//@ expect: accept\n//@ run: panic\n#![crate_type = "lib"]\n',
                 '//@ expect: accept\ninclude!("x.rs");\nfn main() {}\n',
                 "//@ expect: accept\n//@ edition: 2019\nfn main() {}\n"):
        assert isinstance(case_of("a/x.rs", Path("/c/a/x.rs"), text), str), text


def test_tree() -> None:
    """Every file of cases/ is a case, and the areas have their cases."""
    cases, wrong = discover(CASES, (), "")
    assert not wrong, wrong[:5]
    names = Counter(c.name.split("/", 1)[0] for c in cases)
    assert names["nameres"] >= 40 and names["rejects"] >= 10, names
    codes = {t for c in cases if c.name.startswith("nameres/") and not c.expect.accept for t in c.expect.tokens}
    for code in ("E0425", "E0432", "E0433", "E0412", "E0659", "E0603", "E0252", "E0255"):
        assert code in codes, f"no nameres case rejects with {code}"
    editions = {c.edition for c in cases if c.name.startswith("nameres/")}
    assert {"2015", "2018"} <= editions, editions
    assert any(c.expect.accept for c in cases if c.name.startswith("nameres/"))
    rejects = {Path(c.path).name: c for c in cases if c.name.startswith("rejects/")}
    assert rejects["missing-field-in-test.rs"].kind == "test" and rejects["missing-field-in-test.rs"].expect.tokens == ("E0063",)
    assert rejects["const-atomic-load.rs"].expect.tokens == (MESSAGE + "is not yet stable as a const fn",)
    assert {"#1522", "#1440"} <= {c.issue for c in rejects.values()}
    assert all(c.panics for c in cases if c.name == "bodies-cache/048-a-dependency-that-panics.rs")


def test_messages() -> None:
    def diagnostic(level: str, message: str, code: str | None = None) -> dict[str, Any]:
        return {"level": level, "message": message, "code": {"code": code} if code else None, "spans": []}

    def line(reason: str, target: str, body: dict[str, Any] | None = None) -> str:
        value: dict[str, Any] = {"reason": reason, "target": {"name": target, "kind": ["lib"]}}
        if body is not None:
            value["message"] = body
        return json.dumps(value)

    stdout = "\n".join([line("compiler-message", "u0_0", diagnostic("error", "missing field", "E0063")),
                        line("compiler-message", "u0_0", diagnostic("warning", "unused", "dead_code")),
                        line("compiler-message", "u0_0", diagnostic("error", "aborting due to 1 previous error")),
                        line("compiler-artifact", "u0_1"), '{"reason":"build-finished","success":false}', "not json"])
    m = messages_of(stdout)
    assert m.errors == {"u0_0": frozenset({"E0063"})} and m.every == frozenset({"E0063"}) and m.checked == {"u0_1"}
    together = said_together(stdout, ["u0_0", "u0_1", "u0_2"], "", None)
    assert together["u0_0"].keys == {"E0063"} and together["u0_1"] == Said() and together["u0_2"].error
    assert said_alone(101, stdout, "", None).keys == {"E0063"}
    assert said_alone(0, "", "", None) == Said()
    assert "`blaze build` exited" in str(said_alone(101, "", "", None, "build").error)
    assert check_args("lib", "u0_0") == ["-p", "u0_0", "--lib", "--message-format=json"]
    assert check_args("test", None) == ["--workspace", "--keep-going", "--tests", "--message-format=json"]


def program(accept: bool = True, tokens: tuple[str, ...] = (), panics: bool = False, stdout: str | None = None,
            kind: Kind = "bin") -> Case:
    return Case("a/t.rs", "/c/a/t.rs", kind, "2021", Expect(accept, tokens), panics, stdout)


def test_judged() -> None:
    rejects = program(False, ("E0425",))
    assert judged(rejects, Said(frozenset({"E0425"})), None)[:2] == ("pass", "as expected")
    assert judged(rejects, Said(), None)[1] == "accepted what the case rejects"
    assert judged(rejects, Said(frozenset({"E0412"})), None)[1] == "other codes"
    assert judged(rejects, Said(frozenset({"E0425", "E0412"})), None)[1] == "other codes"
    assert judged(rejects, Said(error="timed out"), None)[:2] == ("error", "error")
    assert judged(rejects, Said(frozenset({"E0425"}), who="cargo"), None)[1] == "not blaze's"
    lib = program(kind="lib")
    assert judged(lib, Said(), None)[:2] == ("pass", "as expected")
    result, category, detail = judged(lib, Said(frozenset({"E0425"})), None)
    assert (result, category) == ("fail", "rejected what the case accepts") and "E0425" in detail
    plain = program()
    assert judged(plain, Said(), Run(0, "1\n"))[:2] == ("pass", "as expected")
    assert judged(plain, Said(), Run(1, ""))[1] == "other run"
    assert judged(plain, Said(), Run(101, "", ("main at t.rs:1:1: x",)))[1] == "other run"
    assert judged(plain, Said(), Run(error="took more than 10 s"))[1] == "other run"
    assert judged(plain, Said(), Run(error="no program", started=False))[:2] == ("error", "error")
    assert judged(plain, Said(), None)[:2] == ("error", "error")
    panics = program(panics=True)
    assert judged(panics, Said(), Run(101, "", ("main at t.rs:1:1: x",)))[:2] == ("pass", "as expected")
    assert judged(panics, Said(), Run(0, ""))[1] == "other run"
    assert judged(panics, Said(), Run(101, ""))[1] == "other run", "an exit of 101 with no panic"
    printed = program(stdout="1\n2\n")
    assert judged(printed, Said(), Run(0, "1\n2\n"))[:2] == ("pass", "as expected")
    assert "line 2" in judged(printed, Said(), Run(0, "1\n3\n"))[2]
    assert "2 lines, prints 1" in judged(printed, Said(), Run(0, "1\n"))[2]


def test_run_program() -> None:
    with tempfile.TemporaryDirectory() as tmp:
        root = Path(tmp)
        missing = run_program(root / "nothing", root / "cwd", engine.scrubbed())
        assert not missing.started and missing.error, missing
        if sys.platform != "win32":
            script = root / "p"
            script.write_text("#!/bin/sh\necho one\necho \"thread 'main' panicked at t.rs:2:3:\" >&2\n"
                              "echo boom >&2\nexit 101\n")
            script.chmod(0o755)
            ran = run_program(script, root / "cwd", engine.scrubbed())
            assert ran == Run(101, "one\n", ("main at t.rs:2:3: boom",)), ran


def dep_tree(root: Path) -> Path:
    """An area `bodies-cache` under `root` whose `auxiliary/` has `dep.rs` (2021, naming `pm`,
    a proc-macro crate, and a module `m`); its case's path."""
    aux = root / "bodies-cache" / AUXILIARY
    aux.mkdir(parents=True)
    (aux / "dep.rs").write_text("//@ edition: 2021\n//@ aux-crate: pm=pm.rs\nmod m;\npub fn f() -> u8 { pm::two!() }\n")
    (aux / "m.rs").write_text("pub fn g() {}\n")
    (aux / "pm.rs").write_text('//@ edition: 2021\n//@ force-host\n#![crate_type = "proc-macro"]\n')
    case = root / "bodies-cache" / "c.rs"
    case.write_text("//@ expect: accept\n//@ edition: 2021\n//@ aux-crate: dep=dep.rs\n//@ refused: pm\n"
                    "fn main() { println!(\"{}\", dep::f()); }\n")
    return case


def test_deps() -> None:
    with tempfile.TemporaryDirectory() as tmp:
        root = Path(tmp)
        path = dep_tree(root)
        case = case_of("bodies-cache/c.rs", path, path.read_text())
        assert isinstance(case, Case), case
        assert case.twice and case.refused == {"pm"} and case.edition == "2021" and case.kind == "bin"
        (dep,) = case.deps
        assert (dep.name, dep.edition, dep.proc_macro) == ("dep", "2021", False), dep
        assert [(d.name, d.proc_macro) for d in ordered(case.deps)] == [("pm", True), ("dep", False)]
        cases, wrong = discover(root, (), "")
        assert [c.name for c in cases] == ["bodies-cache/c.rs"] and not wrong, (cases, wrong)

        def wrong_with(text: str, name: str = "bodies-cache/w.rs") -> str:
            at = root / name
            at.parent.mkdir(parents=True, exist_ok=True)
            at.write_text("//@ expect: accept\n//@ edition: 2021\n" + text + "fn main() {}\n")
            got = case_of(name, at, at.read_text())
            assert isinstance(got, str), (text, got)
            return got

        assert "no " in wrong_with("//@ aux-crate: x=missing.rs\n")
        assert "outside" in wrong_with("//@ aux-crate: x=../dep.rs\n")
        assert "not `//@ aux-crate: NAME=FILE.rs`" in wrong_with("//@ aux-crate: dep.rs\n")
        assert "two dependencies" in wrong_with("//@ aux-crate: dep=dep.rs\n//@ aux-crate: dep=m.rs\n")
        assert "does not depend on" in wrong_with("//@ aux-crate: dep=dep.rs\n//@ refused: other\n")
        assert "names a dependency" in wrong_with("")
        (root / "nameres" / AUXILIARY).mkdir(parents=True)
        (root / "nameres" / AUXILIARY / "dep.rs").write_text("pub fn f() {}\n")
        assert "builds twice" in wrong_with("//@ aux-crate: dep=dep.rs\n//@ refused: dep\n", "nameres/w.rs")
        (root / "bodies-cache" / AUXILIARY / "a.rs").write_text("//@ aux-crate: b=b.rs\n")
        (root / "bodies-cache" / AUXILIARY / "b.rs").write_text("//@ aux-crate: a=a.rs\n")
        assert "cycle" in wrong_with("//@ aux-crate: a=a.rs\n")
        (root / "bodies-cache" / AUXILIARY / "flags.rs").write_text("//@ compile-flags: -O\n")
        assert "alone" in wrong_with("//@ aux-crate: f=flags.rs\n")
        plain = root / "nameres" / "p.rs"
        plain.write_text("//@ expect: accept\n//@ aux-crate: dep=dep.rs\nfn main() {}\n")
        once = case_of("nameres/p.rs", plain, plain.read_text())
        assert isinstance(once, Case) and not once.twice and once.deps[0].edition == "2015", once
        (root / "nameres" / "p.stdout").write_text("hi\n")
        printed = case_of("nameres/p.rs", plain, plain.read_text())
        assert isinstance(printed, Case) and printed.stdout == "hi\n", printed
        (root / "nameres" / "lost.stdout").write_text("")
        assert ("nameres/lost.stdout", "no case beside it") in discover(root, ("nameres",), "")[1]


def test_workspace() -> None:
    import tomllib

    with tempfile.TemporaryDirectory() as tmp:
        root = Path(tmp)
        path = dep_tree(root / "cases")
        case = case_of("bodies-cache/c.rs", path, path.read_text())
        assert isinstance(case, Case), case
        ws = root / "ws"
        members, out = write_workspace(ws, 0, [case])
        assert not out and list(members) == ["u0_0"], (members, out)
        assert (ws / "Cargo.toml").read_text() == WORKSPACE
        config = tomllib.loads((ws / ".cargo" / "config.toml").read_text())
        assert config["source"]["crates-io"]["replace-with"] == SOURCE and config["source"][SOURCE]["directory"] == "vendor"
        member = tomllib.loads((ws / "crates" / "u0_0" / "Cargo.toml").read_text())
        assert member["package"]["edition"] == "2021" and member["bin"][0]["name"] == "u0_0", member
        assert member["dependencies"] == {"dep": {"package": "u0_0_dep", "version": f"={VERSION}"}}, member
        dep = tomllib.loads((ws / "vendor" / "u0_0_dep" / "Cargo.toml").read_text())
        assert dep["package"]["name"] == "u0_0_dep" and dep["package"]["edition"] == "2021"
        assert dep["lib"]["name"] == "dep" and "proc-macro" not in dep["lib"]
        assert dep["dependencies"] == {"pm": {"package": "u0_0_pm", "version": f"={VERSION}"}}
        pm = tomllib.loads((ws / "vendor" / "u0_0_pm" / "Cargo.toml").read_text())
        assert pm["lib"]["proc-macro"] is True and "dependencies" not in pm
        assert (ws / "vendor" / "u0_0_dep" / "src" / "m.rs").read_text() == "pub fn g() {}\n", "a module of a dependency"
        sums = json.loads((ws / "vendor" / "u0_0_dep" / CHECKSUMS).read_text())
        assert sums["package"] is None and set(sums["files"]) == {"Cargo.toml", "src/lib.rs", "src/m.rs"}, sums
        for file, sha in sums["files"].items():
            assert hashlib.sha256((ws / "vendor" / "u0_0_dep" / file).read_bytes()).hexdigest() == sha, file
        source = root / "src"
        (source / "sub").mkdir(parents=True)
        (source / "t.rs").write_text("mod sub;\nfn main() {}\n")
        (source / "sub" / "mod.rs").write_text("pub mod deep;\n")
        (source / "sub" / "deep.rs").write_text("")
        (source / "lost.rs").write_text("mod gone;\nfn main() {}\n")
        found = [replace(program(), name="a/t.rs", path=str(source / "t.rs")),
                 replace(program(), name="a/lost.rs", path=str(source / "lost.rs"))]
        members, out = write_workspace(root / "ws2", 3, found)
        assert list(members) == ["u3_0"] and list(out) == ["a/lost.rs"], (members, out)
        assert (root / "ws2" / "crates" / "u3_0" / "src" / "sub" / "deep.rs").is_file()
        assert not (root / "ws2" / ".cargo").exists()
    many = [replace(program(kind="lib" if i % 2 else "bin"), name=f"a/{i}.rs") for i in range(5)]
    assert sorted(len(b) for b in batches_of(many, 2)) == [1, 2, 2]
    assert all(len({c.kind for c in b}) == 1 for b in batches_of(many, 10))


def served(package: str, outcome: str, bodies: str, why: str | None = None) -> str:
    return json.dumps({"reason": SERVED, "package": package, "outcome": outcome, "bodies": bodies, "why": why})


def test_serving() -> None:
    pm = Dep("pm", "/a/pm.rs", "2021", True)
    dep = Dep("dep", "/a/dep.rs", "2021", False, (pm,))
    case = replace(program(), name="bodies-cache/c.rs", deps=(dep,), twice=True)
    both = "\n".join([served("u0_0_dep", "served", "served"), served("u0_0_pm", "served", "served"),
                      '{"reason": "blaze-work"}', "not json"])
    assert serving(case, "u0_0", both) is None
    missing = serving(case, "u0_0", served("u0_0_dep", "served", "served"))
    assert missing is not None and missing[:2] == ("error", "error") and "pm" in missing[2], missing
    checked = serving(case, "u0_0", both.replace('"bodies": "served", "why": null}', '"bodies": "checked", "why": null}', 1))
    assert checked is not None and checked[1] == "not served" and "dep" in checked[2], checked
    odd = serving(case, "u0_0", served("u0_0_dep", "yes", "served") + "\n" + served("u0_0_pm", "served", "served"))
    assert odd is not None and odd[:2] == ("error", "error"), odd
    refuses = replace(case, refused=frozenset({"pm"}))
    said = serving(refuses, "u0_0", both)
    assert said is not None and said[1] == "served what is refused", said
    edge = "\n".join([served("u0_0_dep", "served", "served"), served("u0_0_pm", "served", "refused", "block-local item")])
    assert serving(refuses, "u0_0", edge) is None
    neither = "\n".join([served("u0_0_dep", "served", "served"), served("u0_0_pm", "checked", "checked")])
    assert (serving(refuses, "u0_0", neither) or ("", "", ""))[1] == "not served"


SELFTESTS = (test_expect, test_keys, test_case_of, test_tree, test_messages, test_judged, test_run_program,
             test_deps, test_workspace, test_serving)


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
