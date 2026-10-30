#!/usr/bin/env python3
"""pertest.py: `blaze test` runs each test in a process of its own, forked from the test
executable's fork server (docs/cargo.md, "Each test runs in a process of its own"; #1443).

usage: pertest.py INPUT    (INPUT: a JSON object, a file of one, or `-`: {"blaze": the blaze
                          engine built; `blaze` is found beside it})

A workspace of two libraries: `t`, whose tests pass, fail with output, panic as they should,
are ignored, abort the process, sleep when asked, and two that each count a static from zero
(both pass only when each has a process of its own); and `bare`, a `#![no_std]` library whose
tests name `std` only in their own module, as `crates/const-ast`'s do, with a dev-dependency
on `t` as const-ast has one (one with none can't build its tests yet, #1530). Steps:

- `blaze test --no-fail-fast` runs every test: each ends as libtest says it would on its own,
  the test that aborts fails alone, the failing test's output is printed with it, and `bare`'s
  tests run.
- `BLAZE_TEST_TIMEOUT=2` kills a test that sleeps past it, which fails as `TIMEOUT`.
- `BLAZE_TEST_PROCESSES=one` runs the tests as libtest's threads in one process, as `cargo
  test` does: one of the two counting tests then fails.

Python 3, stdlib only.
"""
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
from dataclasses import dataclass, fields

TIMEOUT = 900
FILES = {
    "Cargo.toml": '[workspace]\nmembers = ["t", "bare"]\nresolver = "2"\n',
    "t/Cargo.toml": '[package]\nname = "t"\nversion = "0.1.0"\nedition = "2021"\n',
    "t/src/lib.rs": r"""pub fn add(a: i32, b: i32) -> i32 {
    a + b
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    static N: AtomicUsize = AtomicUsize::new(0);

    #[test]
    fn adds() {
        assert_eq!(super::add(1, 2), 3);
    }

    #[test]
    fn fails() {
        println!("the output of a failing test");
        assert_eq!(super::add(1, 2), 4);
    }

    #[test]
    #[should_panic(expected = "as it should")]
    fn panics() {
        panic!("as it should");
    }

    #[test]
    #[ignore = "slow"]
    fn ignored() {}

    #[test]
    fn aborts() {
        if std::env::var_os("PERTEST_ONE").is_none() {
            std::process::abort();
        }
    }

    #[test]
    fn sleeps() {
        if std::env::var_os("PERTEST_SLEEP").is_some() {
            std::thread::sleep(std::time::Duration::from_secs(60));
        }
    }

    #[test]
    fn counts_a() {
        assert_eq!(N.fetch_add(1, Ordering::SeqCst), 0);
    }

    #[test]
    fn counts_b() {
        assert_eq!(N.fetch_add(1, Ordering::SeqCst), 0);
    }
}
""",
    "bare/Cargo.toml": '[package]\nname = "bare"\nversion = "0.1.0"\nedition = "2021"\n\n'
                       '[dev-dependencies]\nt = { path = "../t" }\n',
    "bare/src/lib.rs": r"""#![no_std]

pub const fn double(x: u32) -> u32 {
    x * 2
}

#[cfg(test)]
mod tests {
    extern crate std;

    #[test]
    fn doubles() {
        assert_eq!(super::double(2), 4);
    }

    #[test]
    fn formats() {
        assert_eq!(std::format!("{}", super::double(3)), "6");
    }
}
""",
}
RESULT = re.compile(r"^test (\S+) \.\.\. (ok|FAILED|ignored|TIMEOUT)", re.M)
EVERY = {
    "tests::adds": "ok", "tests::fails": "FAILED", "tests::panics": "ok",
    "tests::ignored": "ignored", "tests::aborts": "FAILED", "tests::sleeps": "ok",
    "tests::counts_a": "ok", "tests::counts_b": "ok", "tests::doubles": "ok",
    "tests::formats": "ok",
}
BARE = {"tests::doubles", "tests::formats"}


@dataclass(frozen=True)
class Input:
    blaze: str


def typed(text):
    """`Input` from INPUT; exits naming the field that is wrong."""
    if text == "-":
        text = sys.stdin.read()
    elif not text.lstrip().startswith("{"):
        text = open(text).read()
    try:
        value = json.loads(text)
    except json.JSONDecodeError as e:
        sys.exit(f"pertest.py: INPUT is not JSON: {e}")
    if not isinstance(value, dict):
        sys.exit("pertest.py: INPUT is a JSON object")
    known = {f.name for f in fields(Input)}
    if unknown := sorted(set(value) - known):
        sys.exit(f"pertest.py: INPUT has no field {', '.join(unknown)} (fields: {', '.join(sorted(known))})")
    if not isinstance(value.get("blaze"), str):
        sys.exit("pertest.py: INPUT's `blaze` is the path of the blaze engine, a string")
    return Input(**value)


def run(ws, env, *args, **more):
    ran = subprocess.run(["blaze", "test", *args], cwd=ws, env=dict(env, **more),
                         capture_output=True, text=True, timeout=TIMEOUT)
    return ran.returncode, ran.stdout, ran.stderr


def results(out):
    return dict(RESULT.findall(out))


def main():
    if len(sys.argv) != 2:
        sys.exit(__doc__)
    given = typed(sys.argv[1])
    bin_dir = os.path.dirname(os.path.abspath(given.blaze))
    if not os.path.exists(os.path.join(bin_dir, "blaze")):
        raise SystemExit(f"no blaze beside {given.blaze}")
    root = tempfile.mkdtemp(prefix="blaze-pertest-")
    ws, home = os.path.join(root, "ws"), tempfile.mkdtemp(prefix="bz-pt-", dir="/tmp")
    for path, text in FILES.items():
        os.makedirs(os.path.dirname(os.path.join(ws, path)), exist_ok=True)
        open(os.path.join(ws, path), "w").write(text)
    env = dict(os.environ, PATH=f"{bin_dir}{os.pathsep}{os.environ['PATH']}", BLAZE_HOME=home,
               RUST_BACKTRACE="0", BLAZE_CACHE=os.path.join(root, "store"), BLAZE_TESTS="all")
    for name in ("CARGO_TARGET_DIR", "CARGO_INCREMENTAL", "RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS",
                 "BLAZE_SANDBOX", "BLAZE_TEST_PROCESSES", "BLAZE_TEST_TIMEOUT"):
        env.pop(name, None)
    verdicts = []

    def step(what, verdict, out=""):
        verdicts.append(verdict == "ok")
        print(f"{what:52} {verdict}", flush=True)
        if verdict != "ok":
            print(out[-6000:])

    try:
        code, out, err = run(ws, env, "--no-fail-fast")
        got = results(out)
        verdict = "ok"
        if code != 101:
            verdict = f"ENDED {code}, NOT 101"
        elif got != EVERY:
            verdict = f"RESULTS {sorted(set(got.items()) ^ set(EVERY.items()))} ARE NOT LIBTEST'S"
        elif "the output of a failing test" not in out:
            verdict = "THE FAILING TEST'S OUTPUT IS NOT PRINTED"
        elif re.search(r"^---- tests::fails stdout ----\n(?:(?!\n\n)[\s\S])*^(running|test result)",
                       out, re.M):
            verdict = "A TEST'S OUTPUT HOLDS ITS CHILD'S LIBTEST REPORT"
        step("every test, each in a process of its own", verdict, out + err)

        # One test at a time, so which executable runs first is the only freedom: whichever
        # it is, each reported runs every test, and `t`, which fails, is reported.
        code, out, err = run(ws, env, "--", "--test-threads=1")
        got = results(out)
        of_t = {it: v for it, v in EVERY.items() if it not in BARE}
        of_bare = {it: v for it, v in EVERY.items() if it in BARE}
        verdict = "ok"
        if code != 101:
            verdict = f"ENDED {code}, NOT 101"
        elif got not in (of_t, EVERY):
            verdict = f"WITHOUT --no-fail-fast RAN {sorted(got.items())}, NOT {sorted(of_t.items())} " \
                f"WITH OR WITHOUT {sorted(of_bare.items())}"
        step("a failure stops what had not started, as cargo", verdict, out + err)

        code, out, err = run(ws, env, "-p", "t", "sleeps", BLAZE_TEST_TIMEOUT="2",
                             PERTEST_SLEEP="1")
        verdict = "ok"
        if code != 101 or results(out).get("tests::sleeps") != "TIMEOUT":
            verdict = f"ENDED {code}, sleeps {results(out).get('tests::sleeps')}"
        elif "it took more than its 2.0 s" not in out + err:
            verdict = "IT DOES NOT SAY IT TIMED OUT"
        step("BLAZE_TEST_TIMEOUT=2 kills a test past it", verdict, out + err)

        code, out, err = run(ws, env, "-p", "t", "--no-fail-fast", BLAZE_TEST_PROCESSES="one",
                             PERTEST_ONE="1")
        counted = sorted(results(out).get(f"tests::counts_{it}") for it in "ab")
        verdict = "ok" if code == 101 and counted == ["FAILED", "ok"] else \
            f"ENDED {code}, THE COUNTING TESTS {counted}"
        step("BLAZE_TEST_PROCESSES=one: libtest's threads", verdict, out + err)
    finally:
        subprocess.run(["blaze", "stop"], cwd=ws, env=env, capture_output=True, timeout=120)
    print(f"{sum(verdicts)} of {len(verdicts)} right; under {root}, the daemon's home {home}")
    if not all(verdicts):
        sys.exit(1)
    shutil.rmtree(root, ignore_errors=True)
    shutil.rmtree(home, ignore_errors=True)


if __name__ == "__main__":
    main()
