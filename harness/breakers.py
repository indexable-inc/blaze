#!/usr/bin/env python3
"""breakers.py: every reduction in harness/breakers on its own, blaze's binary against cargo's.

usage: breakers.py [case ...]

The crates in CRATES are run through cratetest.py: what broke was in their tests. Those in
NIGHTLY_CRATES too, with RUSTC_BOOTSTRAP=1 for the nightly feature each uses.
Prints "N of N right" and exits non-zero when a case differs.
"""
import os, subprocess, sys

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
from difftest import Fixture, RUNNER

CRATES = ["devdep", "blanket", "edition2015", "fragment", "procmacro", "overlap", "hostcfg", "wasionly"]
# Crates that use a nightly feature, built by the pinned compiler with RUSTC_BOOTSTRAP=1. That
# also has cargo read the `[unstable]` table of its configuration, where `checksum-freshness`
# would make the touch that has cargo link the test again do nothing.
NIGHTLY_CRATES = ["allocated"]
NIGHTLY_ENV = {"RUSTC_BOOTSTRAP": "1", "CARGO_UNSTABLE_CHECKSUM_FRESHNESS": "false"}
# What is reduced and not fixed: these are expected to differ, and are told apart when they do.
OPEN = {"assert_message", "future_unwind_flags", "fragment_edition_through_macro", "assert_long_expression"}
# Frames that are larger than rustc's, where a program counts on rustc's: the answer is right and the room is not.
# Cranelift's keep three arrays where rustc's keep two; LLVM's keep one.
if os.environ.get("BLAZE_BACKEND", "cranelift") == "cranelift":
    OPEN |= {"frame_of_array"}
# What is reduced and not fixed either, where blaze says so: a function of these is left to rustc.
LEFT = {"empty_array_by_impl", "macro_reads_its_spelling", "fragment_ends_statement"}
# Crates reduced and not fixed, where blaze says so: a function of these is left to rustc.
LEFT_CRATES = []
# Cases about the code around a function blaze has to leave to rustc: that one is meant to be.
AROUND_LEFT = {"await_left_in_generic", "provided_method_static::rustcs", "neon_fma_lanes"}


def main():
    crates = ["breakers", "breakers2021", "breakers2015"]
    fx = Fixture(os.path.join(HERE, "breakers"), crates, ["-p", "breaker"])
    fx.rustc_build()
    fx.blaze_build()
    skipped = [r for rows in fx.reports.values() for r in rows if r[0] == "skipped" and "no rustc symbol for function" not in r[2]]
    left = {name for name in LEFT if any(f"::{name}::" in row[1] for row in skipped)}
    skipped = [row for row in skipped if not any(f"::{name}::" in row[1] for name in LEFT | AROUND_LEFT)]
    for row in skipped:
        print(f"  skipped {row[1]}: {row[2][:300]}")
    names = sys.argv[1:] or subprocess.check_output([*RUNNER, fx.reference], text=True).split()
    wrong, fixed = 0, []
    for name in names:
        fx.run_args = [name]
        want, got = fx.behaviour(fx.reference), fx.behaviour(fx.binary)
        same = want == got
        wrong += not same and name not in OPEN
        if same and name in OPEN:
            fixed.append(name)
        state = "left     " if name in left else "same     " if same else "open     " if name in OPEN else "DIFFERENT"
        print(f"{state} {name}", flush=True)
        if not same:
            ours, theirs = (it[1].decode(errors="replace").strip() for it in (got, want))
            # Where the two part, not where they start.
            at = max(0, next((i for i, (a, b) in enumerate(zip(ours, theirs)) if a != b), 0) - 60)
            print(f"    rustc: status {want[0]} {theirs[at:at + 300]}")
            tail = got[2].decode(errors="replace").strip().splitlines()[-1:]
            print(f"    blaze: status {got[0]} {ours[at:at + 300]} {' '.join(tail)[:200]}")
    if not sys.argv[1:]:
        # What breaks in a crate's tests and not in a program: each a crate for cratetest.py.
        for crate in CRATES + NIGHTLY_CRATES:
            env = dict(os.environ, **NIGHTLY_ENV) if crate in NIGHTLY_CRATES else None
            run = subprocess.run([sys.executable, os.path.join(HERE, "cratetest.py"), os.path.join(HERE, "breakers", crate)],
                                 capture_output=True, text=True, env=env)
            same = run.returncode == 0 and "skipped" not in run.stdout
            wrong += not same
            names.append(crate)
            print(f"{'same     ' if same else 'DIFFERENT'} {crate} (its tests)", flush=True)
            if not same:
                print("    " + "\n    ".join(run.stdout.strip().splitlines()[-8:]))
        for crate in LEFT_CRATES:
            run = subprocess.run([sys.executable, os.path.join(HERE, "cratetest.py"), os.path.join(HERE, "breakers", crate)],
                                 capture_output=True, text=True)
            names.append(crate)
            if run.returncode == 0 and "skipped" in run.stdout:
                left.add(crate)
            elif run.returncode == 0:
                fixed.append(crate)
            else:
                wrong += 1
                print("    " + "\n    ".join(run.stdout.strip().splitlines()[-8:]))
            print(f"{'left     ' if crate in left else 'same     ' if run.returncode == 0 else 'DIFFERENT'} {crate} (its tests)", flush=True)
    compiled = (LEFT & set(names)) - left
    still = len(OPEN & set(names)) - len(fixed) + len(left & set(names))
    print(f"{len(names) - wrong - still} of {len(names) - still} right, {still} open, {len(skipped)} functions skipped")
    if fixed:
        print(f"the same now, take them out of OPEN or LEFT_CRATES: {' '.join(fixed)}")
    if compiled:
        print(f"compiled now, take them out of LEFT: {' '.join(sorted(compiled))}")
    sys.exit(1 if wrong or skipped or fixed or compiled else 0)


if __name__ == "__main__":
    main()
