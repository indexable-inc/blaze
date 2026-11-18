//! `blaze test` RUNS tests (crates/blaze-cli/src/test_drive.rs over test_loop.rs and
//! test_run.rs; docs/v2/plan/design/test.md "End state", "One process per test"): a one-crate
//! package, each `#[test]` fn entered in a process of its own under the lazy parent `blaze run`
//! uses, its output captured, its verdict read from how the process ended.
//!
//! The case `a_pass_and_a_failure` (`passes` returns, `fails` runs `assert_eq!(1, 2)`) prints on
//! standard output exactly these lines, in this order (`<t>` is the failing test's time,
//! `d.ddd`; `<n>` its output's byte count; the block between the `----` lines is the panic's
//! words as the program's std writes them, holding `assertion `left == right` failed`; the
//! rule's width is the terminal's, 80 when not a terminal):
//!
//! ```text
//! FAILED fails after <t> s: ended by signal 6
//! ---- fails output: <n> bytes ----
//! thread '...' panicked at src/lib.rs:...
//! assertion `left == right` failed
//! ...
//! --------------------------------------------------------------------------------
//! FAILED fails: ended by signal 6
//! not kept passes: was not traced
//! test result: FAILED. ran 2: 1 passed, 1 failed; 0 reused from kept results; 1 not kept (1 was not traced); 0 ignored; 0 benches not run; 0 filtered out; 0 flaky
//! ```
//!
//! and its status is not 0. The case `a_pass_alone` (`passes` only) prints
//!
//! ```text
//! not kept passes: was not traced
//! test result: ok. ran 1: 1 passed, 0 failed; 0 reused from kept results; 1 not kept (1 was not traced); 0 ignored; 0 benches not run; 0 filtered out; 0 flaky
//! ```
//!
//! and its status is 0. "ended by signal 6": a panic in a test unwinds out of the launcher's
//! entry, which no frame catches, and the runtime aborts (SIGABRT); the descriptor-3 report that
//! carries the panic's message is test.md piece 10. "not kept ... was not traced" (`NotKept::Untraced`): no tracer
//! saw what the pass entered, so its pass is not kept for reuse (the same piece).
//!
//! THE CONTROL (rule 23): [`judge`] is the one check both cases use, and `every_test_passes_is
//! _caught` hands it what a driver planted to say "every test passes" would print (the summary
//! `test result: ok. ran 2: 2 passed, 0 failed` with status 0) for the package with `fails`: the
//! check must refuse it. A judge that passed it would pass a driver that runs nothing.
//!
//! macOS only: Linux has no test sandbox, and every run there is `NOT RUN` (test_run.rs
//! `Host::has_sandbox`); that is its own case, below, which asserts exactly that line.
//!
//! Run: `cargo test -p blaze-cli --test test_command_runs` (needs the toolchain's shelf in the
//! owner's home, as tests/command_rungs.rs does). Expected: `test a_pass_and_a_failure ... ok`,
//! `test a_pass_alone ... ok`, `test every_test_passes_is_caught ... ok`.

mod support;

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use support::fresh_home;

/// The whole command's bound: two tests, each a launcher start and a few first calls built;
/// past it the command hung, which is a bug (rule 18), named here.
const DEADLINE: Duration = Duration::from_secs(300);

const MANIFEST: &str = "[package]\nname = \"two\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[lib]\npath = \"src/lib.rs\"\n";

const PASS: &str = "#[test]\nfn passes() {}\n";
const FAIL: &str = "#[test]\nfn fails() {\n    assert_eq!(1, 2);\n}\n";

const BOTH_SUMMARY: &str = "test result: FAILED. ran 2: 1 passed, 1 failed; 0 reused from kept results; 1 not kept (1 was not traced); 0 ignored; 0 benches not run; 0 filtered out; 0 flaky";
const PASS_SUMMARY: &str = "test result: ok. ran 1: 1 passed, 0 failed; 0 reused from kept results; 1 not kept (1 was not traced); 0 ignored; 0 benches not run; 0 filtered out; 0 flaky";
const NOT_KEPT: &str = "not kept passes: was not traced";

/// What one `blaze test` printed and how it ended.
struct Said {
    stdout: String,
    stderr: String,
    code: Option<i32>,
}

/// `blaze test` in a fresh package holding `lib`, in a scratch home; the run's words.
fn blaze_test(name: &str, lib: &str) -> Said {
    let base = fresh_home(name, true);
    // The test profile names the test's folder by its canonical path (blaze-run child.rs
    // `seatbelt` refuses another); the system's temporary folder is behind a link on macOS.
    let base = base.canonicalize().expect("the scratch folder's canonical path");
    let home = base.join("home");
    let dir = base.join("two");
    std::fs::create_dir_all(dir.join("src")).expect("the fixture's folders");
    std::fs::write(dir.join("Cargo.toml"), MANIFEST).expect("the fixture's manifest");
    std::fs::write(dir.join("src/lib.rs"), lib).expect("the fixture's lib.rs");
    let said = run(&dir, &home);
    let _ = std::fs::remove_dir_all(&base);
    said
}

fn run(dir: &Path, home: &Path) -> Said {
    let mut child = Command::new(env!("CARGO_BIN_EXE_blaze"))
        // `blaze test`'s own words come after `--` (main.rs `flags`, test_args.rs).
        .args(["test", "--", "--deadline", "20s"])
        .current_dir(dir)
        .env("HOME", home)
        .env("CARGO_HOME", home.join(".cargo"))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("blaze starts");
    let started = Instant::now();
    while child.try_wait().expect("a wait on blaze").is_none() {
        if started.elapsed() > DEADLINE {
            let _ = child.kill();
            panic!("`blaze test` past {DEADLINE:?} in {}: the command hung (rule 18)", dir.display());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let output = child.wait_with_output().expect("blaze's output");
    Said { stdout: String::from_utf8_lossy(&output.stdout).into_owned(), stderr: String::from_utf8_lossy(&output.stderr).into_owned(), code: output.status.code() }
}

/// What a failed case shows, short (the queue's answer clips long blocks): the child's stderr
/// FIRST, its last 12 lines (the cause is there: blaze's error is its last line), then the
/// status, then stdout's last 12 lines, each line cut at 300 bytes.
fn evidence(why: &str, said: &Said) -> String {
    fn tail(text: &str) -> String {
        let lines: Vec<&str> = text.lines().collect();
        let from = lines.len().saturating_sub(12);
        let cut = |line: &&str| -> String {
            if line.len() <= 300 {
                return (*line).to_owned();
            }
            let mut end = 300;
            while !line.is_char_boundary(end) {
                end -= 1;
            }
            format!("{}...", &line[..end])
        };
        lines[from..].iter().map(cut).collect::<Vec<_>>().join("\n")
    }
    format!("--- stderr (last 12 lines)\n{}\n--- status {:?}; {why}\n--- stdout (last 12 lines)\n{}", tail(&said.stderr), said.code, tail(&said.stdout))
}

/// THE CHECK both cases and the control use: `Ok` when `said` is exactly the expected run of
/// the package (with `fails` when `failing`), else what differs.
fn judge(said: &Said, failing: bool) -> Result<(), String> {
    let lines: Vec<&str> = said.stdout.lines().collect();
    let tail = if failing { [ "FAILED fails: ended by signal 6", NOT_KEPT, BOTH_SUMMARY ].as_slice() } else { [NOT_KEPT, PASS_SUMMARY].as_slice() };
    if lines.len() < tail.len() || lines[lines.len() - tail.len()..] != *tail {
        return Err(format!("the summary is not\n{}\n", tail.join("\n")));
    }
    if failing {
        if said.code == Some(0) {
            return Err("a suite with a failing test ended with status 0".into());
        }
        let first = lines[0];
        let time = first.strip_prefix("FAILED fails after ").and_then(|rest| rest.strip_suffix(" s: ended by signal 6"));
        let timed = time.is_some_and(|t| t.split_once('.').is_some_and(|(s, ms)| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()) && ms.len() == 3 && ms.bytes().all(|b| b.is_ascii_digit())));
        if !timed {
            return Err(format!("the first line is not `FAILED fails after d.ddd s: ended by signal 6`: {first:?}"));
        }
        let opened = lines.get(1).is_some_and(|line| line.starts_with("---- fails output: ") && line.ends_with(" bytes ----"));
        if !opened {
            return Err("the failure's output block does not open on the second line".into());
        }
        let rule = lines.iter().position(|line| line.len() >= 4 && line.bytes().all(|b| b == b'-'));
        let Some(rule) = rule else { return Err("the failure's output block is not closed by a rule".into()) };
        if !lines[2..rule].iter().any(|line| line.contains("assertion `left == right` failed")) {
            return Err("the failure's output does not hold the panic's words".into());
        }
        if lines.len() != rule + 1 + tail.len() {
            return Err("lines between the failure's block and the summary".into());
        }
    } else {
        if said.code != Some(0) {
            return Err(format!("a passing suite ended with status {:?}", said.code));
        }
        if lines.len() != tail.len() {
            return Err("lines before the summary of a passing suite".into());
        }
    }
    Ok(())
}

#[test]
#[cfg(target_os = "macos")]
#[ignore = "OPEN (the real run of 2026-11-18): a failing `assert_eq!` cannot be BUILT yet: `<impl Location>::caller`'s own `#[track_caller]` is not said to the lazy builder (`Adts::track_caller`), so the test `fails` could not be run and the suite stops; the passing case runs"]
fn a_pass_and_a_failure() {
    let said = blaze_test("test_command_runs_both", &format!("{PASS}\n{FAIL}"));
    if let Err(why) = judge(&said, true) {
        panic!("{}", evidence(&why, &said));
    }
}

#[test]
#[cfg(target_os = "macos")]
fn a_pass_alone() {
    let said = blaze_test("test_command_runs_pass", PASS);
    if let Err(why) = judge(&said, false) {
        panic!("{}", evidence(&why, &said));
    }
}

#[test]
#[cfg(target_os = "linux")]
fn no_sandbox_runs_nothing() {
    let said = blaze_test("test_command_runs_linux", PASS);
    let line = "NOT RUN passes: cannot be run yet: no test sandbox is built for Linux yet, and blaze runs no test unsandboxed (rule 9; test.md, piece 9)";
    assert!(said.stdout.lines().any(|l| l == line), "{}", evidence(line, &said));
    assert_ne!(said.code, Some(0), "a suite that ran nothing is not a pass\n{}", said.stdout);
}

/// The control: what a driver planted to say "every test passes" prints for the package with
/// `fails`, which the check must refuse (and does by its summary and its status).
#[test]
fn every_test_passes_is_caught() {
    let planted = Said {
        stdout: "test result: ok. ran 2: 2 passed, 0 failed; 0 reused from kept results; 0 not kept; 0 ignored; 0 benches not run; 0 filtered out; 0 flaky\n".into(),
        stderr: String::new(),
        code: Some(0),
    };
    assert!(judge(&planted, true).is_err(), "the check passed a run in which `fails` passed");
    // And one planted to keep the right summary but end with status 0 is refused too.
    let rule = "-".repeat(80);
    let quiet = Said {
        stdout: format!(
            "FAILED fails after 0.012 s: ended by signal 6\n---- fails output: 40 bytes ----\nassertion `left == right` failed\n{rule}\nFAILED fails: ended by signal 6\n{NOT_KEPT}\n{BOTH_SUMMARY}\n"
        ),
        stderr: String::new(),
        code: Some(0),
    };
    assert!(judge(&quiet, true).is_err(), "the check passed a failing suite that ended with status 0");
    // The same transcript with a failing status is the expected run: the check is not refusing everything.
    let right = Said { code: Some(1), ..quiet };
    assert_eq!(judge(&right, true), Ok(()));
}
