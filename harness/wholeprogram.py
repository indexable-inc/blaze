#!/usr/bin/env python3
"""wholeprogram.py: programs blaze builds whole, with a `main` of its own, and runs.

usage: wholeprogram.py [CASE...] [--release] [--keep]

Each case is a program of the workspace `harness/wholeprogram`, copied to a directory of
its own, and built by `blaze build -p CASE` in an empty home. The program runs with no
arguments, and what it prints on either stream and the status it exits with have to be what
Rust says of its source (`EXPECTED`: a pattern where a count is the standard library's to
choose). blaze then builds it once more, in another empty home and into the same target
directory, and the executable has to be the same bytes: a whole program's layout follows the
order its functions were asked for, not the order the threads finished them (#200).

  default_alloc  the default allocator; output on both streams and a status of its own
  custom_alloc   a `#[global_allocator]` of the program that counts what it is asked for
  lib_alloc      the `#[global_allocator]` is in a library of the workspace
  cfg_alloc      a `#[global_allocator]` under a `#[cfg]` that is false, as ripgrep's
  trait_block    a method a trait provides calls a function declared in its body
  c_main         `#![no_main]` and a `main` of C's, as bun's, with a body of 16 384 statements

`--release` builds both with the release profile, which v2 compiles with LLVM; the dev
profile it compiles with Cranelift.
"""
import argparse, os, re, shutil, sys

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
from rebuild import Project, copy, scratch

CASES = ["default_alloc", "custom_alloc", "lib_alloc", "cfg_alloc", "trait_block", "c_main"]


def mixed(seed, rounds):
    """trait_block's `Mix::mixed`, in u32's wrapping arithmetic."""
    at = seed
    for _ in range(rounds):
        at = ((((at << 5) | (at >> 27)) & 0xFFFF_FFFF) ^ 0x9E37_79B9) + 17 & 0xFFFF_FFFF
    return at


def mix(seed):
    """c_main's `mix`: 4^7 rounds, in u64's arithmetic."""
    acc = seed
    for _ in range(4 ** 7):
        acc = (((acc << 5) | (acc >> 59)) & 0xFFFF_FFFF_FFFF_FFFF) ^ (acc >> 3)
    return acc


# Each case's (exit status, stdout, stderr), the streams as patterns matched whole: run with no
# arguments, `std::env::args().count()` and C's `argc` are 1.
EXPECTED = {
    "default_alloc": (3, re.escape('{"and": 1, "brown": 1, "dog": 1, "end": 1, "fox": 1, "jumps": 1, "lazy": 1, '
                                   '"over": 1, "quick": 1, "the": 3}\n'), re.escape("10 words\n")),
    "custom_alloc": (0, r"sum 332833500: [1-9]\d* allocations of [1-9]\d* bytes through the counting allocator\n", ""),
    "lib_alloc": (0, r"490 characters, [1-9]\d* allocations or reallocations\n", ""),
    "cfg_alloc": (0, re.escape("THE DEFAULT ALLOCATOR\n"), ""),
    "trait_block": (mixed(11, 7) % 7, re.escape(f"{mixed(3, 7)}\n"), ""),
    "c_main": (3, re.escape(f"A MAIN OF C'S {mix(1):x}\n"), re.escape("1 arguments\n")),
}


def built(ws, work, case, release, name):
    """`blaze build -p CASE` in the empty home `<work>/<name>`: (Built, the project)."""
    project = Project(ws, work, home=os.path.join(work, name), name=name)
    try:
        return project.build(case, "-p", case, *(["--release"] if release else [])), project
    finally:
        project.stop()


def check(ws, work, case, release):
    shutil.rmtree(os.path.join(ws, "target"), ignore_errors=True)
    first, project = built(ws, work, case, release, f"{case}-1")
    if first.status != 0 or not first.executable:
        return f"  blaze's build failed ({first.status}):\n{first.said[-800:]}"
    status, stdout, stderr = EXPECTED[case]
    got = project.ran(first)
    lines = []
    if got[0] != status:
        lines.append(f"  status: {got[0]!r}, and Rust says {status}")
    for what, pattern, printed in (("stdout", stdout, got[1]), ("stderr", stderr, got[2])):
        if re.fullmatch(pattern, printed) is None:
            lines.append(f"  {what}: {printed!r}, not /{pattern}/")
    if lines:
        return "\n".join(lines)
    # The same source has to give the same executable every build, whichever thread compiled
    # which function first (#200).
    bytes_first = open(first.executable, "rb").read()
    shutil.rmtree(os.path.join(ws, "target"), ignore_errors=True)
    second, _ = built(ws, work, case, release, f"{case}-2")
    if second.status != 0 or not second.executable:
        return f"  blaze's second build failed ({second.status}):\n{second.said[-800:]}"
    if open(second.executable, "rb").read() != bytes_first:
        return f"  built differently the second time: {second.executable} ({len(bytes_first)} bytes the first)"
    return None


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("cases", nargs="*", default=CASES)
    parser.add_argument("--release", action="store_true")
    parser.add_argument("--keep", action="store_true", help="keep the copy of the workspace")
    args = parser.parse_args()
    work = scratch("blaze-wholeprogram-")
    ws = copy(os.path.join(HERE, "wholeprogram"), os.path.join(work, "ws"))
    failed = 0
    for case in args.cases:
        wrong = check(ws, work, case, args.release)
        print(f"{case:15} {'WRONG' if wrong else 'ok'}")
        if wrong:
            print(wrong)
            failed += 1
    if args.keep:
        print(f"kept {ws}")
    else:
        shutil.rmtree(work, ignore_errors=True)
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
