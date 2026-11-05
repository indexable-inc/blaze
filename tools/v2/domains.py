"""Checks that every hash domain written in blaze's Rust is written once: two hashes that share a
domain string are one hash to whoever compares their outputs (AGENTS.md one-concept-one-home).

Input: `{"root": "<repository>"}` (inline, a path, or `-`). Prints one line per problem and exits
1 when there is any; CI runs it beside `tools/docs.py check`.

A domain is a `str` literal under `crates/` that starts with `blaze` and ends in its version
number (`"blaze v2 body code 1"`, `"blaze plane history 1"`), the form every domain in the tree
has. Byte strings (`b"..."`) are left out: they are file magics, and every hashing entry point
(`blaze_ids::Hasher::new`, `Domain::new`, `ContentId::of`) takes a `&str`. Comment lines are
left out too: a doc that names a domain does not write it.

The rule: each domain is written at most once outside test code. Test code (a file under a
`tests` or `benches` directory, a `tests.rs` or `test.rs`, an item under `#[cfg(test)]` or
`#[cfg(all(test, ...))]`) may repeat a domain, its own or one it recomputes, as often as it
likes: its hashes are compared with nothing kept. A literal of the domain form that is not a
domain (a text file's header) is named in ALLOWED with the one place it is written and why; an
entry that no longer matches is itself a problem, so the list only shrinks.
"""

import re
import sys
from collections import defaultdict
from dataclasses import dataclass
from pathlib import Path

from typed_input import parse

SCRIPT = "domains.py"
# A `str` literal (not `b"..."`, not `r"..."`) of the domain form: no escapes or format braces
# but a trailing `\0`, ending in a version number.
DOMAIN = re.compile(r'(?<![\w"])"(blaze[ -][^"\\{}]*?[ -]\d+(?:\\0)?)"')
CFG_TEST = re.compile(r"\s*#\[cfg\((?:test|all\(test\b[^\]]*)\)\]\s*$")
TEST_DIRS = {"tests", "benches"}
TEST_FILES = {"tests.rs", "test.rs"}

# Literals of the domain form that are not hash domains, each with the one non-test file that
# writes it and why it may equal a domain written elsewhere.
ALLOWED: dict[str, tuple[str, str]] = {
    "blaze v2 field order 1": (
        "crates/blaze-items/src/field_order.rs",
        (
            "the first line of a field order plan's text file (`HEADER`), never hashed as a domain; "
            "the plan's ID has its own (`PLAN_DOMAIN`)"
        ),
    ),
}


@dataclass(frozen=True)
class Input:
    root: str


@dataclass(frozen=True)
class Site:
    path: str  # relative to the root
    line: int
    test: bool


def test_path(rel: Path) -> bool:
    return any(part in TEST_DIRS for part in rel.parts[:-1]) or rel.name in TEST_FILES


def sites(root: Path) -> dict[str, list[Site]]:
    found: dict[str, list[Site]] = defaultdict(list)
    for path in sorted((root / "crates").rglob("*.rs")):
        rel = path.relative_to(root)
        if "target" in rel.parts:
            continue
        in_test_file = test_path(rel)
        lines = path.read_text(encoding="utf-8", errors="replace").split("\n")
        # From a `#[cfg(test)]` module on, the rest of the file is its tests (they come last);
        # a `#[cfg(test)]` on another item covers the item's first line.
        tests_from: int | None = None
        test_line = -1
        for at, line in enumerate(lines):
            if CFG_TEST.match(line):
                following = next((it for it in lines[at + 1 :] if not it.strip().startswith("#[")), "")
                if re.match(r"\s*(?:pub(?:\([^)]*\))?\s+)?mod\s+\w+\s*\{", following):
                    tests_from = at if tests_from is None else tests_from
                else:
                    test_line = at + 1
                continue
            if line.lstrip().startswith("//"):
                continue
            test = in_test_file or (tests_from is not None and at >= tests_from) or at == test_line
            for m in DOMAIN.finditer(line):
                found[m.group(1)].append(Site(rel.as_posix(), at + 1, test))
    return found


def problems(root: Path) -> list[str]:
    out: list[str] = []
    found = sites(root)
    for domain, where in sorted(found.items()):
        written = [it for it in where if not it.test]
        allowed = ALLOWED.get(domain)
        if allowed is not None:
            path, _why = allowed
            others = [it for it in written if it.path != path]
            if len(others) == len(written) or not others:
                places = ", ".join(f"{it.path}:{it.line}" for it in written) or "none"
                out.append(f"ALLOWED[{domain!r}] names {path}, which no longer shares the literal with a "
                           f"domain (non-test sites: {places}): remove the entry")
                continue
            written = others
        if len(written) > 1:
            places = ", ".join(f"{it.path}:{it.line}" for it in written)
            out.append(f"the hash domain {domain!r} is written {len(written)} times outside tests ({places}): "
                       "give one of them its own domain, or name the one constant both use")
    for domain, (path, _why) in sorted(ALLOWED.items()):
        if domain not in found:
            out.append(f"ALLOWED[{domain!r}] ({path}): no such literal is left; remove the entry")
    return out


def main() -> None:
    args = parse(Input, sys.argv[1] if len(sys.argv) > 1 else "", SCRIPT)
    root = Path(args.root).resolve()
    if not (root / "crates").is_dir():
        sys.exit(f"{SCRIPT}: {root} has no crates/ directory")
    found = problems(root)
    for problem in found:
        print(f"{SCRIPT}: {problem}")
    if found:
        sys.exit(1)
    count = len(sites(root))
    print(f"{SCRIPT}: {count} hash domains, each written once outside tests")


if __name__ == "__main__":
    main()
