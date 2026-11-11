"""nostd.py: the check of ledger row S4 ("bstd is `#![no_std]` with its own sys and runtime; no
blaze crate links Rust's std"), and its progress meter (docs/v2/plan/design/sys.md, "Migration").

    uv run --frozen python harness/v2/nostd.py '{"command": "check"}'
    uv run --frozen python harness/v2/nostd.py '{"command": "progress"}'
    uv run --frozen python harness/v2/nostd.py '{"command": "list"}'
    uv run --frozen python harness/v2/nostd.py selftest

`harness/v2/nostd.json` is the ban list. Banned everywhere in `crates/` but the system layer
(`sys`: crates/blaze-sys) are `libc::` and Rust std's file, network, thread, process and clock
paths (`banned`). A module that still names one is listed in `not_yet_moved` with why; a listed
path is a file or a directory (a whole crate that has not started). The list only shrinks: it
is what is left of S4.

The input is one JSON object:

    command   check      S4 itself. Passes only when `not_yet_moved` is empty, no file outside
                         `sys` names a banned path, every crate of `crates/` is `#![no_std]`,
                         none names `extern crate std` outside its tests, and every package
                         the lock file gives a blaze crate is a blaze crate or one of
                         `no_std_packages`
              progress   the guard while the work is under way. Fails when a file that is not
                         listed names a banned path (a move went backwards, or new code
                         reached past blaze-sys), or a listed path names none (it moved: take
                         it off the list), or a crate in `no_std_crates` is no longer
                         `#![no_std]`. Prints what is left, by row
              list       prints every file that names a banned path, with its counts
    root      the checkout (".")
    bans      the ban list, under root ("harness/v2/nostd.json")

It reads source text, the lock file and the list; it builds nothing. A use is found by its
text (`libc::`, `std::fs`, a `std::{.. fs ..}` group), outside comments and outside tests (a
`tests.rs`, a `tests/`, `benches/` or `examples/` directory, and an inline `#[cfg(test)] mod`
to the end of its file):
tests are built on std's harness and are not what a blaze binary links. The lint that
refuses the same uses by name resolution is the `ambient` effect (std.md, S2), with blaze-sys
the one trusted crate; this script is the meter and the ledger's check.
"""

import re
import sys
import tomllib
from dataclasses import dataclass
from pathlib import Path
from typing import Literal

HERE = Path(__file__).resolve().parent
sys.path[:0] = [str(HERE), str(HERE.parent)]
from typed_input import parse

SCRIPT = "nostd.py"
# The std modules that reach the system: what blaze-sys and bstd's effects replace.
MODULES = ("fs", "net", "thread", "process")
CLOCKS = ("Instant", "SystemTime")
DIRECT = re.compile(r"\blibc::|\buse libc\b|\bstd::(?:fs|net|thread|process)\b|\bstd::time::(?:Instant|SystemTime)\b")
GROUP = re.compile(r"\bstd::\{([^;]*)\}", re.S)
TIME_GROUP = re.compile(r"\btime::\{([^{}]*)\}")
NO_STD = re.compile(r"^#!\[(?:no_std|cfg_attr\(not\(test\), no_std\))\]", re.M)
# An inline test module, which by convention is the last item of its file.
TESTS = re.compile(r"^#\[cfg\((?:all\()?test\b[^\]]*\]\s*\n(?:\s*#\[[^\n]*\]\s*\n)*\s*mod \w+ \{", re.M)
EXTERN_STD = re.compile(r"^\s*extern crate std\b", re.M)


@dataclass(frozen=True)
class Listed:
    """A file or directory that still names a banned path, and why it has not moved."""

    path: str
    why: str


@dataclass(frozen=True)
class Bans:
    """nostd.json."""

    about: str
    sys: tuple[str, ...]
    banned: tuple[str, ...]
    no_std_crates: tuple[str, ...]
    no_std_packages: tuple[str, ...]
    not_yet_moved: tuple[Listed, ...]


@dataclass(frozen=True)
class Input:
    command: Literal["check", "progress", "list"]
    root: str = "."
    bans: str = "harness/v2/nostd.json"


def code_of(text: str) -> str:
    """`text` without its comments and without its tests: what a blaze binary links."""
    cut = TESTS.search(text)
    text = text if cut is None else text[: cut.start()]
    return "\n".join("" if line.lstrip().startswith("//") else line.split(" // ")[0] for line in text.split("\n"))


def uses(text: str) -> int:
    """How many banned paths `text` names."""
    code = code_of(text)
    found = len(DIRECT.findall(code))
    for group in GROUP.finditer(code):
        inner = group.group(1)
        # A module of the group, at its top level or nested: `fs`, `fs::File`, `fs::{..}`.
        found += len(re.findall(r"(?:^|[{,\s])(?:%s)\b(?=\s*(?:::|,|\}|$))" % "|".join(MODULES), inner))
        for clocks in TIME_GROUP.finditer(inner):
            found += len(re.findall(r"\b(?:%s)\b" % "|".join(CLOCKS), clocks.group(1)))
        found += len(re.findall(r"\btime::(?:%s)\b" % "|".join(CLOCKS), inner))
    return found


def is_test(path: str) -> bool:
    parts = path.split("/")
    return parts[-1] == "tests.rs" or any(it in parts[:-1] for it in ("tests", "benches", "examples"))


def scan(root: Path) -> dict[str, int]:
    """Every source file under `crates/` that names a banned path, with how many."""
    found: dict[str, int] = {}
    for file in sorted((root / "crates").rglob("*.rs")):
        path = file.relative_to(root).as_posix()
        if is_test(path) or "/target/" in path:
            continue
        if n := uses(file.read_text(errors="replace")):
            found[path] = n
    return found


def under(path: str, prefix: str) -> bool:
    return path == prefix or path.startswith(prefix.rstrip("/") + "/")


def crate_roots(root: Path) -> dict[str, Path]:
    """Each crate of `crates/`, by package name, with its root source file."""
    roots: dict[str, Path] = {}
    for manifest in sorted((root / "crates").glob("*/Cargo.toml")):
        package = tomllib.loads(manifest.read_text()).get("package", {})
        for candidate in ("src/lib.rs", "src/main.rs"):
            if (source := manifest.parent / candidate).is_file():
                roots[package.get("name", manifest.parent.name)] = source
                break
    return roots


def lock_packages(root: Path) -> list[tuple[str, bool, list[str]]]:
    """The lock file's packages: name, whether it is a path package (blaze's), dependencies."""
    lock = tomllib.loads((root / "Cargo.lock").read_text())
    return [(p["name"], "source" not in p, [d.split(" ")[0] for d in p.get("dependencies", [])])
            for p in lock.get("package", [])]


def closure_outsiders(packages: list[tuple[str, bool, list[str]]], allowed: set[str]) -> list[str]:
    """The packages blaze's crates reach, through any chain, that are neither blaze's nor
    allowed as `#![no_std]`: each may link std."""
    by_name = {name: (ours, deps) for name, ours, deps in packages}
    seen: set[str] = set()
    stack = [name for name, ours, _ in packages if ours]
    while stack:
        name = stack.pop()
        if name in seen or name not in by_name:
            continue
        seen.add(name)
        stack += by_name[name][1]
    return sorted(name for name in seen if not by_name[name][0] and name not in allowed)


def problems(bans: Bans, found: dict[str, int], no_std: dict[str, bool], extern_std: list[str],
             outsiders: list[str], command: str) -> list[str]:
    """What is wrong: for `progress` only what went backwards or is stale; for `check` all of
    what is left."""
    said: list[str] = []
    listed = [it.path for it in bans.not_yet_moved]
    for path, n in found.items():
        if any(under(path, s) for s in bans.sys):
            continue
        if not any(under(path, l) for l in listed):
            said.append(f"{path}: names {n} banned path(s) and is not on the ban list: go through blaze-sys "
                        f"(or an effect), never around it")
    for l in listed:
        if not any(under(path, l) for path in found):
            said.append(f"{l}: listed as not yet moved and names no banned path: take it off the list")
    for name in bans.no_std_crates:
        if not no_std.get(name, False):
            said.append(f"{name}: listed as `#![no_std]` and its root does not say so")
    if command == "check":
        left = sum(n for path, n in found.items() if not any(under(path, s) for s in bans.sys))
        if listed:
            said.append(f"the ban list is not empty: {len(listed)} path(s), {left} banned use(s) left")
        said += [f"{name}: not `#![no_std]`" for name, ok in sorted(no_std.items()) if not ok]
        said += [f"{path}: `extern crate std` outside tests" for path in extern_std]
        said += [f"{name}: a package of the dependency closure that is not known to be `#![no_std]`"
                 for name in outsiders]
    return said


def main(it: Input) -> None:
    root = Path(it.root).resolve()
    bans = parse(Bans, (root / it.bans).read_text(), it.bans)
    found = scan(root)
    if it.command == "list":
        for path, n in found.items():
            print(f"{n:5d}  {path}")
        print(f"{sum(found.values()):5d}  in {len(found)} files")
        return
    roots = crate_roots(root)
    no_std = {name: bool(NO_STD.search(source.read_text())) for name, source in roots.items()}
    extern_std = [file.relative_to(root).as_posix() for file in sorted((root / "crates").rglob("*.rs"))
                  if not is_test(file.relative_to(root).as_posix())
                  and EXTERN_STD.search(code_of(file.read_text(errors="replace")))]
    outsiders = closure_outsiders(lock_packages(root), set(bans.no_std_packages))
    said = problems(bans, found, no_std, extern_std, outsiders, it.command)
    by_row = {l.path: sum(n for path, n in found.items() if under(path, l.path)) for l in bans.not_yet_moved}
    print(f"{SCRIPT}: {len(by_row)} path(s) on the ban list, {sum(by_row.values())} banned use(s) in them; "
          f"{sum(no_std.values())} of {len(no_std)} crates are #![no_std]")
    if it.command == "progress":
        for path, n in sorted(by_row.items(), key=lambda row: (-row[1], row[0])):
            print(f"{n:5d}  {path}")
    for line in said[:200]:
        print(f"{SCRIPT}: {line}")
    if len(said) > 200:
        print(f"{SCRIPT}: and {len(said) - 200} more")
    sys.exit(1 if said else 0)


def expect(holds: bool, what: str) -> None:
    if not holds:
        raise SystemExit(f"{SCRIPT} selftest: {what}")


def selftest() -> None:
    expect(uses("use std::fs;\nlet f = std::fs::read(p);\n") == 2, "two direct uses")
    expect(uses("use std::{\n    fs::{File, OpenOptions},\n    io,\n    thread::JoinHandle,\n    time::{Duration, Instant},\n};\n") == 3,
           "a group's fs, thread and Instant")
    expect(uses("use std::{fmt, sync::Arc, time::Duration};\n") == 0, "fmt, Arc and Duration are not banned")
    expect(uses("use std::{net::IpAddr, time::Instant};\n") == 2, "net and time::Instant in a group")
    expect(uses("// std::fs::read is gone\n/// see `std::thread`\nfn f() {} // libc::open\n") == 0, "comments do not count")
    expect(uses("let e = libc::EINTR;\nuse libc::c_int;\n") == 2, "libc paths")
    expect(uses("fn f() {}\n#[cfg(test)]\nmod tests {\n    use std::fs;\n}\n") == 0, "tests do not count")
    expect(uses("#[cfg(test)]\nstruct OffThread;\nfn f() { std::thread::yield_now() }\n") == 1,
           "a test-only item does not hide the code after it")
    expect(uses("use core::fmt;\nuse alloc::sync::Arc;\nlet s = mystd::fs::x;\n") == 0, "core, alloc and other names")
    expect(is_test("crates/a/src/tests.rs") and is_test("crates/a/tests/x.rs") and not is_test("crates/a/src/fs.rs"),
           "which files are tests")
    expect(under("crates/a/src/x.rs", "crates/a") and not under("crates/ab/src/x.rs", "crates/a"), "path prefixes")
    expect(bool(NO_STD.search("//! doc\n#![cfg_attr(not(test), no_std)]\n")) and bool(NO_STD.search("#![no_std]\n"))
           and not NO_STD.search("// #![no_std]\n#![feature(x)]\n"), "the no_std attribute")
    bans = Bans("", ("crates/sys",), (), ("b",), ("cfg-if",), (Listed("crates/core/src/uring.rs", "S4 slice 2"),))
    found = {"crates/sys/src/lib.rs": 9, "crates/core/src/uring.rs": 3}
    none = problems(bans, found, {"b": True, "core": False}, [], [], "progress")
    expect(none == [], f"a listed file and the system layer are no problem under way: {none}")
    back = problems(bans, found | {"crates/core/src/new.rs": 1}, {"b": True}, [], [], "progress")
    expect(len(back) == 1 and "crates/core/src/new.rs" in back[0] and "not on the ban list" in back[0],
           f"an unlisted use is refused, naming the file: {back}")
    stale = problems(bans, {"crates/sys/src/lib.rs": 9}, {"b": True}, [], [], "progress")
    expect(len(stale) == 1 and "take it off the list" in stale[0], f"a listed file with no use is stale: {stale}")
    lost = problems(bans, found, {"b": False}, [], [], "progress")
    expect(len(lost) == 1 and "b: listed as `#![no_std]`" in lost[0], f"a crate that lost no_std: {lost}")
    left = problems(bans, found, {"b": True, "core": False}, ["crates/b/src/rt.rs"], ["serde"], "check")
    for what in ("the ban list is not empty: 1 path(s), 3 banned use(s) left", "core: not `#![no_std]`",
                 "crates/b/src/rt.rs: `extern crate std`", "serde: a package of the dependency closure"):
        expect(any(what in line for line in left), f"`{what}` is reported by check, of {left}")
    done = problems(Bans("", ("crates/sys",), (), ("b",), ("cfg-if",), ()), {"crates/sys/src/lib.rs": 9},
                    {"b": True, "sys": True}, [], [], "check")
    expect(done == [], f"an empty list, no_std crates and a known closure pass: {done}")
    packages = [("a", True, ["b", "cfg-if"]), ("b", True, ["serde"]), ("serde", False, ["serde_core"]),
                ("serde_core", False, []), ("cfg-if", False, []), ("unreached", False, [])]
    expect(closure_outsiders(packages, {"cfg-if"}) == ["serde", "serde_core"],
           f"the closure's unknown packages: {closure_outsiders(packages, {'cfg-if'})}")
    print(f"{SCRIPT} selftest: ok")


if __name__ == "__main__":
    if len(sys.argv) != 2:
        sys.exit(f"usage: {SCRIPT} '<json>' | selftest\n\n{__doc__}")
    if sys.argv[1] == "selftest":
        selftest()
    else:
        main(parse(Input, sys.argv[1], SCRIPT))
