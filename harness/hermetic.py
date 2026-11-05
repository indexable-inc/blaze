#!/usr/bin/env python3
"""hermetic.py: a test blaze can see whole runs in the sandbox and its pass is kept, so a run
that changed nothing it depends on does not run it again; a test blaze can't see whole runs
outside, says so, and runs every time, never stood in for by an old pass. Linux and macOS.

usage: hermetic.py    (BLAZE_BIN names the blaze engine built; `blaze` is found next to it)

What blaze promises (docs/cargo.md, "Sandboxed tests" and "Which tests run"): under
`blaze test --reached` a test of a package that opts in (`tests = true` under
`[package.metadata.blaze.sandbox]`) runs in the sandbox, which records what it reads, and its
pass is kept by the functions it executed and what it read (files, the environment); a test
whose executed functions and reads are as they were is reported cached. A test the package's
`unsandboxed-tests` names, and every test of a package that does not opt in, runs outside: what
it reads is not known, so it is reported `outside` and runs every time.

The harness reads each result from `blaze test --reached --message-format=json`'s
`blaze-test-result` lines (crates/blaze-cargo/src/cli/json.rs, `test_result`; storelib's
`test_results`), never libtest's text. A workspace of two packages: `kept` opts in, with a pure
test (three calls deep), one that reads a file of the package, one that reads an environment
variable, and one its `unsandboxed-tests` names that writes the temporary directory and dials an
address outside; `plain` does not opt in and has one pure test. Steps, each a run:

- the first: every test runs and passes, and only the two outside are `outside`;
- again, nothing changed: the three sandboxed tests are cached, kept by what they executed, and
  the two outside run again;
- the file changed: the test that reads it runs, the other sandboxed ones stay cached;
- the variable changed in the client's environment: the test that reads it runs, the others
  stay cached;
- the function three calls below the pure test changed: it runs, the others stay cached.

In every run after the first the outside tests run and say `outside`, and no test is both
cached and outside.
"""

import os
import shutil
import sys
from collections.abc import Mapping
from dataclasses import dataclass

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
from storelib import Daemon, Failed, Ran, TestLine, blaze_of, scratch, test_results

# Seconds one `blaze test` may take: the first builds both packages and starts the sandbox.
TIMEOUT = 900
KNOB = "HERMETIC_KNOB"
DATA = "kept/data/input.txt"
DEEP = (
    "kept/src/lib.rs",
    "    fn level3(x: u32) -> u32 {\n        x\n    }",
    "    fn level3(x: u32) -> u32 {\n        if x == u32::MAX { 0 } else { x }\n    }",
)

FILES = {
    "Cargo.toml": '[workspace]\nmembers = ["kept", "plain"]\nresolver = "2"\n',
    "kept/Cargo.toml": """[package]
name = "kept"
version = "0.1.0"
edition = "2021"

[package.metadata.blaze.sandbox]
tests = true
unsandboxed-tests = ["tests::named_outside"]
""",
    DATA: "input one\n",
    "kept/src/lib.rs": f"""pub fn add(a: u32, b: u32) -> u32 {{
    deep::level1(a) + b
}}

mod deep {{
    pub fn level1(x: u32) -> u32 {{
        level2(x)
    }}

    fn level2(x: u32) -> u32 {{
        level3(x)
    }}

    fn level3(x: u32) -> u32 {{
        x
    }}
}}

pub fn input() -> String {{
    std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/data/input.txt")).unwrap()
}}

pub fn knob() -> Option<String> {{
    std::env::var("{KNOB}").ok()
}}

#[cfg(test)]
mod tests {{
    use super::*;

    #[test]
    fn pure() {{
        assert_eq!(add(2, 3), 5);
    }}

    #[test]
    fn reads_input() {{
        assert!(input().starts_with("input"));
    }}

    #[test]
    fn reads_env() {{
        assert_ne!(knob().as_deref(), Some("never set"));
    }}

    // Named by `unsandboxed-tests`: it writes outside its package and dials an address outside
    // the machine, neither of which blaze sees, so its pass can never be kept.
    #[test]
    fn named_outside() {{
        let path = std::env::temp_dir().join(format!("hermetic-outside-{{}}", std::process::id()));
        std::fs::write(&path, b"outside").unwrap();
        std::fs::remove_file(&path).unwrap();
        let address = "192.0.2.1:9".parse().unwrap();
        let _ = std::net::TcpStream::connect_timeout(&address, std::time::Duration::from_millis(50));
    }}
}}
""",
    "plain/Cargo.toml": '[package]\nname = "plain"\nversion = "0.1.0"\nedition = "2021"\n',
    "plain/src/lib.rs": """pub fn twice(x: u32) -> u32 {
    x * 2
}

#[cfg(test)]
mod tests {
    #[test]
    fn plain_pure() {
        assert_eq!(super::twice(4), 8);
    }
}
""",
}

SANDBOXED = frozenset({"tests::pure", "tests::reads_input", "tests::reads_env"})
OUTSIDE = frozenset({"tests::named_outside", "tests::plain_pure"})
ALL = SANDBOXED | OUTSIDE


@dataclass(frozen=True)
class Step:
    """What one run has to show: the sandboxed tests that run (the rest of them cached)."""

    what: str
    runs: frozenset[str]


def verdict(step: Step, ran: Ran, lines: list[TestLine], first: bool) -> str:
    """`ok`, or what is wrong with the run, upper case where blaze is wrong."""
    if ran.status != 0:
        return f"ENDED {ran.status}"
    by_name: dict[str, TestLine] = {}
    for line in lines:
        if line.name in by_name:
            return f"{line.name}: TWO RESULTS"
        by_name[line.name] = line
    if frozenset(by_name) != ALL:
        return f"TESTS {sorted(frozenset(by_name) ^ ALL)} ARE MISSING OR EXTRA"
    wrong: list[str] = []
    for name, line in sorted(by_name.items()):
        stood = line.cached
        if stood is not None and line.outside:
            wrong.append(f"{name}: CACHED AND OUTSIDE")
        elif name in OUTSIDE:
            if not line.outside:
                wrong.append(f"{name}: RAN OUTSIDE THE SANDBOX, NOT SAID `outside`")
            if stood is not None:
                wrong.append(f"{name}: OUTSIDE, YET AN OLD PASS STOOD IN FOR IT ({stood.basis})")
            elif line.outcome != "passed":
                wrong.append(f"{name}: {line.outcome.upper()}\n{line.output[-1500:]}")
        elif line.outside:
            wrong.append(f"{name}: OPTED IN, YET RAN OUTSIDE THE SANDBOX")
        elif first or name in step.runs:
            if stood is not None:
                wrong.append(f"{name}: CACHED, SHOULD HAVE RUN ({stood.basis}: {stood.why or ''})")
            elif line.outcome != "passed":
                wrong.append(f"{name}: {line.outcome.upper()}\n{line.output[-1500:]}")
        elif stood is None:
            wrong.append(f"{name}: RAN AGAIN ({line.outcome}), SHOULD HAVE BEEN KEPT")
        elif stood.outcome != "passed" or line.outcome != "unreached":
            wrong.append(f"{name}: CACHED AS {stood.outcome}, REPORTED {line.outcome}")
        elif stood.basis != "executed":
            wrong.append(f"{name}: KEPT BY {stood.basis}, NOT BY WHAT IT EXECUTED: {stood.why}")
    return "; ".join(wrong) or "ok"


def edit(ws: str, path: str, old: str | None, new: str) -> None:
    """`old` replaced by `new` once in `ws/path`; `old` None writes the file whole."""
    at = os.path.join(ws, path)
    if old is None:
        with open(at, "w") as file:
            file.write(new)
        return
    with open(at) as file:
        text = file.read()
    if old not in text:
        raise Failed(f"{at} has no {old!r}")
    with open(at, "w") as file:
        file.write(text.replace(old, new, 1))


def main() -> None:
    engine = os.environ.get("BLAZE_BIN")
    if not engine:
        raise SystemExit("hermetic.py: BLAZE_BIN names no blaze engine")
    blaze = blaze_of(os.path.join(os.path.dirname(os.path.abspath(engine)), "blaze"))
    root = scratch("blaze-hermetic-")
    ws, home = os.path.join(root, "kinds"), scratch("bz-he-")
    for path, text in FILES.items():
        os.makedirs(os.path.dirname(os.path.join(ws, path)), exist_ok=True)
        with open(os.path.join(ws, path), "w") as file:
            file.write(text)
    # What the tests read is the client's: the variable starts unset, and a sandbox, a test
    # filter or a backtrace setting of the caller's would change what runs.
    unset: dict[str, str | None] = {
        name: None for name in (KNOB, "BLAZE_SANDBOX", "BLAZE_TESTS", "CARGO_INCREMENTAL")
    }
    daemon = Daemon(blaze, home, name="hermetic", env={**unset, "RUST_BACKTRACE": "0"})
    if not daemon.listening:
        raise SystemExit(f"the daemon did not start:\n{daemon.log()[-3000:]}\nlog: {daemon.log_path}")
    results: list[bool] = []

    def run(step: Step, first: bool = False, env: Mapping[str, str | None] | None = None) -> None:
        ran = daemon.command(ws, "test", "--reached", "--message-format=json", env=env, timeout=TIMEOUT)
        said = verdict(step, ran, test_results(ran.stdout), first)
        results.append(said == "ok")
        print(f"{step.what:58} {said}", flush=True)
        if said != "ok":
            print(f"$ {' '.join(ran.argv)}\n{ran.stdout[-4000:]}\n{ran.stderr[-4000:]}", flush=True)

    try:
        run(Step("the first run: every test runs, two of them outside", SANDBOXED), first=True)
        run(Step("again: the sandboxed ones kept, the outside ones run", frozenset()))
        edit(ws, DATA, None, "input two\n")
        run(Step("the file one test reads changed: that test runs", frozenset({"tests::reads_input"})))
        knob = {KNOB: "turned"}
        run(
            Step("the variable one test reads changed: that test runs", frozenset({"tests::reads_env"})),
            env=knob,
        )
        path, old, new = DEEP
        edit(ws, path, old, new)
        run(Step("a function three calls below the pure test: it runs", frozenset({"tests::pure"})), env=knob)
    except Failed as error:
        raise SystemExit(f"hermetic.py: {error}\nunder {root}, the daemon's home {home}") from error
    finally:
        daemon.stop()
    print(f"{sum(results)} of {len(results)} right; under {root}, the daemon's home {home}")
    if not all(results):
        sys.exit(1)
    shutil.rmtree(root, ignore_errors=True)
    shutil.rmtree(home, ignore_errors=True)


if __name__ == "__main__":
    main()
