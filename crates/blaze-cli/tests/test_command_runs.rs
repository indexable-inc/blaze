//! `blaze test` RUNS tests (crates/blaze-cli/src/test_drive.rs over test_loop.rs and
//! test_run.rs; docs/v2/plan/design/test.md "End state", "One process per test"): a one-crate
//! package, each `#[test]` fn entered in a process of its own under the lazy parent `blaze run`
//! uses, its output captured, its verdict read from how the process ended.
//!
//! The case `a_pass_and_a_failure` (`passes` returns, `fails` runs `assert_eq!(1, 2)`) prints on
//! standard output, after its check's own lines (`CHECK_FIRST`: D147, D151), exactly these lines, in this order (`<t>` is the failing test's time,
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

const BOTH_SUMMARY: &str = "test result: FAILED. ran 2: 1 passed, 1 failed; 0 reused from kept results; 0 not kept; 0 ignored; 0 benches not run; 0 filtered out; 0 flaky";
const PASS_SUMMARY: &str = "test result: ok. ran 1: 1 passed, 0 failed; 0 reused from kept results; 0 not kept; 0 ignored; 0 benches not run; 0 filtered out; 0 flaky";

/// What `blaze test` prints before anything of its tests: its check's own lines, exactly. D147:
/// "`run` and `test` read the engine's answer: one check (`check_for_start`), then the start";
/// D151: "`run` prints the check's own incomplete line". While borrows are notes (body_types.rs
/// `BORROWS_ARE_ERRORS` is false) a clean check is "incomplete" (`blaze explain 3`). [`run`]
/// takes these off and refuses a run that did not print them first; every expectation below is
/// of what follows.
const CHECK_FIRST: &str = "0 errors: blaze check\n";

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

fn write_source(path: &Path, text: &str, what: &str) {
    std::fs::write(path, text).unwrap_or_else(|error| panic!("writing {what}: {error}"));
}

fn pause_poll() {
    std::thread::sleep(Duration::from_millis(10));
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
        write_source(&path, source, name);
    }
    let result = check(&dir, &home);
    // A detached `blaze gc --background` child may still be unlinking its lock file inside `home`.
    for attempt in 0..100 {
        match std::fs::remove_dir_all(&base) {
            Ok(()) => break,
            Err(_) if !base.exists() => break,
            Err(_) if attempt + 1 < 100 => pause_poll(),
            Err(err) => panic!("remove the scratch command fixture: {err}"),
        }
    }
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
                pause_poll();
            }
            panic!("`blaze test` past {DEADLINE:?} in {}: the command hung (rule 18)", dir.display());
        }
        pause_poll();
    };
    let said = Said {
        stdout: std::fs::read_to_string(stdout).expect("read blaze's standard output"),
        // Lossy on purpose: a test child's raw standard error is quoted in it, and a child that
        // writes bytes that are not UTF-8 is evidence to show, not a reason to hide the rest.
        stderr: String::from_utf8_lossy(&std::fs::read(stderr).expect("read blaze's standard error")).into_owned(),
        code: status.code(),
    };
    // The check's lines first (D147, D151), then the tests' alone.
    let Some(tests) = said.stdout.strip_prefix(CHECK_FIRST) else {
        panic!("{}", evidence(&format!("the check's own lines first, exactly:\n{CHECK_FIRST}"), &said));
    };
    let tests = tests.to_owned();
    Said { stdout: tests, ..said }
}

/// Quote each stream as ASCII so binary data and long lines survive the test log.
fn evidence(why: &str, said: &Said) -> String {
    byte_evidence(why, said.code, said.stderr.as_bytes(), said.stdout.as_bytes())
}

fn byte_evidence(why: &str, code: Option<i32>, stderr: &[u8], stdout: &[u8]) -> String {
    use std::fmt::Write as _;

    let mut report = format!("--- status {code:?}; {why}\n");
    for (name, bytes) in [("stderr", stderr), ("stdout", stdout)] {
        writeln!(report, "--- {name} ({} bytes)", bytes.len()).expect("write the stream length");
        let mut width = 0;
        for &byte in bytes.iter().take(4000) {
            let escaped = std::ascii::escape_default(byte);
            if width + escaped.len() > 100 {
                report.push('\n');
                width = 0;
            }
            width += escaped.len();
            report.extend(escaped.map(char::from));
        }
        if width != 0 {
            report.push('\n');
        }
    }
    report
}

/// THE CHECK both cases and the control use: `Ok` when `said` is exactly the expected run of
/// the package (with `fails` when `failing`), else what differs.
fn judge(said: &Said, failing: bool) -> Result<(), String> {
    let lines: Vec<&str> = said.stdout.lines().collect();
    let tail = if failing { ["FAILED fails: ended by signal 6", BOTH_SUMMARY].as_slice() } else { [PASS_SUMMARY].as_slice() };
    if lines.len() < tail.len() || lines[lines.len() - tail.len()..] != *tail {
        return Err(format!("the summary is not\n{}\n", tail.join("\n")));
    }
    if failing {
        if said.code != Some(1) {
            return Err(format!("a suite with a failing test ended with status {:?}", said.code));
        }
        let first = lines[0];
        let time = first.strip_prefix("FAILED fails after ").and_then(|rest| rest.strip_suffix(" s: ended by signal 6"));
        let timed = time.is_some_and(|t| t.split_once('.').is_some_and(|(s, ms)| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()) && ms.len() == 3 && ms.bytes().all(|b| b.is_ascii_digit())));
        if !timed {
            return Err("the first line is not `FAILED fails after d.ddd s: ended by signal 6`".into());
        }
        let Some(header) = lines.get(1).and_then(|line| line.strip_prefix("---- fails output: ")).and_then(|s| s.strip_suffix(" ----")) else {
            return Err("the failure's output block does not open on the second line".into());
        };
        if header.contains(", the first ") {
            return Err("the assertion's output exceeded the reporter's cap and its beginning was cut".into());
        }
        if header.strip_suffix(" bytes").and_then(|n| n.parse::<usize>().ok()).is_none() {
            return Err("the failure's output header has no byte count".into());
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
fn a_pass_and_a_failure() {
    let lib = format!("{PASS}\n{FAIL}");
    let (said, stderr, stdout) = fixture("test_command_runs_both", &[("Cargo.toml", MANIFEST), ("src/lib.rs", &lib)], |dir, home| {
        // The shared runner decodes stdout before returning. Preserve the file evidence
        // even if a malformed byte stops that decode.
        let result = std::panic::catch_unwind(|| run(dir, home, &["--", "--deadline", "20s"]));
        let stderr = std::fs::read(home.join("test.stderr")).expect("read the command's stderr bytes");
        let stdout = std::fs::read(home.join("test.stdout")).expect("read the command's stdout bytes");
        let said = match result {
            Ok(said) => said,
            Err(payload) => {
                eprintln!("{}", byte_evidence("the command runner did not return a status", None, &stderr, &stdout));
                std::panic::resume_unwind(payload);
            }
        };
        (said, stderr, stdout)
    });
    let report = |why: &str| byte_evidence(why, said.code, &stderr, &stdout);
    let diagnostic = std::str::from_utf8(&stderr)
        .unwrap_or_else(|_| panic!("{}", report("the command wrote binary stderr outside its escaped report")));
    if let Err(why) = judge(&said, true) {
        panic!("{}", report(&why));
    }
    // Check the child's fd 2 independently of the report's merged stdout/stderr tail.
    let (before, block) = diagnostic.split_once(" signal 6: stderr (")
        .unwrap_or_else(|| panic!("{}", report("missing the child's signal report")));
    assert!(before.ends_with("/fails"), "{}", report("signal reported for the wrong test"));
    let (length, block) = block.split_once(" bytes) ----\n")
        .unwrap_or_else(|| panic!("{}", report("missing the stderr byte count")));
    let total = length.parse::<usize>().unwrap_or_else(|_| panic!("{}", report("invalid stderr byte count")));
    let (child, parent) = block.split_once("---- end escaped stderr ----\n")
        .unwrap_or_else(|| panic!("{}", report("incomplete escaped child stderr")));
    assert!(child.lines().all(|line| line.is_ascii() && line.len() <= 100), "{}", report("invalid escaped line"));
    // Only fd 2 writes in this fixture. The stdout reporter adds a final newline if
    // necessary, so take exactly the announced number of original bytes from its block.
    let shown = said.stdout.splitn(3, '\n').nth(2).expect("judge checked the output header");
    let original = shown.as_bytes().get(..total).unwrap_or_else(|| panic!("{}", report("incorrect child stderr count")));
    let expected = original.iter().take(4000).flat_map(|&byte| std::ascii::escape_default(byte));
    assert!(child.lines().flat_map(str::bytes).eq(expected), "{}", report("fd 2 differs from the failure's output"));
    let original = std::str::from_utf8(original).unwrap_or_else(|_| panic!("{}", report("the assertion was not text")));
    assert!(original.lines().any(|line| line.contains("panicked at ") && line.ends_with("src/lib.rs:6:5:"))
        && original.contains("assertion `left == right` failed"), "{}", report("fd 2 is missing the assertion"));
    assert!(parent.lines().any(|line| line.starts_with("last published: ") && line != "last published: none")
        && parent.lines().any(|line| line == "pending need: none")
        && parent.lines().any(|line| line == "refusal stub: no refused first call observed by the parent"),
        "{}", report("missing first-call evidence for the panic"));
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
#[cfg(target_os = "macos")]
fn pinned_core_transparent_arguments_and_results() {
    let source = r#"
use core::{mem::{ManuallyDrop, transmute}, num::NonZeroU32, ptr::NonNull};

extern "C" fn pointer(x: NonNull<u8>) -> NonNull<u8> { x }
extern "C-unwind" fn nullable(x: Option<NonNull<u8>>) -> Option<NonNull<u8>> { x }
extern "C" fn integer(x: NonZeroU32) -> NonZeroU32 { x }
fn manual(x: ManuallyDrop<u64>) -> ManuallyDrop<u64> { x }

#[test]
fn passes() {
    let p: extern "C" fn(NonNull<u8>) -> NonNull<u8> = pointer;
    let o: extern "C-unwind" fn(Option<NonNull<u8>>) -> Option<NonNull<u8>> = nullable;
    let n: extern "C" fn(NonZeroU32) -> NonZeroU32 = integer;
    let m: fn(ManuallyDrop<u64>) -> ManuallyDrop<u64> = manual;
    // SAFETY: these are the documented field and Option ABIs. Only nullable
    // receives null, and the argument to NonZeroU32 is nonzero.
    unsafe {
        let p: extern "C" fn(*mut u8) -> *mut u8 = transmute(p);
        let o: extern "C-unwind" fn(*mut u8) -> *mut u8 = transmute(o);
        let n: extern "C" fn(u32) -> u32 = transmute(n);
        let m: fn(u64) -> u64 = transmute(m);
        let mut byte = 19u8;
        let ptr = &raw mut byte;
        assert_eq!(o(p(ptr)), ptr);
        assert!(o(core::ptr::null_mut()).is_null());
        assert_eq!(n(0xfedcba98), 0xfedcba98);
        assert_eq!(m(0xfedcba9876543210), 0xfedcba9876543210);
    }
}
"#;
    let said = blaze_test("test_command_core_transparent", source);
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
    let correct = format!("{refusal}\n{REFUSED_SUMMARY}\n");
    assert!(only_dependent_test_refused(&correct, Some(1)));
    let one_stopped = format!("{refusal}\n");
    assert!(!only_dependent_test_refused(&one_stopped, Some(1)));
    let not_run_as_pass = format!("{refusal}\ntest result: ok. ran 2: 2 passed, 0 failed\n");
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
        "test result: ok. ran 2: 2 passed, 0 failed; 0 reused from kept results; 0 not kept; 0 ignored; 0 benches not run; 0 filtered out; 0 flaky",
    ];
    assert_eq!(said.code, Some(0), "{}", evidence("two passing packages", &said));
    assert!(said.stdout.lines().eq(expected), "{}", evidence(&expected.join("\n"), &said));
}

/// `alpha` depends on `beta`, under `[profile.dev] panic = "abort"`: alpha's test program would
/// link beta's lib, which the unit graph builds once, under the profile's abort, for the bin's
/// graph and the test's alike. Cargo builds a test program's dependencies to unwind (the Cargo
/// Book, Profiles, `panic`), which blaze does not yet (engine_start.rs `abort_dependency`), so
/// `blaze test` stops up front naming the setting, the test and the first such dependency, with
/// status 125 (main.rs STATUSES: "blaze did not start the program"). CONTROLS (rule 23): the same
/// workspace without the setting is not refused; `runs_both_workspace_packages` (abort, no
/// dependency) runs its 2 tests.
#[test]
#[cfg(target_os = "macos")]
fn tests_over_a_dependency_built_to_abort_stop_up_front() {
    const ALPHA_ON_BETA: &str = "[package]\nname = \"alpha\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\nbeta = { path = \"../beta\" }\n";
    const LOCK: &str = "version = 4\n\n[[package]]\nname = \"alpha\"\nversion = \"0.1.0\"\ndependencies = [\n \"beta\",\n]\n\n[[package]]\nname = \"beta\"\nversion = \"0.1.0\"\n";
    let said = |abort: bool| {
        let manifest = if abort { format!("{WORKSPACE}\n[profile.dev]\npanic = \"abort\"\n") } else { WORKSPACE.to_owned() };
        let files = [
            ("Cargo.toml", manifest.as_str()),
            ("Cargo.lock", LOCK),
            ("alpha/Cargo.toml", ALPHA_ON_BETA),
            ("alpha/src/lib.rs", "pub fn a() -> u8 {\n    beta::b()\n}\n\n#[test]\nfn calls() {\n    assert_eq!(a(), 1);\n}\n"),
            ("beta/Cargo.toml", BETA),
            ("beta/src/lib.rs", "pub fn b() -> u8 {\n    1\n}\n"),
        ];
        let name = if abort { "test_command_abort_dependency" } else { "test_command_abort_dependency_control" };
        fixture(name, &files, |dir, home| run(dir, home, &["--", "--deadline", "20s"]))
    };
    let refused = said(true);
    let words = format!("{}{}", refused.stdout, refused.stderr);
    assert_eq!(refused.code, Some(125), "{}", evidence("blaze did not start the tests (125)", &refused));
    assert!(
        words.contains("tests of a profile with panic = \"abort\" need their dependencies built to unwind: not built yet")
            && words.contains("the tests of `alpha 0.1.0` link `beta 0.1.0`"),
        "{}",
        evidence("the refusal names the setting, the test and its first dependency", &refused)
    );
    let control = said(false);
    let control_words = format!("{}{}", control.stdout, control.stderr);
    assert!(!control_words.contains("need their dependencies built to unwind"), "{}", evidence("CONTROL: an unwinding profile is not refused", &control));
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
            "FAILED fails after 0.012 s: ended by signal 6\n---- fails output: 79 bytes ----\nthread 'main' (1) panicked at src/lib.rs:6:5:\nassertion `left == right` failed\n{rule}\nFAILED fails: ended by signal 6\n{BOTH_SUMMARY}\n"
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

#[test]
#[cfg(target_os = "macos")]
fn incremental_reruns_only_changed_test_and_reuses_unchanged() {
    let make_lib = |a_expr: &str, b_expr: &str| {
        format!(
            "fn helper_a() -> u32 {{ {a_expr} }}\n\
             fn helper_b() -> u32 {{ {b_expr} }}\n\
             #[test]\nfn test_a() {{ assert_eq!(helper_a(), 42); }}\n\
             #[test]\nfn test_b() {{ assert_eq!(helper_b(), 100); }}\n"
        )
    };
    let initial = make_lib("40 + 2", "100");
    fixture("test_command_incremental_selective", &[("Cargo.toml", MANIFEST), ("src/lib.rs", &initial)], |dir, home| {
        let lib_path = dir.join("src/lib.rs");

        // 1. Cold run: both tests run and both results are kept.
        let cold = run(dir, home, &["--", "--deadline", "20s"]);
        let expect_cold = "test result: ok. ran 2: 2 passed, 0 failed; 0 reused from kept results; 0 not kept; 0 ignored; 0 benches not run; 0 filtered out; 0 flaky\n";
        assert_eq!(cold.code, Some(0), "{}", evidence("cold run passes", &cold));
        assert_eq!(cold.stdout, expect_cold, "{}", evidence(expect_cold, &cold));

        // 2. No-op rerun: both tests reused from kept results (ran 0).
        let warm = run(dir, home, &["--", "--deadline", "20s"]);
        let expect_warm = "test result: ok. ran 0: 0 passed, 0 failed; 2 reused from kept results; 0 not kept; 0 ignored; 0 benches not run; 0 filtered out; 0 flaky\n";
        assert_eq!(warm.code, Some(0), "{}", evidence("warm rerun passes", &warm));
        assert_eq!(warm.stdout, expect_warm, "{}", evidence(expect_warm, &warm));

        // 3. Edit helper_a only: only test_a reruns; test_b is reused from kept results.
        let edit_1 = make_lib("21 * 2", "100");
        write_source(&lib_path, &edit_1, "edit helper_a");
        let edited = run(dir, home, &["--", "--deadline", "20s"]);
        let expect_edited = "test result: ok. ran 1: 1 passed, 0 failed; 1 reused from kept results; 0 not kept; 0 ignored; 0 benches not run; 0 filtered out; 0 flaky\n";
        assert_eq!(edited.code, Some(0), "{}", evidence("only test_a reruns after helper_a edit", &edited));
        assert_eq!(edited.stdout, expect_edited, "{}", evidence(expect_edited, &edited));

        // 4. Edit helper_a to a second value, then revert back to edit_1: Effect::Look finds the
        //    previously kept pass at edit_1's address without re-running test_a (ran 0).
        write_source(&lib_path, &make_lib("41 + 1", "100"), "edit helper_a again");
        let edited2 = run(dir, home, &["--", "--deadline", "20s"]);
        assert_eq!(edited2.code, Some(0), "{}", evidence("second helper_a edit reruns only test_a", &edited2));
        assert_eq!(edited2.stdout, expect_edited, "{}", evidence(expect_edited, &edited2));

        write_source(&lib_path, &edit_1, "revert helper_a to edit_1");
        let reverted = run(dir, home, &["--", "--deadline", "20s"]);
        assert_eq!(reverted.code, Some(0), "{}", evidence("reverted edit reuses kept result", &reverted));
        assert_eq!(reverted.stdout, expect_warm, "{}", evidence(expect_warm, &reverted));

        // 5. --run-all bypasses kept results and runs both tests.
        let forced = run(dir, home, &["--", "--run-all", "--deadline", "20s"]);
        assert_eq!(forced.code, Some(0), "{}", evidence("--run-all passes", &forced));
        assert_eq!(forced.stdout, expect_cold, "{}", evidence(expect_cold, &forced));

        // 6. Soundness control: break helper_a -> only test_a runs and FAILS, test_b stays reused.
        write_source(&lib_path, &make_lib("99", "100"), "break helper_a");
        let broken = run(dir, home, &["--", "--deadline", "20s"]);
        let expect_broken_summary = "test result: FAILED. ran 1: 0 passed, 1 failed; 1 reused from kept results; 0 not kept; 0 ignored; 0 benches not run; 0 filtered out; 0 flaky";
        assert_eq!(broken.code, Some(1), "{}", evidence("broken helper_a must fail", &broken));
        assert!(
            broken.stdout.contains("FAILED test_a")
                && !broken.stdout.contains("test_b")
                && broken.stdout.lines().last() == Some(expect_broken_summary),
            "{}",
            evidence(expect_broken_summary, &broken),
        );
    });
}

#[test]
#[cfg(target_os = "macos")]
fn impure_test_is_not_kept_and_reruns() {
    let source = "#[test]\nfn reads_clock() {\n    let _ = std::time::SystemTime::now();\n}\n";
    fixture("test_command_impure_clock", &[("Cargo.toml", MANIFEST), ("src/lib.rs", source)], |dir, home| {
        let check = |label: &str, said: &Said| {
            assert_eq!(said.code, Some(0), "{}", evidence(label, said));
            let mut lines = said.stdout.lines();
            let first = lines.next().unwrap_or("");
            let second = lines.next().unwrap_or("");
            assert!(
                first.starts_with("not kept reads_clock: ")
                    && second.starts_with("test result: ok. ran 1: 1 passed, 0 failed; 0 reused from kept results; 1 not kept (")
                    && lines.next().is_none(),
                "{}",
                evidence(label, said),
            );
        };
        let first = run(dir, home, &["--", "--deadline", "20s"]);
        check("first impure run is not kept", &first);

        // Second invocation must NOT reuse the clock-reading test.
        let second = run(dir, home, &["--", "--deadline", "20s"]);
        check("second impure run reruns instead of reusing", &second);
    });
}
