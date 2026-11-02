#!/usr/bin/env python3
"""The doctests of a workspace of three libraries, run by `blaze test --doc`: blaze loads the
workspace once for all of them, and they run one library after another, each example reading
where it runs and the variables as it does under rustdoc.

    BLAZE_BIN=../target/release/blaze-engine python3 doctestonce.py

The steps: every example passes; an example of the first two libraries fails, and the run stops
after the first; the same with `--no-fail-fast`; one of the first fails, with `--no-fail-fast`.
Each step is held to what its examples say (`STEPS`): the exit status, the libraries in turn
(`Doc-tests` lines), how many examples of each passed and which items' examples failed, and the
packages the run names as failed. Every example has to be blaze's (on macOS std's writable
statics leave them to rustc, #255), and blaze has to have loaded the workspace once.
Anything else is WRONG and fails the run."""
import os
import re
import subprocess
import sys
import tempfile

from doctests import doctest_report, outcomes

HERE = os.path.dirname(os.path.abspath(__file__))
BLAZE = os.path.abspath(os.environ.get("BLAZE_BIN", os.path.join(HERE, "..", "target", "release", "blaze-engine")))

# An exit status, not a panic: where a panic in an example is said is not this step's to check.
FAILS = '/// if std::env::var_os("DOCTEST_FAIL").is_some() {\n///     std::process::exit(3);\n/// }\n'
FAILS_FIRST = FAILS.replace('var_os("DOCTEST_FAIL")', 'var_os("DOCTEST_FAIL").or(std::env::var_os("DOCTEST_FAIL_FIRST"))')
# Each library's examples read the package they run in and its name, at run time and at
# compile time; gamma's use beta, a dev-dependency of gamma alone.
WORKSPACE = {
    "Cargo.toml": '[workspace]\nmembers = ["alpha", "beta", "gamma"]\nresolver = "2"\n',
    "alpha/Cargo.toml": '[package]\nname = "alpha"\nversion = "0.1.0"\nedition = "2021"\n',
    "alpha/src/lib.rs": "//! ```\n//! assert_eq!(alpha::double(2), 4);\n//! ```\n\n"
    "/// Twice `n`.\n///\n/// ```\n" + FAILS_FIRST + "/// assert_eq!(alpha::double(3), 6);\n/// ```\n"
    "pub fn double(n: u32) -> u32 {\n    n * 2\n}\n\n"
    "/// ```\n/// assert!(std::env::current_dir().unwrap().ends_with(\"alpha\"));\n"
    "/// assert_eq!(std::env::var(\"CARGO_PKG_NAME\").unwrap(), \"alpha\");\n/// ```\npub fn here() {}\n",
    "beta/Cargo.toml": '[package]\nname = "beta"\nversion = "0.1.0"\nedition = "2021"\n\n'
    '[dependencies]\nalpha = { path = "../alpha" }\n',
    "beta/src/lib.rs": "/// ```\n" + FAILS + "/// assert_eq!(beta::quadruple(1), 4);\n"
    "/// assert!(std::env::current_dir().unwrap().ends_with(\"beta\"));\n"
    "/// assert_eq!(std::env::var(\"CARGO_PKG_NAME\").unwrap(), \"beta\");\n/// ```\n"
    "pub fn quadruple(n: u32) -> u32 {\n    alpha::double(alpha::double(n))\n}\n\n"
    "/// ```\n/// assert_eq!(env!(\"CARGO_PKG_NAME\"), \"beta\");\n/// ```\npub fn name() {}\n",
    "gamma/Cargo.toml": '[package]\nname = "gamma"\nversion = "0.1.0"\nedition = "2021"\n\n'
    '[dev-dependencies]\nbeta = { path = "../beta" }\n',
    "gamma/src/lib.rs": "/// ```\n/// assert_eq!(beta::quadruple(gamma::one()), 4);\n/// ```\n"
    "pub fn one() -> u32 {\n    1\n}\n",
}

ALL_PASS = [("alpha", 3, []), ("beta", 2, []), ("gamma", 1, [])]
# Each step: its name, the arguments, the variables, the exit status, each library run in turn
# with how many of its examples pass and the items whose examples fail, and the packages named
# as failed. Without `--no-fail-fast` the run stops after the first library that fails.
STEPS = [
    ("every example passes", [], {}, 0, ALL_PASS, []),
    ("examples fail", [], {"DOCTEST_FAIL": "1"}, 101, [("alpha", 2, ["double"])], ["alpha"]),
    ("examples fail, --no-fail-fast", ["--no-fail-fast"], {"DOCTEST_FAIL": "1"}, 101,
     [("alpha", 2, ["double"]), ("beta", 1, ["quadruple"]), ("gamma", 1, [])], ["alpha", "beta"]),
    ("one fails, --no-fail-fast", ["--no-fail-fast"], {"DOCTEST_FAIL_FIRST": "1"}, 101,
     [("alpha", 2, ["double"]), ("beta", 2, []), ("gamma", 1, [])], ["alpha"]),
]


def item_of(test):
    """The item a doctest's libtest name documents (`src/lib.rs - double (line 5)`), "" for the crate's."""
    found = re.search(r" - (?:(\S+) )?\(line \d+\)$", test)
    return (found[1] or "") if found else test


def tested(ws, args, extra, target_dir):
    """How the doctests ended: the exit status, each library in turn as (name, examples passed,
    items whose examples failed), the packages named as failed, and all that was said beside."""
    path = os.path.dirname(BLAZE) + os.pathsep + os.environ["PATH"]
    env = {**os.environ, **extra, "PATH": path, "CARGO_TARGET_DIR": target_dir}
    run = subprocess.run(["blaze", "test", "--doc", *args], cwd=ws, capture_output=True,
                         text=True, timeout=3600, env=env)
    # Colored under `CARGO_TERM_COLOR=always`, as CI has it; the text is read.
    said = re.sub(r"\x1b\[[0-9;]*m", "", run.stderr)
    doc_tests = re.findall(r"(?m)^\s*Doc-tests (\S+)$", said)
    reports = [outcomes(doctest_report("running " + it)) for it in re.split(r"(?m)^running ", run.stdout)[1:]]
    libraries = [(name, sum(how == "ok" for how in ended.values()),
                  sorted(item_of(test) for test, how in ended.items() if how == "FAILED"))
                 for name, ended in zip(doc_tests, reports)]
    if len(doc_tests) != len(reports):
        libraries.append(("?", len(reports), [f"{len(doc_tests)} `Doc-tests` lines"]))
    failed = sorted(set(re.findall(r"`-p (\S+) --doc`", said)))
    return (run.returncode, libraries, failed), said


def main():
    wrong = 0
    with tempfile.TemporaryDirectory(prefix="blaze-doctestonce-") as scratch:
        ws = os.path.join(scratch, "ws")
        for file, text in WORKSPACE.items():
            os.makedirs(os.path.dirname(os.path.join(ws, file)), exist_ok=True)
            open(os.path.join(ws, file), "w").write(text)
        for name, args, extra, *want in STEPS:
            got, said = tested(ws, args, extra, os.path.join(scratch, "blaze"))
            loads = len(re.findall(r"(?m)^load workspace: ", said))
            left = re.findall(r"blaze: (?:the doctests are left to rustdoc|.* is rustc's): .*", said)
            built = sum(int(n) for n in re.findall(r"doctests: (\d+) built by blaze", said))
            examples = sum(passed + len(failed) for _, passed, failed in got[1])
            parts = ["exit status", "libraries (name, passed, failed items)", "packages named as failed"]
            wrong_parts = [f"{part}: {g!r}, and the examples say {w!r}"[:400]
                           for part, w, g in zip(parts, want, got) if w != g]
            if loads != 1:
                wrong_parts.append(f"the workspace was loaded {loads} times")
            if left:
                wrong_parts.append(f"not blaze's: {left[0][:80]} ... {left[0][-160:]}")
            if built < examples:
                wrong_parts.append(f"{built} examples built by blaze of the {examples} that ran")
            verdict = "WRONG" if wrong_parts else "ok"
            wrong += bool(wrong_parts)
            print(f"  {name:32} exit {got[0]:3}  {len(got[1])} libraries, {built} examples built by blaze, "
                  f"{loads} load{'s' if loads != 1 else ''}  {verdict}", flush=True)
            for it in wrong_parts:
                print(f"    {it}", flush=True)
            if wrong_parts and got[0] not in (0, want[0]):
                print(f"    blaze said: {said.strip()[-800:]!r}", flush=True)
    print(f"{len(STEPS) - wrong}/{len(STEPS)} ok")
    sys.exit(1 if wrong else 0)


if __name__ == "__main__":
    main()
