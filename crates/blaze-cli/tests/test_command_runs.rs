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
//! thread '...' (...) panicked at src/lib.rs:6:5:
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
    fixture(name, &[("Cargo.toml", MANIFEST), ("src/lib.rs", lib)], |dir, home| {
        run(dir, home, &["--", "--deadline", "20s"])
    })
}

fn fixture<T>(name: &str, files: &[(&str, &str)], check: impl FnOnce(&Path, &Path) -> T) -> T {
    let base = fresh_home(name, true);
    // The test profile names the test's folder by its canonical path (blaze-run child.rs
    // `seatbelt` refuses another); the system's temporary folder is behind a link on macOS.
    let base = base.canonicalize().expect("the scratch folder's canonical path");
    let home = base.join("home");
    let dir = base.join("project");
    for &(name, source) in files {
        let path = dir.join(name);
        std::fs::create_dir_all(path.parent().expect("a file in the fixture"))
            .unwrap_or_else(|error| panic!("making the folder for {name}: {error}"));
        std::fs::write(&path, source).unwrap_or_else(|error| panic!("writing {name}: {error}"));
    }
    let result = check(&dir, &home);
    std::fs::remove_dir_all(&base).expect("remove the scratch command fixture");
    result
}

fn run(dir: &Path, home: &Path, args: &[&str]) -> Said {
    // A full pipe must never stall the compiler while the fixture waits for it to exit.
    let stdout = home.join("test.stdout");
    let stderr = home.join("test.stderr");
    let mut child = Command::new(env!("CARGO_BIN_EXE_blaze"))
        .arg("test")
        .args(args)
        .current_dir(dir)
        .env("PWD", dir)
        .env("HOME", home)
        .env("CARGO_HOME", home.join(".cargo"))
        .stdin(Stdio::null())
        .stdout(std::fs::File::create(&stdout).expect("capture blaze's standard output"))
        .stderr(std::fs::File::create(&stderr).expect("capture blaze's standard error"))
        .spawn()
        .expect("blaze starts");
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().expect("a wait on blaze") {
            break status;
        }
        if started.elapsed() > DEADLINE {
            child.kill().expect("stop the timed-out test command");
            let killed = Instant::now();
            while child.try_wait().expect("reap the killed test command").is_none() {
                assert!(killed.elapsed() < Duration::from_secs(10), "reaping blaze in {}", dir.display());
                std::thread::sleep(Duration::from_millis(10));
            }
            panic!("`blaze test` past {DEADLINE:?} in {}: the command hung (rule 18)", dir.display());
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    Said {
        stdout: std::fs::read_to_string(stdout).expect("read blaze's standard output"),
        stderr: std::fs::read_to_string(stderr).expect("read blaze's standard error"),
        code: status.code(),
    }
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
        if !lines[2..rule].iter().any(|line| line.contains("panicked at ") && line.ends_with("src/lib.rs:6:5:")) {
            return Err("the panic's location is not the assert_eq! invocation at src/lib.rs:6:5".into());
        }
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
#[ignore = "OPEN (the real run of 2026-11-18 06:10, after source positions landed): the failing test's code is still not BUILT: `NOT RUN fails: core::io::error::os_functions::OS_FUNCTIONS at core/src/io/error/os_functions_atomic.rs:14:1: the static ...: its value's allocation 1 holds a fn's address at byte 0, which a static's record does not name`; `Location::caller` is no longer the refusal; the passing test runs and the suite reports `ran 1: 1 passed, 0 failed, 1 not run (code not built)`"]
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

// Deliberately put beta first in the manifest. The command orders results by owner and path.
const WORKSPACE: &str = "[workspace]\nmembers = [\"beta\", \"alpha\"]\nresolver = \"2\"\n";
const ALPHA: &str = "[package]\nname = \"alpha\"\nversion = \"0.1.0\"\nedition = \"2024\"\n";
const BETA: &str = "[package]\nname = \"beta\"\nversion = \"0.1.0\"\nedition = \"2024\"\n";
const SHARED: &str = "mod shared { #[test] fn same() {} }\n";
const LOCK_ALPHA: &str = "version = 4\n\n[[package]]\nname = \"alpha\"\nversion = \"0.1.0\"\n";
const LOCK_BETA: &str = "\n[[package]]\nname = \"beta\"\nversion = \"0.1.0\"\n";

const INVENTORY: &str = concat!(
    "alpha/lib/shared::same: test\n",
    "alpha/test:devcase/uses_dev_dependency: test\n",
    "alpha/test:empty: 0 tests\n",
    "alpha/test:own/main: test\n",
    "beta/lib/shared::same: test\n",
    "4 tests, 0 benchmarks\n",
);

fn listed_each_unit(stdout: &str, code: Option<i32>) -> bool {
    code == Some(0) && stdout == INVENTORY
}

#[test]
fn lists_each_selected_unit_once() {
    let targets = format!(
        "{ALPHA}\n[dev-dependencies]\nbeta = {{ path = \"../beta\" }}\n\n\
         [[test]]\nname = \"own\"\npath = \"tests/own.rs\"\nharness = false\n"
    );
    let dependency = format!("pub fn helper() {{}}\n{SHARED}");
    let lock = format!("{LOCK_ALPHA}dependencies = [\"beta\"]\n{LOCK_BETA}");
    let files = [
        ("Cargo.toml", WORKSPACE),
        ("Cargo.lock", lock.as_str()),
        ("alpha/Cargo.toml", targets.as_str()),
        ("alpha/src/lib.rs", SHARED),
        ("alpha/tests/devcase.rs", "#[test] fn uses_dev_dependency() { beta::helper(); }\n"),
        ("alpha/tests/empty.rs", ""),
        ("alpha/tests/own.rs", "fn main() {}\n#[test] fn hidden_by_harness_false() {}\n"),
        ("beta/Cargo.toml", BETA),
        ("beta/src/lib.rs", dependency.as_str()),
    ];
    fixture("test_command_unit_inventory", &files, |dir, home| {
        let all = run(dir, home, &["--", "--list"]);
        assert!(listed_each_unit(&all.stdout, all.code), "{}", evidence(INVENTORY, &all));

        let repeated = run(dir, home, &["-p", "beta", "--package=alpha", "--package", "beta", "--", "--list"]);
        assert!(listed_each_unit(&repeated.stdout, repeated.code), "{}", evidence(INVENTORY, &repeated));

        let selected = run(dir, home, &["-p", "beta", "--", "--list", "shared::same", "--exact"]);
        let one = "beta/lib/shared::same: test\n1 tests, 0 benchmarks\n";
        assert_eq!(selected.code, Some(0), "{}", evidence(one, &selected));
        assert_eq!(selected.stdout, one, "{}", evidence(one, &selected));

        // The package and target labels identify a result, but libtest matches a filter
        // against its Rust path alone. "beta" is in the label and absent from shared::same.
        let filtered = run(dir, home, &["-p", "beta", "--", "--list", "beta"]);
        assert_eq!(filtered.code, Some(0), "{}", evidence("a filter with no matches", &filtered));
        assert_eq!(filtered.stdout, "0 tests, 0 benchmarks\n");
    });
}

#[test]
fn the_inventory_oracle_catches_one_unit_and_display_path_identity() {
    assert!(listed_each_unit(INVENTORY, Some(0)));
    let one_unit = "alpha/lib/shared::same: test\n1 tests, 0 benchmarks\n";
    assert!(!listed_each_unit(one_unit, Some(0)));
    let by_display_path = INVENTORY.replace("beta/lib/shared::same: test\n", "").replace("4 tests", "3 tests");
    assert!(!listed_each_unit(&by_display_path, Some(0)));
}

const REFUSED: &str = "a `ref` binding or a binding with a subpattern";
const REFUSED_SUMMARY: &str = "test result: FAILED. ran 1: 1 passed, 0 failed, 1 not run (code not built)";

fn only_dependent_test_refused(stdout: &str, code: Option<i32>) -> bool {
    let mut lines = stdout.lines();
    let Some(refusal) = lines.next() else { return false };
    let Some(instance_and_cause) = refusal.strip_prefix("NOT RUN a_needs_callee: ") else { return false };
    instance_and_cause.contains("refused_callee")
        && instance_and_cause.contains(REFUSED)
        && lines.next() == Some("not kept b_returns: was not traced")
        && lines.next().is_some_and(|summary| summary.starts_with(REFUSED_SUMMARY))
        && lines.next().is_none()
        && code.is_some_and(|status| status != 0)
}

#[test]
#[cfg(target_os = "macos")]
fn refused_first_call_does_not_hide_the_next_test() {
    // A ref binding extends the value temporary's lifetime (Reference, "Temporary lifetime
    // extension"). The builder supports the place initializer only (walk/matches/ref_bind.rs)
    // and refuses this valid value initializer in walk.rs plain_binding. Keep it in the
    // *callee* so the launcher first enters a successfully built test.
    let lib = "fn refused_callee() { let ref _word = 7u8; }\n\
               #[test] fn a_needs_callee() { refused_callee(); }\n\
               #[test] fn b_returns() {}\n";
    let said = fixture("test_command_refused_call", &[("Cargo.toml", MANIFEST), ("src/lib.rs", lib)], |dir, home| {
        run(dir, home, &["--", "--test-threads", "1", "--deadline", "20s"])
    });
    assert!(only_dependent_test_refused(&said.stdout, said.code), "{}", evidence(REFUSED_SUMMARY, &said));
}

#[test]
fn the_refusal_oracle_catches_one_stopped_and_not_run_as_pass() {
    let refusal = format!("NOT RUN a_needs_callee: refused_callee: {REFUSED}");
    let passed = "not kept b_returns: was not traced";
    let correct = format!("{refusal}\n{passed}\n{REFUSED_SUMMARY}\n");
    assert!(only_dependent_test_refused(&correct, Some(1)));
    let one_stopped = format!("{refusal}\n");
    assert!(!only_dependent_test_refused(&one_stopped, Some(1)));
    let not_run_as_pass = format!("{refusal}\n{passed}\ntest result: ok. ran 2: 2 passed, 0 failed\n");
    assert!(!only_dependent_test_refused(&not_run_as_pass, Some(0)));
}

#[test]
#[cfg(target_os = "macos")]
fn runs_both_workspace_packages() {
    let lock = format!("{LOCK_ALPHA}{LOCK_BETA}");
    let manifest = format!("{WORKSPACE}\n[profile.dev]\npanic = \"abort\"\n");
    // Cargo ignores the profile panic setting for a test build. Both unit graphs must
    // select the unwind-only module. This checks cfg selection independently of code keys.
    let lib = format!("#[cfg(panic = \"unwind\")]\n{SHARED}");
    let files = [
        ("Cargo.toml", manifest.as_str()),
        ("Cargo.lock", lock.as_str()),
        ("alpha/Cargo.toml", ALPHA),
        ("alpha/src/lib.rs", lib.as_str()),
        ("beta/Cargo.toml", BETA),
        ("beta/src/lib.rs", lib.as_str()),
    ];
    let said = fixture("test_command_two_packages", &files, |dir, home| {
        run(dir, home, &["--", "--test-threads", "2", "--deadline", "20s"])
    });
    let expected = [
        "not kept alpha/lib/shared::same: was not traced",
        "not kept beta/lib/shared::same: was not traced",
        "test result: ok. ran 2: 2 passed, 0 failed; 0 reused from kept results; 2 not kept (2 was not traced); 0 ignored; 0 benches not run; 0 filtered out; 0 flaky",
    ];
    assert_eq!(said.code, Some(0), "{}", evidence("two passing packages", &said));
    assert!(said.stdout.lines().eq(expected), "{}", evidence(&expected.join("\n"), &said));
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
            "FAILED fails after 0.012 s: ended by signal 6\n---- fails output: 79 bytes ----\nthread 'main' (1) panicked at src/lib.rs:6:5:\nassertion `left == right` failed\n{rule}\nFAILED fails: ended by signal 6\n{NOT_KEPT}\n{BOTH_SUMMARY}\n"
        ),
        stderr: String::new(),
        code: Some(0),
    };
    assert!(judge(&quiet, true).is_err(), "the check passed a failing suite that ended with status 0");
    // The same transcript with a failing status is the expected run: the check is not refusing everything.
    let right = Said { code: Some(1), ..quiet };
    assert_eq!(judge(&right, true), Ok(()));
    let shifted = Said { stdout: right.stdout.replace("src/lib.rs:6:5", "src/lib.rs:5:5"), ..right };
    assert!(judge(&shifted, true).is_err(), "the check passed a panic at the enclosing fn instead of its assertion");
}
