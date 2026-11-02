#!/usr/bin/env python3
# pyright: basic
"""keys.py: whether the store hands a build code that was compiled from something else.

The workspace in harness/keys has a function for each thing code can depend on that is not the
text of its file. Each case builds the workspace in an empty home, stops the daemon and leaves
the home nothing but its store (`storelib.forget`), changes one such thing, and builds again
with a daemon that has only the store the first build filled: both programs have to do what a
fresh build of the same source does (rebuild.py), and a case that names a line has to change
what the fresh build prints. "again" changes the checkout where it is, "elsewhere" changes a
copy of it at another path, as a second worktree would have it.

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
from dataclasses import dataclass
from pathlib import Path

from typed_input import fail, parse

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
from rebuild import WRONG, Project, copy, scratch
from storelib import forget, store_of, stored_bytes


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
    ("a cfg of RUSTFLAGS", setenv("RUSTFLAGS", "--cfg keys_flag"), "flag_cfg"),
    # The crate's identity is in the TypeIds of its types (astral-experiments/blaze#56).
    ("the identity of a crate", setenv("RUSTFLAGS", "-C metadata=keys"), "identity"),
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


PROGRAM = "kapp"


def build(project, args):
    """The daemon's build held to a fresh one: (verdict, what was said, what the fresh build's
    program does)."""
    target = ("-p", PROGRAM, *args)
    verdict, said = project.check(PROGRAM, *target)
    project.stop()
    return verdict, said, project.fresh(PROGRAM, *target)


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
    work = scratch("blaze-keys-")
    runs = [
        (name, change, line, mode)
        for name, change, line in CASES
        if not it.cases or any(case in name for case in it.cases)
        for mode in ("again", "elsewhere")
    ]
    runs = runs[it.shard.index - 1 :: it.shard.count]
    wrong = total = 0
    for name, change, line, mode in runs:
        total += 1
        at = os.path.join(work, str(total))
        home = os.path.join(at, "home")
        first = copy(os.path.join(HERE, "keys"), os.path.join(at, "first"))
        project, args = Project(first, os.path.join(at, "w1"), home=home), []
        verdict, said, before = build(project, args)
        problems = [] if verdict not in WRONG else [f"the first build: {verdict} {said[-600:]}"]
        forget(home)
        filled = stored_bytes(store_of(home))
        if mode == "elsewhere":
            second = copy(first, os.path.join(at, "second"))
            project = Project(second, os.path.join(at, "w2"), home=home, env=project.env)
        change(project.ws, project.env, args)
        verdict, said, after = build(project, args)
        if verdict in WRONG:
            problems.append(f"the second build: {verdict} {said[-1500:]}")
        if line and before == after:
            problems.append("the program built fresh is the same after the change")
        wrong += bool(problems)
        print(
            f"{'same     ' if not problems else 'DIFFERENT'} {mode:9} {name}: store {filled} bytes, {stored_bytes(store_of(home)) - filled} more",
            flush=True,
        )
        for problem in problems:
            print(f"    {problem}")
        if not problems:
            shutil.rmtree(at, ignore_errors=True)
    print(f"{total - wrong} of {total} right; {work}")
    sys.exit(1 if wrong else 0)


if __name__ == "__main__":
    main()
