#!/usr/bin/env python3
"""hermetic.py: `blaze test` tells each test hermetic or not from the code it reaches, keeps a
hermetic test's pass by its code alone, with no sandbox and nothing opted into, and says why
each other test is not, by kind, in `target/blaze/tests.ndjson`.

usage: hermetic.py    (BLAZE_BIN names the blaze engine built; `blaze` is found next to it)

A library has a test for each kind of reason (files, the environment, the network, a process
started and one ended, foreign code, assembly, a function pointer made of data, an atomic
static, a `static mut` and a function pointer kept in a static that any caller sets; the clock,
a `HashMap`'s seed and a thread, which don't block) and pure tests beside them (arithmetic
through three calls, a sort, formatting, a `LazyLock`, a thread-local, and a function chosen
once and kept in a static as memchr chooses its code by the CPU). Steps:

- `blaze test --why --json` runs no test and prints a line a test: each test is hermetic as
  its kind says, its reasons have that kind with a chain that starts at the test, and the
  lines are sorted; `tests.ndjson` has the same lines, `tests.summary.json` the counts.
- `blaze test` runs every test, then again only those that are not hermetic.
- an edit to the function three calls below a pure test runs it again, and no other pure test.
- `--why` prints the table, with what makes the most tests not hermetic.

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
LIB = r"""use std::sync::LazyLock;
use std::sync::atomic::{AtomicUsize, Ordering};

pub fn add(a: u32, b: u32) -> u32 {
    deep::level1(a) + b
}

mod deep {
    pub fn level1(x: u32) -> u32 {
        level2(x)
    }

    fn level2(x: u32) -> u32 {
        level3(x)
    }

    fn level3(x: u32) -> u32 {
        x
    }
}

pub fn sorted(mut v: Vec<u32>) -> Vec<u32> {
    v.sort();
    v
}

static TABLE: LazyLock<Vec<u32>> = LazyLock::new(|| (0..4).collect());
static COUNTER: AtomicUsize = AtomicUsize::new(0);
static mut TOTAL: u32 = 0;

thread_local! {
    static LOCAL: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

extern "C" {
    fn getpid() -> i32;
}

fn answer() -> u32 {
    42
}

mod dispatch {
    use std::sync::atomic::{AtomicPtr, Ordering};

    type Fn = *mut ();
    type RealFn = unsafe fn(u32) -> u32;

    // As memchr writes it: in a macro, so the static is in a block in the body's block.
    macro_rules! chosen {
        ($x:ident) => {{
            static FN: AtomicPtr<()> = AtomicPtr::new(detect as Fn);

            unsafe fn fast(x: u32) -> u32 {
                x + 1
            }

            unsafe fn slow(x: u32) -> u32 {
                x + 1
            }

            unsafe fn detect(x: u32) -> u32 {
                let fun = if std::hint::black_box(true) { fast as RealFn } else { slow as RealFn };
                FN.store(fun as Fn, Ordering::Relaxed);
                unsafe { fun(x) }
            }

            let fun = FN.load(Ordering::Relaxed);
            unsafe { std::mem::transmute::<Fn, RealFn>(fun)($x) }
        }};
    }

    pub fn chosen(x: u32) -> u32 {
        chosen!(x)
    }

    pub fn hooked(f: RealFn, x: u32) -> u32 {
        unsafe fn same(x: u32) -> u32 {
            x
        }

        static HOOK: AtomicPtr<()> = AtomicPtr::new(same as Fn);
        HOOK.store(f as Fn, Ordering::Relaxed);
        let fun = HOOK.load(Ordering::Relaxed);
        unsafe { std::mem::transmute::<Fn, RealFn>(fun)(x) }
    }

    pub unsafe fn twice(x: u32) -> u32 {
        x * 2
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pure_add() {
        assert_eq!(add(1, 2), 3);
    }

    #[test]
    fn pure_sort() {
        assert_eq!(sorted(vec![3, 1, 2]), [1, 2, 3]);
    }

    #[test]
    fn pure_format() {
        assert_eq!(format!("{}-{}", 1, "a"), "1-a");
    }

    #[test]
    fn pure_lazy() {
        assert_eq!(TABLE.len(), 4);
    }

    #[test]
    fn pure_thread_local() {
        LOCAL.with(|it| it.set(it.get() + 1));
        assert_eq!(LOCAL.with(|it| it.get()), 1);
    }

    #[test]
    fn pure_dispatch() {
        assert_eq!(dispatch::chosen(1), 2);
        assert_eq!(dispatch::chosen(2), 3);
    }

    #[test]
    fn global_hook() {
        assert_eq!(dispatch::hooked(dispatch::twice, 2), 4);
    }

    #[test]
    fn fs() {
        assert!(std::fs::metadata("Cargo.toml").is_ok());
    }

    #[test]
    fn env() {
        let _ = std::env::var("HOME");
    }

    #[test]
    fn net() {
        let _ = std::net::TcpListener::bind("127.0.0.1:0");
    }

    #[test]
    fn process() {
        let _ = std::process::Command::new("true").status();
    }

    #[test]
    fn exit() {
        if std::hint::black_box(false) {
            std::process::exit(3)
        }
    }

    #[test]
    fn clock() {
        let _ = std::time::Instant::now();
    }

    #[test]
    fn random() {
        let mut map = std::collections::HashMap::new();
        map.insert(1, 2);
        assert_eq!(map[&1], 2);
    }

    #[test]
    fn thread() {
        assert_eq!(std::thread::spawn(|| 2).join().unwrap(), 2);
    }

    #[test]
    fn ffi() {
        assert!(unsafe { getpid() } > 0);
    }

    #[test]
    fn asm() {
        unsafe { std::arch::asm!("nop") }
    }

    #[test]
    fn indirect() {
        let f: fn() -> u32 = unsafe { std::mem::transmute(answer as usize) };
        assert_eq!(f(), 42);
    }

    #[test]
    fn global_atomic() {
        COUNTER.fetch_add(1, Ordering::Relaxed);
    }

    #[test]
    #[allow(static_mut_refs)]
    fn global_static_mut() {
        unsafe { TOTAL += 1 }
    }
}
"""
FILES = {
    "Cargo.toml": '[package]\nname = "kinds"\nversion = "0.1.0"\nedition = "2021"\n\n[workspace]\n',
    "src/lib.rs": LIB,
}
# Each test, by libtest's name: the kind it has to be found to reach, and whether it blocks.
BLOCKS = {"fs": "fs", "env": "env", "net": "net", "process": "process", "exit": "process",
          "ffi": "ffi", "asm": "asm", "indirect": "indirect", "global_atomic": "global-state",
          "global_static_mut": "global-state", "global_hook": "global-state"}
FLAKY_ONLY = {"clock": "clock", "random": "random", "thread": "thread"}
PURE = {"pure_add", "pure_sort", "pure_format", "pure_lazy", "pure_thread_local",
        "pure_dispatch"}
ALL = {f"tests::{it}" for it in [*BLOCKS, *FLAKY_ONLY, *PURE]}
NOT_HERMETIC = {f"tests::{it}" for it in BLOCKS}
RAN = re.compile(r"^test (\S+) \.\.\. (ok|ignored|FAILED)", re.M)
DEEP = ("src/lib.rs", "    fn level3(x: u32) -> u32 {\n        x\n    }",
        "    fn level3(x: u32) -> u32 {\n        if x == u32::MAX { 0 } else { x }\n    }")


def run(cmd, ws, env, *args):
    ran = subprocess.run([*cmd, "test", *args], cwd=ws, env=env, capture_output=True, text=True,
                         timeout=TIMEOUT)
    return ran.returncode, ran.stdout, ran.stderr


def lines_right(lines):
    """What is wrong with the `--why --json` lines, or None."""
    names = [it["name"] for it in lines]
    if set(names) != ALL:
        return f"TESTS {sorted(set(names) ^ ALL)} ARE MISSING OR EXTRA"
    order = [(it["package"], it["target"], it["name"]) for it in lines]
    if order != sorted(order):
        return "NOT SORTED BY PACKAGE, TARGET AND NAME"
    for line in lines:
        if line.get("v") != 1:
            return f"{line['name']}: SCHEMA {line.get('v')}"
        short = line["name"].removeprefix("tests::")
        kinds = {it["kind"] for it in line["reasons"]}
        flaky = {it["kind"] for it in line["nondeterministic"]}
        if short in BLOCKS:
            want = BLOCKS[short]
            if line["hermetic"] or want not in kinds:
                return f"{short}: HERMETIC {line['hermetic']}, REASONS {sorted(kinds)}, NOT {want}"
            reason = next(it for it in line["reasons"] if it["kind"] == want)
            if not reason["chain"] or not reason["chain"][0]["fn"].endswith(f"tests::{short}"):
                return f"{short}: THE CHAIN DOES NOT START AT THE TEST: {reason['chain']}"
        elif short in FLAKY_ONLY:
            want = FLAKY_ONLY[short]
            if not line["hermetic"] or want not in flaky:
                return (f"{short}: HERMETIC {line['hermetic']}, REASONS {sorted(kinds)}, "
                        f"NONDETERMINISTIC {sorted(flaky)}, NOT {want}")
        elif not line["hermetic"] or kinds:
            return f"{short}: A PURE TEST FOUND NOT HERMETIC: {line['reasons']}"
        if line["result"] != "not-run":
            return f"{short}: --why RAN IT ({line['result']})"
    return None


def main():
    bin_dir = os.path.dirname(os.path.abspath(os.environ["BLAZE_BIN"]))
    if not os.path.exists(os.path.join(bin_dir, "blaze")):
        raise SystemExit(f"no blaze beside {os.environ['BLAZE_BIN']}")
    cmd = ["blaze"]
    root = tempfile.mkdtemp(prefix="blaze-hermetic-")
    ws, home = os.path.join(root, "kinds"), tempfile.mkdtemp(prefix="bz-he-", dir="/tmp")
    for path, text in FILES.items():
        os.makedirs(os.path.dirname(os.path.join(ws, path)), exist_ok=True)
        open(os.path.join(ws, path), "w").write(text)
    env = dict(os.environ, PATH=f"{bin_dir}{os.pathsep}{os.environ['PATH']}", BLAZE_HOME=home,
               RUST_BACKTRACE="0")
    for name in ("CARGO_TARGET_DIR", "CARGO_INCREMENTAL", "RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS",
                 "BLAZE_TESTS", "BLAZE_SANDBOX"):
        env.pop(name, None)
    results = []

    def step(what, verdict):
        results.append(verdict == "ok")
        print(f"{what:44} {verdict}", flush=True)

    try:
        deadline = time.monotonic() + 300
        while True:
            code, out, err = run(cmd, ws, env, "--why", "--json")
            if code == 0 and "[blaze]" in err and out.strip():
                break
            if time.monotonic() > deadline:
                raise SystemExit(f"never served by the daemon:\n{err[-3000:]}")
            time.sleep(0.5)
        lines = [json.loads(it) for it in out.splitlines() if it.strip()]
        verdict = lines_right(lines) or "ok"
        if verdict != "ok":
            print(out[-6000:])
        step("--why --json: each test's kind", verdict)

        written = open(os.path.join(ws, "target", "blaze", "tests.ndjson")).read()
        summary = json.load(open(os.path.join(ws, "target", "blaze", "tests.summary.json")))
        verdict = "ok"
        if written != out:
            verdict = "tests.ndjson IS NOT WHAT --why --json PRINTED"
        elif (summary["tests"], summary["not_hermetic"]) != (len(ALL), len(NOT_HERMETIC)):
            verdict = f"SUMMARY COUNTS {summary['tests']} TESTS, {summary['not_hermetic']} NOT HERMETIC"
        elif summary["by_kind"].get("global-state") != 3:
            verdict = f"SUMMARY BY KIND {summary['by_kind']}"
        step("tests.ndjson and tests.summary.json", verdict)

        code, out, err = run(cmd, ws, env)
        ran = {name for name, _ in RAN.findall(out)}
        step("blaze test: every test, the first time",
             "ok" if code == 0 and ran == ALL else f"ENDED {code} HAVING RUN {sorted(ran)}")
        code, out, err = run(cmd, ws, env)
        ran = {name for name, _ in RAN.findall(out)}
        verdict = "ok" if code == 0 and ran == NOT_HERMETIC else \
            f"ENDED {code}; RAN {sorted(ran - NOT_HERMETIC)}; DID NOT RUN {sorted(NOT_HERMETIC - ran)}"
        step("again: only those not hermetic", verdict)
        lines = [json.loads(it) for it in open(os.path.join(ws, "target", "blaze", "tests.ndjson"))]
        cached = {it["name"] for it in lines if it["cached"] == "hit" and it["result"] == "cached"}
        step("tests.ndjson: the hermetic ones cached",
             "ok" if cached == ALL - NOT_HERMETIC else f"CACHED {sorted(cached)}")

        path, old, new = DEEP
        text = open(os.path.join(ws, path)).read()
        open(os.path.join(ws, path), "w").write(text.replace(old, new, 1))
        code, out, err = run(cmd, ws, env)
        ran = {name for name, _ in RAN.findall(out)}
        want = NOT_HERMETIC | {"tests::pure_add"}
        verdict = "ok" if code == 0 and ran == want else \
            f"ENDED {code}; RAN {sorted(ran - want)}; DID NOT RUN {sorted(want - ran)}"
        step("a function three calls below a pure test", verdict)

        code, out, err = run(cmd, ws, env, "--why")
        verdict = "ok"
        if code != 0 or RAN.search(out):
            verdict = f"ENDED {code}, OR RAN TESTS"
        elif not re.search(rf"^{len(ALL) - len(NOT_HERMETIC)} of {len(ALL)} tests are hermetic", out, re.M):
            verdict = "THE TABLE DOES NOT COUNT THE HERMETIC TESTS"
        elif not re.search(r"^\s+1\s+global-state\s+", out, re.M):
            verdict = "THE TABLE DOES NOT RANK THE REASONS"
        if verdict != "ok":
            print(out[-4000:])
        step("--why: the table", verdict)
    finally:
        subprocess.run([*cmd, "stop"], cwd=ws, env=env, capture_output=True, timeout=120)
    print(f"{sum(results)} of {len(results)} right; under {root}, the daemon's home {home}")
    if not all(results):
        sys.exit(1)
    shutil.rmtree(root, ignore_errors=True)
    shutil.rmtree(home, ignore_errors=True)


if __name__ == "__main__":
    main()
