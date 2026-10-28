#!/usr/bin/env python3
"""staticcallbacks.py: a lazy static's initializer calls the current version of its helper.

The initializer of a `LazyLock` in `app` calls an `#[inline(always)]` helper of the
dependency `values`. Each step rewrites `values`, `blaze build`s and runs the program, which
has to print what cargo's prints: first an edit of a sibling function alone, then of the
helper the initializer inlines. Reduced by Charlie Marsh (astral-experiments/blaze#83, #87,
Apache-2.0/MIT).
"""
import os, sys, tempfile

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
from blaze_cli import Side, base_env, how_built, program_output, difference

MANIFEST = ('[package]\nname = "app"\nversion = "0.1.0"\nedition = "2021"\n\n'
            '[workspace]\nmembers = ["values"]\n\n[dependencies]\nvalues = { path = "values" }\n\n'
            '[profile.dev]\nopt-level = 1\n')
MAIN = """use std::sync::LazyLock;

#[inline(never)]
fn registry() -> &'static usize {
    static REGISTRY: LazyLock<usize> = LazyLock::new(|| values::left());
    &REGISTRY
}

fn main() {
    println!("{} {} {}", *registry(), values::left(), values::tag());
}
"""
# (the helper's value, the sibling's)
STEPS = [(10, 30), (10, 20), (11, 20)]


def values(value, tag):
    return (f"#[inline(always)]\npub fn left() -> usize {{\n    {value}\n}}\n\n"
            f"#[inline(never)]\npub fn tag() -> usize {{\n    {tag}\n}}\n")


def main():
    work = tempfile.mkdtemp(prefix="blaze-callbacks-")
    ws = os.path.realpath(os.path.join(work, "ws"))
    for file, text in (("Cargo.toml", MANIFEST), ("src/main.rs", MAIN), ("values/Cargo.toml",
                       '[package]\nname = "values"\nversion = "0.1.0"\nedition = "2021"\n'),
                       ("values/src/lib.rs", "")):
        os.makedirs(os.path.dirname(os.path.join(ws, file)), exist_ok=True)
        open(os.path.join(ws, file), "w").write(text)
    env = base_env(os.path.join(work, "home"))
    ours, theirs = Side(ws, env, True), Side(ws, env, False)
    wrong = 0
    try:
        for value, tag in STEPS:
            open(os.path.join(ws, "values/src/lib.rs"), "w").write(values(value, tag))
            (_, mine), (_, other) = ours.cargo("run"), theirs.cargo("run")
            differs = difference(program_output(mine), program_output(other))
            how = how_built(mine.stderr)
            bad = mine.returncode != 0 or how != "daemon" or bool(differs)
            wrong += bad
            print(f"{'WRONG' if bad else 'right'}  left {value}, tag {tag}: {how}; "
                  f"{differs or mine.stdout.strip()!r} {mine.stderr.strip()[-300:] if bad else ''}", flush=True)
    finally:
        ours.cargo("stop")
    print(f"{len(STEPS) - wrong} of {len(STEPS)} right; work {work}")
    sys.exit(1 if wrong else 0)


if __name__ == "__main__":
    main()
