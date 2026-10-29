#!/usr/bin/env python3
# pyright: basic
"""keys.py: whether the store hands a build code that was compiled from something else.

The workspace in harness/keys has a function for each thing code can depend on that is not the
text of its file. Each case builds the workspace with an empty store, changes one such thing,
and builds again with the store the first build filled: the program has to print what cargo's
does. "again" changes the checkout where it is, "elsewhere" changes a copy of it at another
path, as a second worktree or another machine would have it.

usage: keys.py '{}'
       keys.py '{"cases": ["a feature", "release"], "shard": {"index": 2, "count": 3}}'

The input is one JSON object (inline, the path of a file, or `-` for standard input), an Input
below; a field it does not have is refused. `cases` picks the cases whose names contain any of
its strings (all of them when there are none). `shard` runs every `count`th build of the chosen
ones, starting at the `index`th: `count` machines with `index` from 1 to `count` run all of them
between them. Run it in the repository's Python workspace: `uv run --frozen python
harness/keys.py ...` from the root.
"""

import os
import shutil
import sys
import tempfile
from dataclasses import dataclass
from pathlib import Path

from typed_input import fail, parse

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
from difftest import Fixture


def write(path, text):
    return lambda ws, env, args: open(os.path.join(ws, path), "w").write(text)


def replace(path, old, new):
    def edit(ws, env, args):
        text = Path(ws, path).read_text()
        assert old in text, (path, old)
        Path(ws, path).write_text(text.replace(old, new))

    return edit


def append(path, text):
    return lambda ws, env, args: open(os.path.join(ws, path), "a").write(text)


def both(*changes):
    return lambda ws, env, args: [change(ws, env, args) for change in changes]


def setenv(name, value):
    return lambda ws, env, args: env.__setitem__(name, value)


def arg(*more):
    return lambda ws, env, args: args.extend(more)


# What blaze says of a function it leaves to rustc because the copy of an instance it calls
# is one rustc picked and blaze cannot tell which.
REFUSED = "overflow checks"

# What blaze does not do yet: these differ, and are told apart when they do.
OPEN = set()

# (name, the change, the line of the program's output it has to show in or None for no line)
CASES = [
    ("nothing", lambda ws, env, args: None, None),
    ("a file outside the package that is included", write("shared.txt", "shared anew"), "outside"),
    ("a file of the package that is included", write("klib/src/inside.txt", "inside anew"), "inside"),
    ("what a build script reads and generates code from", write("klib/gen.txt", "8"), "generated"),
    ("a variable a build script reads and sets for the crate", setenv("KEYS_SCRIPT", "set"), "script_env"),
    ("a cfg a build script sets", write("klib/flag", ""), "script_cfg"),
    ("a feature", arg("--features", "extra"), "feature"),
    ("a variable of the process read by option_env!", setenv("KEYS_PROCESS", "set"), "process_env"),
    (
        "a variable of .cargo/config.toml",
        replace(".cargo/config.toml", '"config"', '"config anew"'),
        "config_env",
    ),
    # With the flag `difftest` sets, which this would otherwise take the place of: without it
    # rustc keeps to one object the functions nothing else calls, and those are left to rustc.
    ("a cfg of RUSTFLAGS", setenv("RUSTFLAGS", "-C link-dead-code --cfg keys_flag"), "flag_cfg"),
    # The crate's identity is in the TypeIds of its types (astral-experiments/blaze#56).
    ("the identity of a crate", setenv("RUSTFLAGS", "-C link-dead-code -C metadata=keys"), "identity"),
    ("the release profile", arg("--release"), "assertions"),
    (
        "overflow checks of the dev profile",
        append("Cargo.toml", "\n[profile.dev]\noverflow-checks = false\n"),
        "overflows",
    ),
    (
        "debug assertions of the dev profile",
        append("Cargo.toml", "\n[profile.dev]\ndebug-assertions = false\n"),
        "assertions",
    ),
    (
        "a profile of its own",
        both(
            append("Cargo.toml", '\n[profile.quick]\ninherits = "dev"\noverflow-checks = false\n'),
            arg("--profile", "quick"),
        ),
        "overflows",
    ),
    (
        "overflow checks of the package a function is written in",
        append("Cargo.toml", "\n[profile.dev.package.kdep]\noverflow-checks = false\n"),
        "checks",
    ),
    (
        "overflow checks of a package that calls generic code",
        append("Cargo.toml", "\n[profile.dev.package.klib]\noverflow-checks = false\n"),
        "checks",
    ),
    (
        "overflow checks of the package of the program",
        append("Cargo.toml", "\n[profile.dev.package.kapp]\noverflow-checks = false\n"),
        "checks",
    ),
    (
        "overflow checks of one package in release",
        both(
            append("Cargo.toml", "\n[profile.release.package.kdep]\noverflow-checks = true\n"),
            arg("--release"),
        ),
        "checks",
    ),
    (
        "overflow checks of the program alone in release",
        both(
            append("Cargo.toml", "\n[profile.release.package.kapp]\noverflow-checks = true\n"),
            arg("--release"),
        ),
        "checks",
    ),
    (
        "overflow checks of all but the package a small function is written in, in release",
        both(
            append(
                "Cargo.toml",
                "\n[profile.release]\noverflow-checks = true\n[profile.release.package.kdep]\noverflow-checks = false\n",
            ),
            arg("--release"),
        ),
        "checks",
    ),
    (
        "overflow checks of all but the package whose copy of generic code the program uses",
        append(
            "Cargo.toml",
            "\n[profile.dev]\noverflow-checks = false\n[profile.dev.package.klib]\noverflow-checks = true\n",
        ),
        "checks",
    ),
    (
        "debug assertions of one package",
        append("Cargo.toml", "\n[profile.dev.package.klib]\ndebug-assertions = false\n"),
        "assertions",
    ),
    ("a constant of a dependency", replace("kdep/src/lib.rs", "SCALE: u32 = 3", "SCALE: u32 = 4"), "scaled"),
    ("an inlined function of a dependency", replace("kdep/src/lib.rs", "x + 1", "x + 2"), "scaled"),
    (
        "the layout of a type of a dependency",
        replace("kdep/src/lib.rs", "pub side: u32,", "pub side: u32,\n    pub other: u64,"),
        "shape",
    ),
    (
        "a generic function of a dependency",
        replace("kdep/src/lib.rs", '"{:?} {}"', '"{:?} of {}"'),
        "described",
    ),
]


SCRIPT = "keys.py"


@dataclass(frozen=True)
class Shard:
    index: int
    count: int


@dataclass(frozen=True)
class Input:
    cases: tuple[str, ...] = ()
    shard: Shard = Shard(1, 1)


def size(directory):
    return sum(os.path.getsize(os.path.join(directory, name)) for name in os.listdir(directory))


def build(ws, env, args):
    """What cargo's program prints and what blaze's does, and the functions that were left to rustc."""
    saved = dict(os.environ)
    os.environ.update(env)
    try:
        fx = Fixture(ws, ["kapp", "klib", "kdep"], ["-p", "kapp", *args])
        fx.blaze_args = list(args)
        fx.rustc_build()
        fx.blaze_build()
        skipped = [
            row
            for rows in fx.reports.values()
            for row in rows
            if row[0] == "skipped" and "no rustc symbol" not in row[2]
        ]
        return fx.behaviour(fx.reference), fx.behaviour(fx.binary), skipped
    finally:
        os.environ.clear()
        os.environ.update(saved)
        shutil.rmtree(fx.work, ignore_errors=True)


def main():
    match sys.argv[1:]:
        case [text]:
            it = parse(Input, text, SCRIPT)
        case _:
            fail(SCRIPT, f"usage: {SCRIPT} JSON (see the top of the file)")
    if not 1 <= it.shard.index <= it.shard.count:
        fail(SCRIPT, f"shard.index is from 1 to shard.count ({it.shard.count}), not {it.shard.index}")
    if unknown := [case for case in it.cases if not any(case in name for name, _, _ in CASES)]:
        fail(SCRIPT, f"cases: {unknown} is in the name of no case")
    work = os.path.realpath(tempfile.mkdtemp(prefix="blaze-keys-"))
    runs = [
        (name, change, line, mode)
        for name, change, line in CASES
        if not it.cases or any(case in name for case in it.cases)
        for mode in ("again", "elsewhere")
    ]
    runs = runs[it.shard.index - 1 :: it.shard.count]
    wrong = total = fixed = refusals = 0
    for name, change, line, mode in runs:
        total += 1
        at = os.path.join(work, str(total))
        store, first = os.path.join(at, "store"), os.path.join(at, "first")
        os.makedirs(store)
        shutil.copytree(os.path.join(HERE, "keys"), first, ignore=shutil.ignore_patterns("target"))
        env, args = {"BLAZE_CACHE": store}, []
        want, got, _ = build(first, env, args)
        problems = [] if want == got else ["the first build differs"]
        filled = size(store)
        second = first
        if mode == "elsewhere":
            second = os.path.join(at, "second")
            shutil.copytree(first, second, ignore=shutil.ignore_patterns("target"))
        before = want
        change(second, env, args)
        want, got, left = build(second, env, args)
        if want != got:
            problems.append("the second build differs")
        if line and before == want:
            problems.append("cargo's program is the same after the change")
        # Where the crates differ in overflow checks, blaze leaves to rustc a function that
        # calls a copy it cannot tell the checks of. The function the case is about has to
        # be blaze's all the same, or the case shows nothing.
        refused = [row for row in left if REFUSED in row[2] and row[1].split("::")[-1] != line]
        if len(left) > len(refused):
            problems.append(f"{len(left) - len(refused)} functions left to rustc")
        refusals += len(refused)
        known = name in OPEN
        fixed += known and not problems
        wrong += bool(problems) and not known
        print(
            f"{'same     ' if not problems else 'open     ' if known else 'DIFFERENT'} {mode:9} {name}: store {filled} bytes, {size(store) - filled} more",
            flush=True,
        )
        for problem in problems:
            print(f"    {problem}")
        for row in refused:
            print(f"    refused {row[1]}: {row[2][:160]}")
        if want != got:
            ours, theirs = (it[1].decode(errors="replace").splitlines() for it in (got, want))
            for a, b in zip(theirs, ours):
                if a != b:
                    print(f"    rustc: {a}\n    blaze: {b}")
            if len(ours) != len(theirs):
                print(
                    f"    blaze: status {got[0]}, {len(ours)} lines of {len(theirs)}; {got[2].decode(errors='replace').strip()[-300:]}"
                )
    still = sum(1 for name, _, _, _ in runs if name in OPEN) - fixed
    print(
        f"{total - wrong - still} of {total - still} right, {still} open, {refusals} functions refused; {work}"
    )
    if fixed:
        print(f"{fixed} of the open cases are the same now: take them out of OPEN")
    sys.exit(1 if wrong or fixed else 0)


if __name__ == "__main__":
    main()
