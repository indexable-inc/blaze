#!/usr/bin/env python3
"""gate_changes.py: which tier of blaze.yaml a change runs (its `changes` job).

    gate_changes.py '{"tier": "gate", "repository": "indexable-inc/blaze", "base": SHA, "head": SHA}'

`tier` is the one the event asks for (`gate` for a pull request, `warm` for a push to main or a
run by hand); it is what runs when the change between `base` and `head` touches a file the
tier's jobs read (CODE below, and WARM for `warm`), and `none` when it touches nothing else.
Anything that cannot be told counts as code: no `base` (a run by hand, a new branch), a
comparison GitHub refuses or cuts short (300 files), or no file at all. Prints `tier=...` and
writes it to $GITHUB_OUTPUT.

An allow-list, not a deny-list: a new directory of documents, benches or harness scripts runs
no build until a tier reads it. The harness scripts' own checks (`harness/imports.py`,
`harness/rustcsites.py`) are blaze.yaml's `scripts` job, which runs on every change.

The JSON is inline, a file's path, or `-` for stdin. Needs gh (GH_TOKEN). Python 3.11+, stdlib.
"""
import asyncio
import json
import os
import sys
import typing
from dataclasses import dataclass, fields

# What the build of blaze and the unit tests (blaze-platform.yaml's `build` and `tests` jobs,
# tiers gate and warm) read. A path ending in `/` is the directory and everything under it.
CODE = (
    # The workspace's crates, with their test data, crates/hir-ty-codegen/llvm.json (the LLVM
    # the flake and the toolchain action fetch) and blaze-cargo's toolchain pins.txt (the commit
    # of rustc's tests the rust-tests action fetches).
    "crates/",
    # The patched crates of [patch.crates-io], hashed into BLAZE_SOURCES (blaze-engine/build.rs).
    "vendor/",
    # The workspace, the lock file (the build is --locked) and cargo's config ([env]).
    "Cargo.toml",
    "Cargo.lock",
    ".cargo/",
    # Found by blaze-fmt's and blaze-lints' search up from a directory; not known to be unread
    # by their tests.
    ".rustfmt.toml",
    ".clippy.toml",
    # The pinned nightly: compiled into blaze-cargo, the flake's dev shell and the toolchain action.
    "harness/toolchain",
    # The tests job runs it.
    "harness/unittests.py",
    # `nix develop`: the dev shell both jobs build and test in, and `.#llvm`.
    "flake.nix",
    "flake.lock",
    # The release of blaze that builds and tests blaze (.github/actions/blaze), and its installer.
    ".github/blaze-version",
    "install.sh",
    # The gate's jobs, and the actions they use.
    ".github/workflows/blaze.yaml",
    ".github/workflows/blaze-platform.yaml",
    ".github/actions/toolchain/",
    ".github/actions/blaze/",
    ".github/actions/blaze-remote/",
    ".github/actions/rust-tests/",
)
# What the warm tier (a push to main) reads besides: the std cases of the smoke corpus, run
# against the blaze that build built.
WARM = (
    "harness/smoke.py",
    "harness/smoke/",
)
# GitHub's comparison lists at most this many files; a longer one is cut short.
MOST_FILES = 300


@dataclass(frozen=True)
class Input:
    tier: typing.Literal["gate", "warm"]
    repository: str
    base: str = ""
    head: str = ""


def typed(text: str) -> Input:
    """An Input from the JSON `text` (inline, a path, or `-`); exits naming the field that is wrong."""
    if text == "-":
        text = sys.stdin.read()
    elif not text.lstrip().startswith("{"):
        with open(text) as f:
            text = f.read()
    try:
        value = json.loads(text)
    except json.JSONDecodeError as e:
        sys.exit(f"gate_changes.py: the input is not JSON: {e}")
    if not isinstance(value, dict):
        sys.exit("gate_changes.py: the input is a JSON object")
    known = {f.name for f in fields(Input)}
    if unknown := sorted(set(value) - known):
        sys.exit(f"gate_changes.py: no field {', '.join(unknown)} (fields: {', '.join(sorted(known))})")
    if missing := sorted({"tier", "repository"} - set(value)):
        sys.exit(f"gate_changes.py: field {', '.join(missing)} is missing")
    if value["tier"] not in typing.get_args(typing.get_type_hints(Input)["tier"]):
        sys.exit(f"gate_changes.py: field tier is gate or warm, not {value['tier']!r}")
    for name in ("repository", "base", "head"):
        if name in value and not isinstance(value[name], str):
            sys.exit(f"gate_changes.py: field {name} is a string, not {value[name]!r}")
    return Input(**value)


def is_code(path: str, paths: tuple[str, ...]) -> bool:
    return any(path.startswith(it) if it.endswith("/") else path == it for it in paths)


async def changed(given: Input) -> list[str] | None:
    """The files of the comparison, or None when they cannot be told (said on stderr)."""
    if not given.base or not given.head or set(given.base) == {"0"}:
        print("no base or head to compare: counted as code", file=sys.stderr)
        return None
    argv = ["gh", "api", f"repos/{given.repository}/compare/{given.base}...{given.head}", "--jq", "[.files[].filename]"]
    proc = await asyncio.create_subprocess_exec(*argv, stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE)
    try:
        async with asyncio.timeout(120):
            out, err = await proc.communicate()
    except TimeoutError:
        proc.kill()
        print(f"`{' '.join(argv)}` took over 120 s: counted as code", file=sys.stderr)
        return None
    if proc.returncode:
        tail = err.decode(errors="replace").strip()[-1500:]
        print(f"`{' '.join(argv)}` exited {proc.returncode}: {tail}: counted as code", file=sys.stderr)
        return None
    files = json.loads(out)
    if not files or len(files) >= MOST_FILES:
        print(f"{len(files)} files in the comparison: counted as code", file=sys.stderr)
        return None
    return files


async def decide(given: Input) -> str:
    paths = CODE + (WARM if given.tier == "warm" else ())
    files = await changed(given)
    if files is None:
        return given.tier
    code = [it for it in files if is_code(it, paths)]
    for it in files:
        print(f"{'code' if it in code else '    '}  {it}", file=sys.stderr)
    return given.tier if code else "none"


def main() -> None:
    if len(sys.argv) != 2:
        sys.exit(__doc__)
    tier = asyncio.run(decide(typed(sys.argv[1])))
    print(f"tier={tier}")
    if out := os.environ.get("GITHUB_OUTPUT"):
        with open(out, "a") as f:
            f.write(f"tier={tier}\n")


if __name__ == "__main__":
    main()
