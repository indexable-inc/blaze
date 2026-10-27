#!/usr/bin/env python3
"""testselect.py: `cargo ra-jit test` with `RA_JIT_TESTS=affected` runs the tests an edit
reached since they last passed, and no others.

usage: testselect.py    (RA_JIT_BIN names the ra-jit built; cargo-ra-jit is found next to it)

A package has a library with unit tests and an integration test of it, and a library of the
workspace beside it that both link, which says under `cfg(test)` what it does not otherwise
(an edit to it is the daemon's, built as rustc built it for the tests): tests that call one
function or another of the library, one in a private module named as a public function beside
it, one that downcasts a `dyn Any` of what one returns (a
`TypeId`, which code generation takes from rustc), one that keeps it in a `HashMap` (seeded at
random), one that prints a backtrace (which reads the environment and the working directory),
one that reads a file, one that calls nothing. It is tested until the daemon serves the build,
then edited a step at a time, and after each the tests that ran have to be the ones the step
names: those whose code or what it calls changed, those added or given other attributes, the
one that reads a file every time, the one that failed until it passes (and not the others that
ran and passed beside it), one added to a program with an `ignore` with a list, one added to a library with a bench (in
a package of its own, built with RUSTC_BOOTSTRAP=1), and with
`RA_JIT_TESTS=all` every test. A test that ran has to end as its source says. With `-v` the
output says why the one that reads a file runs, by the calls from it to the file system, and
how many tests run every time.

Every test runs sandboxed (Linux and macOS, unless `RA_JIT_SANDBOX=off`): the one that reads a
file runs again only when the file changed, `-v` says which file, a test that writes into the
package changes nothing of it outside its run, a test that writes outside fails and the command
says what the sandbox refused it and how to take it out, a test its package takes out of the
sandbox (`unsandboxed-tests`) runs outside and writes there, every time, and with
`RA_JIT_SANDBOX=off` the one that reads a file runs every time again.

Python 3, stdlib only.
"""
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
import time

HERE = os.path.dirname(os.path.abspath(__file__))
os.environ.setdefault("RUSTUP_TOOLCHAIN", open(os.path.join(HERE, "toolchain")).read().strip())
TIMEOUT = 900
LIB = """pub fn helper(x: u32) -> u32 {
    x + 1
}

pub fn other(x: u32) -> u32 {
    x * 2
}

// A private module and a public function of one name, the test reached through the module, as
// helix-core's `increment::date_time`.
pub mod shapes {
    mod twice {
        pub fn twice(x: u32) -> u32 {
            2 * x
        }

        #[cfg(test)]
        mod tests {
            #[test]
            fn doubles() {
                assert_eq!(super::twice(2), 4);
            }
        }
    }

    pub fn twice(x: u32) -> u32 {
        twice::twice(x)
    }
}

pub fn based(x: u32) -> u32 {
    tbase::base(x)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uses_helper() {
        assert_eq!(helper(1), 2);
    }

    #[test]
    fn uses_other() {
        assert_eq!(other(2), 4);
    }

    #[test]
    fn uses_base() {
        assert_eq!(based(1), 3);
    }

    #[test]
    fn uses_any() {
        let value: Box<dyn std::any::Any> = Box::new(helper(1));
        assert_eq!(value.downcast_ref::<u32>(), Some(&2));
    }

    #[test]
    fn uses_a_map() {
        let mut map = std::collections::HashMap::new();
        map.insert(helper(1), "two");
        assert_eq!(map.get(&2), Some(&"two"));
    }

    #[test]
    fn shows_a_backtrace() {
        let shown = std::backtrace::Backtrace::capture().to_string();
        assert_eq!(helper(shown.len().min(0) as u32), 1);
    }

    #[test]
    fn reads_a_file() {
        assert!(std::fs::read_to_string("data.txt").unwrap().contains("tsel"));
    }

    #[test]
    fn plain() {
        assert_eq!(1 + 1, 2);
    }
}
"""
BASE = """pub fn base(x: u32) -> u32 {
    x + 2 + offset()
}

#[cfg(not(test))]
fn offset() -> u32 {
    0
}

#[cfg(test)]
fn offset() -> u32 {
    100
}
"""
FILES = {
    "Cargo.toml": '[package]\nname = "tsel"\nversion = "0.1.0"\nedition = "2021"\n\n'
                  '[dependencies]\ntbase = { path = "tbase" }\n\n[workspace]\nmembers = ["tbase"]\n',
    "src/lib.rs": LIB,
    "data.txt": "tsel\n",
    # What the test that writes into the package overwrites: not `data.txt`, which the test
    # that reads a file reads in the same run of the tests, on another thread.
    "notes.txt": "notes\n",
    # A library of the workspace the unit tests are linked with as rustc built it, without
    # `cfg(test)`, as ripgrep's crates are each other's.
    "tbase/Cargo.toml": '[package]\nname = "tbase"\nversion = "0.1.0"\nedition = "2021"\n',
    "tbase/src/lib.rs": BASE,
    # An `ignore` with a list after it, which rustc reads as one without a message.
    "tests/it.rs": """#![allow(ill_formed_attribute_input)]

#[test]
#[ignore(not_yet)]
fn it_listed() {}

#[test]
fn it_helper() {
    assert_eq!(tsel::helper(0), 1);
}

#[test]
fn it_other() {
    assert_eq!(tsel::other(0), 0);
}

#[test]
fn it_base() {
    assert_eq!(tsel::based(0), 2);
}
""",
}
CALL_HELPER = {"tests::uses_helper", "tests::uses_any", "tests::uses_a_map", "tests::shows_a_backtrace", "it_helper"}
CALL_BASE = {"tests::uses_base", "it_base"}
ALL = CALL_HELPER | CALL_BASE | {"tests::uses_other", "tests::reads_a_file", "tests::plain", "it_helper", "it_other",
                                 "shapes::twice::tests::doubles", "it_listed"}
FILE = "tests::reads_a_file"
WRITES = "tests::writes_a_file"
SANDBOXED = sys.platform in ("linux", "darwin") and os.environ.get("RA_JIT_SANDBOX") not in ("off", "0")
OUTSIDE_TEST = "tests::writes_outside"
# A directory outside the package and the temporary ones, which the sandbox keeps read-only.
OUTSIDE = os.path.join(os.path.expanduser("~"), ".cache", f"ra-jit-testselect-{os.getpid()}")
# The test that reads a file, in a step that did not change the file: where it runs sandboxed,
# it is skipped like any other unchanged test.
F = set() if SANDBOXED else {FILE}
# (what the step is, [(file, old, new)], RA_JIT_TESTS, the tests that run (`None`: cargo's build
# of an edited manifest, which runs them all), whether they pass)
STEPS = [
    ("nothing edited", [], "affected", F, True),
    ("a function one test calls", [("src/lib.rs", "x + 1", "x + 2 - 1")], "affected",
     CALL_HELPER | F, True),
    ("a function another test calls", [("src/lib.rs", "x * 2", "x + x")], "affected",
     {"tests::uses_other", "it_other"} | F, True),
    ("a library the unit tests link", [("tbase/src/lib.rs", "x + 2", "x + 3 - 1")], "affected",
     CALL_BASE | F, True),
    ("that library again", [("tbase/src/lib.rs", "x + 3 - 1", "x + 4 - 2")], "affected",
     CALL_BASE | F, True),
    ("a test added", [("src/lib.rs", "    #[test]\n    fn plain()",
                       "    #[test]\n    fn added() {\n        assert_eq!(helper(2), 3);\n    }\n\n"
                       "    #[test]\n    fn plain()")], "affected",
     {"tests::added"} | F, True),
    ("a test added beside an ignore with a list",
     [("tests/it.rs", "#[test]\nfn it_base()", "#[test]\nfn it_added() {\n    assert_eq!(1 + 1, 2);\n}\n\n#[test]\nfn it_base()")],
     "affected", {"it_added"} | F, True),
    ("a test ignored", [("src/lib.rs", "    #[test]\n    fn plain()", "    #[test]\n    #[ignore]\n    fn plain()")],
     "affected", {"tests::plain"} | F, True),
    ("a test that fails", [("src/lib.rs", "other(2), 4", "other(2), 5")], "affected",
     {"tests::uses_other"} | F, False),
    ("nothing edited after it failed", [], "affected", {"tests::uses_other"} | F, False),
    ("the test fixed", [("src/lib.rs", "other(2), 5", "other(2), 4")], "affected",
     {"tests::uses_other"} | F, True),
    # The unit tests fail, so cargo runs no integration test after them.
    ("one of several reached failing", [("src/lib.rs", "x + 2 - 1", "x + 3 - 2"),
                                         ("src/lib.rs", "assert_eq!(helper(1), 2);", "assert_eq!(helper(1), 3);")],
     "affected", CALL_HELPER - {"it_helper"} | {"tests::added"} | F, False),
    ("nothing edited after one failed", [], "affected", {"tests::uses_helper"} | F, False),
    ("that one fixed", [("src/lib.rs", "assert_eq!(helper(1), 3);", "assert_eq!(helper(1), 2);")],
     "affected", {"tests::uses_helper", "it_helper"} | F, True),
    ("every test asked for", [], "all", ALL | {"tests::added", "it_added"}, True),
    ("nothing edited again", [], "affected", F, True),
    ("why a test runs every time", [], "affected -v", F, True),
]
if SANDBOXED:
    STEPS += [
        # Outside the sandbox a test that reads a file runs every time, as it does where there
        # is none.
        ("outside the sandbox", [], "affected -v RA_JIT_SANDBOX=off", {FILE}, True),
        ("the file a test reads edited", [("data.txt", "tsel", "tsel tsel")], "affected -v", {FILE}, True),
        ("nothing edited after the file", [], "affected", set(), True),
        ("a test that writes into the package", [
            ("src/lib.rs", "    #[test]\n    fn added()",
             "    #[test]\n    fn writes_a_file() {\n"
             "        std::fs::write(\"notes.txt\", \"written\").unwrap();\n"
             "        std::fs::write(\"written.txt\", \"written\").unwrap();\n"
             "        assert_eq!(std::fs::read_to_string(\"notes.txt\").unwrap(), \"written\");\n"
             "    }\n\n    #[test]\n    fn added()")], "affected", {WRITES}, True),
        ("nothing edited after it wrote", [], "affected", set(), True),
        ("a test that writes outside", [
            ("src/lib.rs", "    #[test]\n    fn added()",
             "    #[test]\n    fn writes_outside() {\n"
             f"        std::fs::write({json.dumps(os.path.join(OUTSIDE, 'written.txt'))}, \"written\").unwrap();\n"
             "    }\n\n    #[test]\n    fn added()")], "affected", {OUTSIDE_TEST}, False),
        ("that test taken out of the sandbox", [
            ("Cargo.toml", "[workspace]",
             f'[package.metadata.blaze.sandbox]\nunsandboxed-tests = ["{OUTSIDE_TEST}"]\n\n[workspace]')],
         "affected", None, True),
        ("nothing edited after it ran outside", [], "affected -v", {OUTSIDE_TEST}, True),
    ]
# What a step's output has to say, beside the tests it runs.
SAYS = {} if SANDBOXED else {
    "why a test runs every time": [
        r"Always tests::reads_a_file: it reaches `\w+`, which reads or writes files, by "
        r"tsel::tests::reads_a_file -> std::fs::read_to_string::<…> -> .* -> \w+$",
        r"Skipped \d+ tests, unchanged since the last pass; 1 test run every time \(said above\);",
    ],
}
if SANDBOXED:
    SAYS = {
        "outside the sandbox": [r"Always tests::reads_a_file: it reaches `\w+`, which reads or writes files"],
        "the file a test reads edited": [
            r"Sandboxed tests::reads_a_file: it read `[^`]*/data\.txt`, which changed$",
        ],
        "a test that writes outside": [
            rf"note: {OUTSIDE_TEST} failed in the sandbox, which refused it writing `[^`]*/written\.txt` "
            r"\(the machine is read-only in a sandbox\); if it needs that, run it outside: "
            rf'`unsandboxed-tests = \["{OUTSIDE_TEST}"\]` under `\[package\.metadata\.blaze\.sandbox\]`',
        ],
        "nothing edited after it ran outside": [
            rf"Unsandboxed {OUTSIDE_TEST}: taken out of the sandbox in \S*Cargo\.toml$",
        ],
    }
# What has to be so of the package after a step, beside what it says.
AFTER = {
    "a test that writes into the package": lambda ws: (
        open(os.path.join(ws, "notes.txt")).read() == "notes\n"
        and not os.path.exists(os.path.join(ws, "written.txt"))) or "IT WROTE INTO THE PACKAGE",
    "a test that writes outside": lambda ws: (
        not os.path.exists(os.path.join(OUTSIDE, "written.txt"))) or "IT WROTE OUTSIDE THE SANDBOX",
    "that test taken out of the sandbox": lambda ws: (
        os.path.exists(os.path.join(OUTSIDE, "written.txt"))) or "IT DID NOT WRITE OUTSIDE",
}
# Steps that fail now and then for a reason not yet found, each with its issue. In the fast tier
# (ci.py's RA_JIT_CI_TIER) a wrong one is printed as such and not counted, so it does not hold up
# the merge queue; everywhere else, the release train's full tier included, it counts.
FLAKY: dict[str, str] = {}
RAN = re.compile(r"^test (\S+) \.\.\. (ok|ignored|FAILED)", re.M)
# A library with a `#[bench]`, which is nightly's: built with RUSTC_BOOTSTRAP=1.
BENCHED = {
    "Cargo.toml": '[package]\nname = "benched"\nversion = "0.1.0"\nedition = "2021"\n',
    "src/lib.rs": """#![feature(test)]
extern crate test;

pub fn add(x: u32) -> u32 {
    x + 1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[bench]
    fn bench_add(b: &mut test::Bencher) {
        b.iter(|| add(1))
    }

    #[test]
    fn adds() {
        assert_eq!(add(1), 2);
    }
}
""",
}
BENCHED_ADDED = ("src/lib.rs", "    #[test]\n    fn adds()",
                 "    #[test]\n    fn added() {\n        assert_eq!(add(2), 3);\n    }\n\n    #[test]\n    fn adds()")


def benched(root, env):
    """A test added to a program with a bench: what the step's line says."""
    ws, env = os.path.join(root, "benched"), dict(env, RUSTC_BOOTSTRAP="1", CARGO_UNSTABLE_CHECKSUM_FRESHNESS="false")
    for path, text in BENCHED.items():
        os.makedirs(os.path.dirname(os.path.join(ws, path)), exist_ok=True)
        open(os.path.join(ws, path), "w").write(text)
    deadline = time.monotonic() + 300
    while "[ra-jit]" not in test(ws, env, "all")[1]:
        if time.monotonic() > deadline:
            return "NEVER SERVED BY THE DAEMON"
        time.sleep(0.5)
    path, old, new = BENCHED_ADDED
    text = open(os.path.join(ws, path)).read()
    open(os.path.join(ws, path), "w").write(text.replace(old, new, 1))
    code, said = test(ws, env, "all")
    ran = {name for name, _ in RAN.findall(said)}
    if "[ra-jit]" not in said:
        return "NOT SERVED BY THE DAEMON"
    if ran != {"tests::added", "tests::adds", "tests::bench_add"} or code != 0:
        return f"ENDED {code} HAVING RUN {', '.join(sorted(ran)) or 'nothing'}"
    return "ok"


def test(ws, env, mode):
    """`mode` is RA_JIT_TESTS's value, then `cargo ra-jit test`'s arguments and NAME=VALUE for the
    environment."""
    mode, *args = mode.split()
    more = dict(it.split("=", 1) for it in args if "=" in it)
    args = [it for it in args if "=" not in it]
    ran = subprocess.run(["cargo", "ra-jit", "test", *args], cwd=ws, env=dict(env, RA_JIT_TESTS=mode, **more),
                         capture_output=True, text=True, timeout=TIMEOUT)
    return ran.returncode, ran.stdout + ran.stderr


def main():
    bin_dir = os.path.dirname(os.path.abspath(os.environ["RA_JIT_BIN"]))
    if not os.path.exists(os.path.join(bin_dir, "cargo-ra-jit")):
        raise SystemExit(f"no cargo-ra-jit beside {os.environ['RA_JIT_BIN']}")
    if SANDBOXED:
        probe = subprocess.run([os.environ["RA_JIT_BIN"], "sandbox", "--probe"], capture_output=True, text=True)
        if probe.returncode != 0:
            raise SystemExit(f"there is no sandbox for the tests here (`RA_JIT_SANDBOX=off` tests without):\n"
                             f"{probe.stderr.strip()}")
    root = tempfile.mkdtemp(prefix="ra-jit-testselect-")
    os.makedirs(OUTSIDE, exist_ok=True)
    # The daemon's sockets are under its home, and a socket's path is short (104 bytes on macOS).
    ws, home = os.path.join(root, "tsel"), tempfile.mkdtemp(prefix="rj-ts-", dir="/tmp")
    for path, text in FILES.items():
        os.makedirs(os.path.dirname(os.path.join(ws, path)), exist_ok=True)
        open(os.path.join(ws, path), "w").write(text)
    env = dict(os.environ, PATH=f"{bin_dir}{os.pathsep}{os.environ['PATH']}", RA_JIT_HOME=home,
               RUST_BACKTRACE="0")
    for name in ("CARGO_TARGET_DIR", "CARGO_INCREMENTAL", "RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS", "RA_JIT_TESTS"):
        env.pop(name, None)
    wrong = 0

    def served(when):
        deadline = time.monotonic() + 300
        while time.monotonic() < deadline:
            code, said = test(ws, env, "affected")
            if code != 0:
                raise SystemExit(f"the tests failed {when}:\n{said[-3000:]}")
            if "[ra-jit]" in said:
                break
            time.sleep(0.5)
        else:
            raise SystemExit(f"no build said `[ra-jit]` {when}; the daemon's log is in {home}")
        # The daemon's first build of the tests: each is new to it, and runs.
        test(ws, env, "affected")

    try:
        served("before any edit")
        for what, edits, mode, expected, passes in STEPS:
            for path, old, new in edits:
                text = open(os.path.join(ws, path)).read()
                assert old in text, (what, old)
                open(os.path.join(ws, path), "w").write(text.replace(old, new, 1))
            code, said = test(ws, env, mode)
            ran = {name for name, _ in RAN.findall(said)}
            failed = {name for name, end in RAN.findall(said) if end == "FAILED"}
            verdict = "ok"
            # An edit of the manifest is cargo's to build, and the daemon's again after it.
            manifest = any(path == "Cargo.toml" for path, _, _ in edits)
            if "[ra-jit]" not in said and not manifest:
                verdict = "NOT SERVED BY THE DAEMON"
            elif expected is not None and ran != expected:
                extra, missing = sorted(ran - expected), sorted(expected - ran)
                verdict = f"RAN {', '.join(extra) or 'nothing more'}; DID NOT RUN {', '.join(missing) or '-'}"
            elif (code == 0) != passes or (failed == set()) != passes:
                verdict = f"ENDED {code} WITH {', '.join(sorted(failed)) or 'no test'} FAILED"
            elif unsaid := [it for it in SAYS.get(what, []) if not re.search(it, said, re.M)]:
                verdict = f"DID NOT SAY {' NOR '.join(unsaid)}"
            elif what in AFTER and (after := AFTER[what](ws)) is not True:
                verdict = after
            if verdict != "ok" and what in FLAKY and os.environ.get("RA_JIT_CI_TIER") == "fast":
                verdict += f" (FLAKY, not counted in the fast tier: {FLAKY[what]})"
            else:
                wrong += verdict != "ok"
            print(f"{what:34} {verdict}", flush=True)
            if verdict != "ok" and (os.environ.get("TESTSELECT_VERBOSE") or os.environ.get("RA_JIT_CI_TIER")):
                print(said[-4000:])
            if manifest:
                served(f"after {what}")
        verdict = benched(root, env)
        wrong += verdict != "ok"
        print(f"{'a test added beside a bench':34} {verdict}", flush=True)
    finally:
        subprocess.run(["cargo", "ra-jit", "stop"], cwd=ws, env=env, capture_output=True, timeout=120)
        shutil.rmtree(OUTSIDE, ignore_errors=True)
    print(f"{len(STEPS) + 1 - wrong} of {len(STEPS) + 1} right; under {root}, the daemon's home {home}")
    if wrong:
        sys.exit(1)
    shutil.rmtree(root, ignore_errors=True)
    shutil.rmtree(home, ignore_errors=True)


if __name__ == "__main__":
    main()
