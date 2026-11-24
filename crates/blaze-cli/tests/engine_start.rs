//! `blaze run` and `blaze test` start from the engine's answer (engine_start.rs; run.md section
//! R, D147): the engine's check, then the start over what it stored, the gate being the call.
//!
//! Each case is the built `blaze` in a fresh package and a scratch home (tests/support), as
//! tests/test_command_runs.rs runs it, each command under a deadline (rule 18):
//!
//! - hello world prints through `run`, and the second run replays its start list (the start's
//!   profile, engine_start.rs `START_PROFILE`: the first run builds the front end, the second
//!   places its first answers from the list); control: the first run placed none;
//! - a fn declared in a body runs: a written local fn, and the blaze edition's hosted start;
//! - a two-test package whose tests both pass runs both and exits 0;
//! - `check`, then `test` in a second command, past a bin its features skip: the test runs;
//! - a test NOT RUN (it calls a test-only fn with a type error) beside a passing one exits
//!   nonzero; control: the passing test alone exits 0;
//! - a failing `assert_eq!` beside a passing test is counted failed and exits 1: ignored, open
//!   item D143 (the assert's code is refused by the builder; the ignore reason quotes it);
//! - an error in a fn no test calls does not stop the tests, and a test that calls it is NOT
//!   RUN with that fn's finding; control: the fn fixed, both run and pass;
//! - D191, each holder unit checks a body under its own view: (a) a lib fn a `#[cfg(test)] mod
//!   tests` test calls runs, `check` then `test`; (b) `cfg!(test)` in a body is true in the test
//!   build only (the bin's run of the same fn is the control); (c) a file a lib and a bin both
//!   `mod` runs in the bin; (d) a type error only under test is reported naming the view, and
//!   the non-test program runs;
//! - a test runs in its package's root, told `CARGO_MANIFEST_DIR` and its `CARGO_PKG_*`;
//! - a tuple struct's and a tuple variant's constructor, as fn values and called, from a test and
//!   from the bin, run after a check (gated under the ADT's head task); twin: one of an ADT whose
//!   head has an error is NOT RUN, naming the ADT.
//!
//! macOS only: Linux has no test sandbox (test_command_runs.rs says why).
//!
//! Run: `cargo test -p blaze-cli --test engine_start` (needs the toolchain's shelf in the
//! owner's home, as tests/command_rungs.rs does).

mod support;

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use support::fresh_home;

/// One command's bound: a check of a small package and a few first calls built.
const DEADLINE: Duration = Duration::from_secs(300);

const BIN: &str = "[package]\nname = \"hello\"\nversion = \"0.1.0\"\nedition = \"2024\"\n";
const HELLO: &str = "fn main() {\n    println!(\"hello, world\");\n}\n";
const LIB: &str = "[package]\nname = \"two\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[lib]\npath = \"src/lib.rs\"\n";
const PASS: &str = "#[test]\nfn passes() {}\n";
const FAIL: &str = "#[test]\nfn fails() {\n    assert_eq!(1, 2);\n}\n";
const ALSO: &str = "#[test]\nfn also() {}\n";
/// `broken` holds a type error and only the test unit holds it (`#[cfg(test)]`), so its row is
/// the test crate's own; `calls` calls it and is NOT RUN, which needs nothing D143 holds.
const CALLS_TEST_ONLY_BROKEN: &str = "#[cfg(test)]\nfn broken() -> u8 {\n    \"not a number\"\n}\n\n#[test]\nfn calls() {\n    broken();\n}\n";
/// `broken` holds a type error and no test calls it; `calls` calls it, `fine` does not.
const BROKEN: &str = "fn broken() -> u8 {\n    \"not a number\"\n}\n\n#[test]\nfn fine() {}\n\n#[test]\nfn calls() {\n    broken();\n}\n";
const FIXED: &str = "fn broken() -> u8 {\n    7\n}\n\n#[test]\nfn fine() {}\n\n#[test]\nfn calls() {\n    broken();\n}\n";

/// The status of a `check` that found nothing with `BORROWS_ARE_ERRORS = true`: 0.
const CLEAN_CHECK: Option<i32> = Some(0);
/// What [`CLEAN_CHECK`]'s evidence says.
const CLEAN_CHECK_RULE: &str = "a clean check exits 0 when every stage (including borrows) runs clean";

struct Said {
    stdout: String,
    stderr: String,
    code: Option<i32>,
}

impl Said {
    fn evidence(&self, why: &str) -> String {
        format!("--- status {:?}; {why}\n--- stderr\n{}\n--- stdout\n{}", self.code, self.stderr, self.stdout)
    }
}

/// A scratch package of `files` and its home; `check` is given both, then they are removed.
fn fixture<T>(name: &str, files: &[(&str, &str)], check: impl FnOnce(&Path, &Path) -> T) -> T {
    let base = fresh_home(name, true);
    let base = base.canonicalize().expect("the scratch folder's canonical path");
    let home = base.join("home");
    let dir = base.join("project");
    for &(name, source) in files {
        write(&dir, name, source);
    }
    let result = check(&dir, &home);
    std::fs::remove_dir_all(&base).expect("remove the scratch command fixture");
    result
}

fn write(dir: &Path, name: &str, source: &str) {
    let path = dir.join(name);
    std::fs::create_dir_all(path.parent().expect("a file in the fixture")).unwrap_or_else(|error| panic!("making the folder for {name}: {error}"));
    std::fs::write(&path, source).unwrap_or_else(|error| panic!("writing {name}: {error}"));
}

/// `blaze <command> <args>` in `dir` with `home`, under [`DEADLINE`].
fn blaze(dir: &Path, home: &Path, command: &str, args: &[&str]) -> Said {
    let stdout = home.join("command.stdout");
    let stderr = home.join("command.stderr");
    let mut child = Command::new(env!("CARGO_BIN_EXE_blaze"))
        .arg(command)
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
            child.kill().expect("stop the timed-out command");
            let _ = child.wait();
            panic!("`blaze {command}` past {DEADLINE:?} in {}: the command hung (rule 18)", dir.display());
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    Said {
        stdout: std::fs::read_to_string(stdout).expect("read blaze's standard output"),
        stderr: String::from_utf8_lossy(&std::fs::read(stderr).expect("read blaze's standard error")).into_owned(),
        code: status.code(),
    }
}

/// The start's profile's `start list:` line (engine_start.rs `START_PROFILE`, run_parent.rs
/// `StartTally`).
fn start_line(dir: &Path) -> String {
    let profile = std::fs::read_to_string(dir.join(".blaze/v2/start.txt")).expect("the start's profile is written after every start");
    profile.lines().find(|line| line.contains("start list: placed ")).map(str::to_owned).unwrap_or_else(|| panic!("no start list line in the start's profile:\n{profile}"))
}

/// `placed P of A first answers`: P.
fn placed(line: &str) -> u32 {
    let rest = line.split_once("start list: placed ").expect("the start list line").1;
    rest.split(' ').next().and_then(|n| n.parse().ok()).unwrap_or_else(|| panic!("no placed count in {line:?}"))
}

#[test]
#[cfg(target_os = "macos")]
fn hello_world_runs_and_the_second_run_replays_its_start_list() {
    fixture("engine_start_hello", &[("Cargo.toml", BIN), ("src/main.rs", HELLO)], |dir, home| {
        let first = blaze(dir, home, "run", &[]);
        assert_eq!((first.code, first.stdout.as_str()), (Some(0), "hello, world\n"), "{}", first.evidence("the first run"));
        let cold = start_line(dir);
        // Control: a fresh home has no list, so the first run places nothing and builds its head.
        assert_eq!(placed(&cold), 0, "the first run in a fresh home placed answers: {cold}");
        assert!(cold.contains("front end built: yes"), "{cold}");
        let second = blaze(dir, home, "run", &[]);
        assert_eq!((second.code, second.stdout.as_str()), (Some(0), "hello, world\n"), "{}", second.evidence("the second run"));
        let warm = start_line(dir);
        assert!(placed(&warm) > 0, "the second run did not replay its start list: {warm}");
        assert!(warm.contains("front end built: no"), "the second run built the front end again: {warm}");
        // The whole list: every first answer placed, a positioned group's too (start_list.rs
        // version 5), none resolved after it.
        assert!(warm.contains("resolved after the list 0;"), "the second run resolved first answers its list did not hold: {warm}");
    });
}

/// A blaze-edition binary: its root `fn main` is `bstd::main`'s input unasked (D104), so the
/// program starts at the launcher's local `bstd_hosted_start`.
const BLAZE_BIN: &str = "[package]\nname = \"hello\"\nversion = \"0.1.0\"\nedition = \"blaze-2026.11\"\n";
const EMPTY_MAIN: &str = "fn main() {}\n";
/// A `main` that calls a fn it declares: a body checked inside `main`'s check (front2
/// every_body.rs `each_from`), never an outline entry of its own.
const CALLS_LOCAL: &str = "fn main() {\n    fn say() {\n        println!(\"hello, world\");\n    }\n    say();\n}\n";

/// A fn declared in a body is run like any checked fn: its row is in the covers of the body
/// whose check reached it (run_ids.rs `workspace_of`, work.rs `covered`). Two forms: a written
/// local fn (edition 2024), and the blaze edition's entry, a fn `bstd::main` declares inside the
/// launcher (seen 2026-11-19 as exit 125, "the body ids hold nothing for" fn 0xd43caa19..,
/// one id for every `main`, the same fn). CONTROL: hello world, whose `main` declares nothing,
/// runs (`hello_world_runs_and_the_second_run_replays_its_start_list`).
#[test]
#[cfg(target_os = "macos")]
fn a_fn_declared_in_a_body_runs() {
    fixture("engine_start_local", &[("Cargo.toml", BIN), ("src/main.rs", CALLS_LOCAL)], |dir, home| {
        let said = blaze(dir, home, "run", &[]);
        assert_eq!((said.code, said.stdout.as_str()), (Some(0), "hello, world\n"), "{}", said.evidence("a local fn called from main"));
    });
    fixture("engine_start_blaze_edition", &[("Cargo.toml", BLAZE_BIN), ("src/main.rs", EMPTY_MAIN)], |dir, home| {
        let said = blaze(dir, home, "run", &[]);
        assert_eq!(said.code, Some(0), "{}", said.evidence("a blaze-edition empty main"));
        assert!(!said.stderr.contains("hold nothing"), "{}", said.evidence("the hosted start has a row"));
    });
}

#[test]
#[cfg(target_os = "macos")]
fn two_passing_tests_run_and_exit_zero() {
    let lib = format!("{PASS}\n{ALSO}");
    fixture("engine_start_two", &[("Cargo.toml", LIB), ("src/lib.rs", &lib)], |dir, home| {
        let said = blaze(dir, home, "test", &["--", "--deadline", "20s"]);
        assert!(said.stdout.contains("ran 2: 2 passed, 0 failed"), "{}", said.evidence("both tests ran"));
        assert!(!said.stdout.contains("NOT RUN"), "{}", said.evidence("nothing is NOT RUN"));
        assert_eq!(said.code, Some(0), "{}", said.evidence("a passing suite exits 0"));
    });
}

#[test]
#[cfg(target_os = "macos")]
fn a_test_not_run_beside_a_passing_one_exits_nonzero() {
    let lib = format!("{PASS}\n{CALLS_TEST_ONLY_BROKEN}");
    fixture("engine_start_not_run", &[("Cargo.toml", LIB), ("src/lib.rs", &lib)], |dir, home| {
        let said = blaze(dir, home, "test", &["--", "--deadline", "20s"]);
        assert!(said.stdout.contains("ran 1: 1 passed, 0 failed, 1 not run"), "{}", said.evidence("`passes` ran, `calls` did not"));
        assert!(said.stdout.lines().any(|line| line.contains("NOT RUN calls")), "{}", said.evidence("`calls` is NOT RUN by name"));
        assert_ne!(said.code, Some(0), "{}", said.evidence("a test NOT RUN fails the command"));
    });
    // Control: the passing test alone exits 0, so the status above is the NOT RUN's.
    fixture("engine_start_one", &[("Cargo.toml", LIB), ("src/lib.rs", PASS)], |dir, home| {
        let said = blaze(dir, home, "test", &["--", "--deadline", "20s"]);
        assert!(said.stdout.contains("ran 1: 1 passed, 0 failed"), "{}", said.evidence("the test ran"));
        assert_eq!(said.code, Some(0), "{}", said.evidence("a passing suite exits 0"));
    });
}

/// A package with a bin its features skip (`required-features`, the feature off), as tempo's
/// `localnet` is, and a lib with one passing test.
const SKIPPING: &str = "[package]\nname = \"two\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[lib]\npath = \"src/lib.rs\"\n\n[features]\nextra = []\n\n[[bin]]\nname = \"extra\"\npath = \"src/bin/extra.rs\"\nrequired-features = [\"extra\"]\n";
const EXTRA_BIN: &str = "fn main() {\n    let _ = 1u8;\n}\n";

/// THE TEMPO CASE (2026-11-21: `blaze test -p tempo-hardfork` after a whole `blaze check` stopped
/// with "the walk reached BodyCheck(..), which this request did not settle"): `check`, then
/// `test` in a SECOND command with nothing changed. The skipped bin's body has no row (no body
/// task is planned for its file), and the run's walk must not open it: the lib's test runs and
/// passes. The planted control is the unit test run_ids_tests.rs
/// `a_features_skipped_targets_bodies_are_not_walked` (and engine_run_answer_tests.rs
/// `a_features_skipped_bin_is_not_walked_over_the_stored_answer`): the skipped word withheld,
/// the walk refuses that body by name.
#[test]
#[cfg(target_os = "macos")]
fn a_test_after_a_check_walks_past_a_features_skipped_bin() {
    fixture("engine_start_skipped_bin", &[("Cargo.toml", SKIPPING), ("src/lib.rs", PASS), ("src/bin/extra.rs", EXTRA_BIN)], |dir, home| {
        let checked = blaze(dir, home, "check", &[]);
        assert_eq!(checked.code, CLEAN_CHECK, "{}", checked.evidence(CLEAN_CHECK_RULE));
        let said = blaze(dir, home, "test", &["--", "--deadline", "20s"]);
        assert!(!said.stderr.contains("the walk reached"), "{}", said.evidence("the skipped bin's body is not walked"));
        assert!(said.stdout.contains("ran 1: 1 passed, 0 failed"), "{}", said.evidence("the lib's test ran"));
        assert_eq!(said.code, Some(0), "{}", said.evidence("a passing suite exits 0"));
    });
}

/// The failing-assert case: `fails` panics in `assert_eq!(1, 2)`, so it is counted failed and the
/// command exits 1. Open item D143: the failing-assert path needs code the builder refuses today.
#[test]
#[cfg(target_os = "macos")]
#[ignore = "D143: the failing assert's code is refused: `NOT RUN fails: core::ptr::read_unaligned (key 5fc05c346c68) [core::core_arch::arm_shared::neon::uint8x16_t] at core/src/ptr/mod.rs:1758:1: ... the const block at expression 1 ...: `offset_of!` (a field's offset from the layout) is not evaluated yet` (t5/cli-start2.txt, 2026-11-19)"]
fn a_failing_test_beside_a_passing_one_exits_one() {
    let lib = format!("{PASS}\n{FAIL}");
    fixture("engine_start_fail", &[("Cargo.toml", LIB), ("src/lib.rs", &lib)], |dir, home| {
        let said = blaze(dir, home, "test", &["--", "--deadline", "20s"]);
        assert!(said.stdout.contains("ran 2: 1 passed, 1 failed"), "{}", said.evidence("both tests ran"));
        assert!(said.stdout.lines().any(|line| line.starts_with("FAILED fails")), "{}", said.evidence("`fails` failed"));
        assert_eq!(said.code, Some(1), "{}", said.evidence("a failing suite exits 1"));
    });
}

/// D191: `broken` is held by the library and its test build, and each checks it under its own
/// view, so the test program's call of the test build's key meets the test build's own row
/// (Errors) and `calls` is NOT RUN with that finding, not for "the body ids hold nothing" (as
/// it was under D152's gap, seen 2026-11-19, rounds/wave-1119/t6/cli-start.txt). Needs the
/// planner's view rows (engine_body_plan.rs) as well as run_ids.rs `workspace_of`'s asks.
#[test]
#[cfg(target_os = "macos")]
fn an_error_no_test_calls_does_not_stop_the_tests() {
    fixture("engine_start_gate", &[("Cargo.toml", LIB), ("src/lib.rs", BROKEN)], |dir, home| {
        let said = blaze(dir, home, "test", &["--", "--deadline", "20s"]);
        // The check printed the error (its summary is out before any test runs).
        assert!(said.stdout.contains("src/lib.rs") && said.stdout.contains("error"), "{}", said.evidence("the check's finding is printed"));
        // The gate is the call: `fine` runs and passes.
        assert!(!said.stdout.lines().any(|line| line.contains("NOT RUN fine") || line.starts_with("FAILED fine")), "{}", said.evidence("`fine` ran"));
        // `calls` is NOT RUN, with the words of `broken`'s check.
        let not_run = said.stdout.lines().find(|line| line.contains("NOT RUN") && line.contains("calls")).unwrap_or_else(|| panic!("{}", said.evidence("`calls` is NOT RUN")));
        assert!(said.stdout.contains("whose check today is Errors") || not_run.contains("Errors"), "{}", said.evidence("`calls` names `broken`'s finding"));
        assert_ne!(said.code, Some(0), "{}", said.evidence("a test NOT RUN fails the command"));
    });
    // Control: `broken` fixed, both run and pass; the NOT RUN above was the error's.
    fixture("engine_start_gate_fixed", &[("Cargo.toml", LIB), ("src/lib.rs", FIXED)], |dir, home| {
        let said = blaze(dir, home, "test", &["--", "--deadline", "20s"]);
        assert!(!said.stdout.contains("NOT RUN"), "{}", said.evidence("nothing is NOT RUN"));
        assert!(said.stdout.contains("ran 2: 2 passed, 0 failed"), "{}", said.evidence("both ran"));
        assert_eq!(said.code, Some(0), "{}", said.evidence("a passing suite exits 0"));
    });
}

/// A workspace whose member `pkg` is in a folder of its own, so the package's root is not where
/// blaze was started.
const WORKSPACE: &str = "[workspace]\nmembers = [\"pkg\"]\nresolver = \"2\"\n";
const PKG: &str = "[package]\nname = \"pkg\"\nversion = \"0.3.1\"\nedition = \"2024\"\n";
/// Reads its package's manifest by a relative path and its package's variables at run time.
const READS_ITS_PACKAGE: &str = "#[test]\nfn runs_in_its_package_root() {\n    let manifest = std::fs::read_to_string(\"Cargo.toml\").unwrap();\n    assert!(manifest.contains(\"name = \\\"pkg\\\"\"));\n    let dir = std::env::var(\"CARGO_MANIFEST_DIR\").unwrap();\n    assert!(dir.ends_with(\"/pkg\"));\n    assert!(std::env::var(\"CARGO_PKG_NAME\").as_deref() == Ok(\"pkg\"));\n    assert!(std::env::var(\"CARGO_PKG_VERSION_MINOR\").as_deref() == Ok(\"3\"));\n}\n";

/// A test runs in its package's root, told its package's variables, as `cargo test` runs it
/// (test_drive.rs `PackageRun`): from the workspace's root, `pkg`'s test reads `Cargo.toml` and
/// finds `pkg`'s manifest, and `CARGO_MANIFEST_DIR`, `CARGO_PKG_NAME` and
/// `CARGO_PKG_VERSION_MINOR` are `pkg`'s. CAUSE: in the folder blaze was started in (the run's
/// `dir: "."` before), `Cargo.toml` is the workspace's, which names no package, and no
/// `CARGO_*` was set, so the asserts fail. The unit test test_drive.rs
/// `a_members_tests_run_in_its_package_root_told_its_package` holds the in-process control.
#[test]
#[cfg(target_os = "macos")]
fn a_test_runs_in_its_package_root_told_its_package() {
    fixture("engine_start_package_root", &[("Cargo.toml", WORKSPACE), ("pkg/Cargo.toml", PKG), ("pkg/src/lib.rs", READS_ITS_PACKAGE)], |dir, home| {
        let said = blaze(dir, home, "test", &["--", "--deadline", "20s"]);
        assert!(!said.stdout.contains("NOT RUN") && !said.stdout.contains("FAILED"), "{}", said.evidence("the test read its package's manifest and variables"));
        assert!(said.stdout.contains("ran 1: 1 passed, 0 failed"), "{}", said.evidence("the test ran and passed"));
        assert_eq!(said.code, Some(0), "{}", said.evidence("a passing suite exits 0"));
    });
}

// --- Each holder unit checks a body under its own view (D191) ---
//
// These need both halves of D191: the planner's view rows (engine_body_plan.rs: every holder
// unit checks a body, the first in rank under the plain name, each other at its view) and the
// run's asks (run_ids.rs `workspace_of`). The planted control of the old rule (the first holder
// checks alone, the base row for every unit) is in-process, where the rule can be swapped:
// run_ids_tests.rs `each_unit_that_holds_a_body_has_its_own_row_at_its_view` and blaze-drive
// view_tests.rs `control_the_first_holder_checking_alone_is_caught`.

/// A library fn called by a `#[cfg(test)] mod tests` test: the test build holds lib.rs too, so
/// the call is through the test build's key. `assert!`, not `assert_eq!`: a passing test, with
/// nothing of D143's failing path.
const CALLED_BY_MOD_TESTS: &str = "pub fn add(a: u8, b: u8) -> u8 {\n    a + b\n}\n\n#[cfg(test)]\nmod tests {\n    use super::*;\n\n    #[test]\n    fn adds() {\n        assert!(add(1, 2) == 3);\n    }\n}\n";

/// (a) `check`, then `test` in a SECOND command, nothing changed: the test's call of the library
/// fn is the test build's key, whose row the walk reads from the stored answer at that unit's
/// view. Under the old rule it was NOT RUN for "the body ids hold nothing".
#[test]
#[cfg(target_os = "macos")]
fn a_lib_fn_called_by_a_mod_tests_test_runs_after_a_check() {
    fixture("engine_start_mod_tests", &[("Cargo.toml", LIB), ("src/lib.rs", CALLED_BY_MOD_TESTS)], |dir, home| {
        let checked = blaze(dir, home, "check", &[]);
        assert_eq!(checked.code, CLEAN_CHECK, "{}", checked.evidence(CLEAN_CHECK_RULE));
        let said = blaze(dir, home, "test", &["--", "--deadline", "20s"]);
        assert!(!said.stdout.contains("hold nothing") && !said.stderr.contains("hold nothing"), "{}", said.evidence("the test build's `add` has a row"));
        assert!(!said.stdout.contains("NOT RUN"), "{}", said.evidence("nothing is NOT RUN"));
        assert!(said.stdout.contains("ran 1: 1 passed, 0 failed"), "{}", said.evidence("`adds` ran and passed"));
        assert_eq!(said.code, Some(0), "{}", said.evidence("a passing suite exits 0"));
    });
}

/// A body whose unit facts differ between the library and its test build: `cfg!(test)` is
/// `true` in the test build only. The bin prints the library's answer.
const CFG_TEST_LIB: &str = "pub fn f() -> u8 {\n    if cfg!(test) { 1 } else { 2 }\n}\n\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn f_is_one_under_test() {\n        assert!(super::f() == 1);\n    }\n}\n";
const CFG_TEST_MAIN: &str = "fn main() {\n    println!(\"{}\", two::f());\n}\n";

/// (b) One body, two answers: the test build's `f` is its own check's (`cfg!(test)` is true:
/// 1) and the library's is the library's (2), in one package. Under the old rule (the base row
/// for the test unit) the test would run the library's `f`, see 2 and fail; the bin's run is the
/// control that the library's row says 2.
#[test]
#[cfg(target_os = "macos")]
fn cfg_test_in_a_body_is_true_in_the_test_build_only() {
    fixture("engine_start_cfg_test", &[("Cargo.toml", LIB), ("src/lib.rs", CFG_TEST_LIB), ("src/main.rs", CFG_TEST_MAIN)], |dir, home| {
        let said = blaze(dir, home, "test", &["--", "--deadline", "20s"]);
        assert!(!said.stdout.contains("NOT RUN") && !said.stdout.contains("FAILED"), "{}", said.evidence("the test build's `f` is 1"));
        assert!(said.stdout.contains("ran 1: 1 passed, 0 failed"), "{}", said.evidence("`f_is_one_under_test` ran and passed"));
        assert_eq!(said.code, Some(0), "{}", said.evidence("a passing suite exits 0"));
        // Control: the library's `f` (the bin's call) is 2.
        let ran = blaze(dir, home, "run", &[]);
        assert_eq!((ran.code, ran.stdout.as_str()), (Some(0), "2\n"), "{}", ran.evidence("the library's `f` outside the test build"));
    });
}

/// A file both the lib and the bin `mod`: two units hold `word`, each checks it.
const SHARED: &str = "pub fn word() -> &'static str {\n    \"shared\"\n}\n";
const SHARED_LIB: &str = "mod shared;\n\npub fn lib_word() -> &'static str {\n    shared::word()\n}\n";
const SHARED_MAIN: &str = "mod shared;\n\nfn main() {\n    println!(\"{}\", shared::word());\n}\n";

/// (c) The bin runs its own row of a file the lib holds too (the bin's `shared::word` is the
/// bin's key: under the old rule only the first holder in rank, the lib, had a row, and the bin
/// stopped on "the body ids hold nothing").
#[test]
#[cfg(target_os = "macos")]
fn a_file_a_lib_and_a_bin_both_mod_runs_in_the_bin() {
    fixture("engine_start_shared_mod", &[("Cargo.toml", LIB), ("src/lib.rs", SHARED_LIB), ("src/main.rs", SHARED_MAIN), ("src/shared.rs", SHARED)], |dir, home| {
        let said = blaze(dir, home, "run", &[]);
        assert!(!said.stderr.contains("hold nothing"), "{}", said.evidence("the bin's `word` has a row"));
        assert_eq!((said.code, said.stdout.as_str()), (Some(0), "shared\n"), "{}", said.evidence("the bin's own row of the shared file"));
    });
}

/// A body with a type error only in the test build (a `#[cfg(test)]` statement), and a bin that
/// calls it.
const TEST_ONLY_ERROR_LIB: &str = "pub fn g() -> u8 {\n    #[cfg(test)]\n    let x: u8 = \"not a number\";\n    #[cfg(not(test))]\n    let x: u8 = 2;\n    x\n}\n";
const TEST_ONLY_ERROR_MAIN: &str = "fn main() {\n    println!(\"{}\", two::g());\n}\n";

/// (d) `check` reports the test build's error, naming the view it is under (the planner's
/// spelling: "(in the test build of `<package>`)" after a finding only the test view has;
/// engine_lines.rs prints it); the program outside the test build (the bin, calling the library's `g`, which is
/// clean) still runs. CONTROL of the error's cause: the library's own row is clean, so the run
/// prints 2.
#[test]
#[cfg(target_os = "macos")]
fn a_type_error_only_under_test_is_reported_and_the_program_still_runs() {
    fixture("engine_start_test_only_error", &[("Cargo.toml", LIB), ("src/lib.rs", TEST_ONLY_ERROR_LIB), ("src/main.rs", TEST_ONLY_ERROR_MAIN)], |dir, home| {
        let checked = blaze(dir, home, "check", &[]);
        assert!(checked.stdout.contains("src/lib.rs") && checked.stdout.contains("error"), "{}", checked.evidence("the test build's error is printed"));
        assert!(checked.stdout.contains("in the test build of `two`"), "{}", checked.evidence("the finding names the view it is under"));
        let ran = blaze(dir, home, "run", &[]);
        assert_eq!((ran.code, ran.stdout.as_str()), (Some(0), "2\n"), "{}", ran.evidence("the non-test program runs"));
    });
}

/// A method of a plain `impl`, called by a `#[cfg(test)] mod tests` test and by the bin.
const METHOD_LIB: &str = "pub struct S;\n\nimpl S {\n    pub fn name(&self) -> &'static str {\n        \"s\"\n    }\n}\n\npub fn free() -> u8 {\n    7\n}\n\n#[cfg(test)]\nmod tests {\n    use super::*;\n\n    #[test]\n    fn t() {\n        assert!(free() == 7);\n        assert!(S.name() == \"s\");\n    }\n}\n";
const METHOD_MAIN: &str = "fn main() {\n    println!(\"{}\", two::S.name());\n}\n";

/// tempo-hardfork's shape (crates/hardfork/src/lib.rs, `tempo_hardfork!` at :56, called at
/// :182): the struct, its impl and the test module all written by one `macro_rules!` call.
const MACRO_METHOD_LIB: &str = "macro_rules! make {\n    () => {\n        pub struct S;\n        impl S {\n            pub fn name(&self) -> &'static str {\n                \"s\"\n            }\n        }\n        #[cfg(test)]\n        mod tests {\n            use super::*;\n            #[test]\n            fn t() {\n                assert!(S.name() == \"s\");\n            }\n        }\n    };\n}\n\nmake!();\n";

/// (e) A METHOD runs, in the test build (its view row) and in the bin (its base row). THE CAUSE
/// it guards (tempo-hardfork: 30 of 35 tests NOT RUN, every one a call of an impl member:
/// `<impl TempoHardfork>::variant_index`, `is_t7`, `name`, the derived `eq`; tempo-dkg's one
/// test on `<impl OnchainDkgOutcome>::dealt`): a body's run facts listed each covered item as its
/// library key's first 16 bytes (front2 keys.rs `checker_of`), while a program calls a fn of an
/// impl or a trait by its MEMBER key (keys.rs `member_key`, filed by the impl's head, heads.rs
/// `Keys::member`), so the walk found no row for any method: "the body ids hold nothing for
/// ... none this fn's". The covers are now each item as a program asks it (keys.rs `asked_key`;
/// engine_bodies.rs `asked_keys`). CONTROL in the same test: `free()` (a free fn, whose two
/// keys agree) ran before the fix too; only the method's route changed.
#[test]
#[cfg(target_os = "macos")]
fn a_method_called_by_a_test_and_by_the_bin_runs_after_a_check() {
    fixture("engine_start_method", &[("Cargo.toml", LIB), ("src/lib.rs", METHOD_LIB), ("src/main.rs", METHOD_MAIN)], |dir, home| {
        let checked = blaze(dir, home, "check", &[]);
        assert_eq!(checked.code, CLEAN_CHECK, "{}", checked.evidence(CLEAN_CHECK_RULE));
        let said = blaze(dir, home, "test", &["--", "--deadline", "20s"]);
        assert!(!said.stdout.contains("hold nothing") && !said.stderr.contains("hold nothing"), "{}", said.evidence("the test build's `<impl S>::name` has a row: its covers hold its member key"));
        assert!(!said.stdout.contains("NOT RUN"), "{}", said.evidence("nothing is NOT RUN"));
        assert!(said.stdout.contains("ran 1: 1 passed, 0 failed"), "{}", said.evidence("`t` ran and passed"));
        assert_eq!(said.code, Some(0), "{}", said.evidence("a passing suite exits 0"));
        // The base row: the library's `name`, called by the bin.
        let ran = blaze(dir, home, "run", &[]);
        assert!(!ran.stderr.contains("hold nothing"), "{}", ran.evidence("the library's `<impl S>::name` has a row"));
        assert_eq!((ran.code, ran.stdout.as_str()), (Some(0), "s\n"), "{}", ran.evidence("the bin calls the library's method"));
    });
}

/// (f) The tempo-hardfork reduction: struct, impl and `mod tests` all from one `macro_rules!`
/// call. `check`, then `test` in a second command: "ran 1: 1 passed".
#[test]
#[cfg(target_os = "macos")]
fn a_method_a_macro_rules_call_wrote_runs_in_the_test_its_call_wrote() {
    fixture("engine_start_macro_method", &[("Cargo.toml", LIB), ("src/lib.rs", MACRO_METHOD_LIB)], |dir, home| {
        let checked = blaze(dir, home, "check", &[]);
        assert_eq!(checked.code, CLEAN_CHECK, "{}", checked.evidence(CLEAN_CHECK_RULE));
        let said = blaze(dir, home, "test", &["--", "--deadline", "20s"]);
        assert!(!said.stdout.contains("hold nothing") && !said.stderr.contains("hold nothing"), "{}", said.evidence("the method `make!` wrote has a row at the test build's view"));
        assert!(!said.stdout.contains("NOT RUN"), "{}", said.evidence("nothing is NOT RUN"));
        assert!(said.stdout.contains("ran 1: 1 passed, 0 failed"), "{}", said.evidence("`t` ran and passed"));
        assert_eq!(said.code, Some(0), "{}", said.evidence("a passing suite exits 0"));
    });
}

/// A tuple struct's and a tuple variant's constructors, each passed as a fn value
/// (`Option::map`) and called directly, from the library's `wrapped`, a `mod tests` test and
/// the bin: 1 + 3 + 4 + 5 = 13 in the library.
const CTOR_LIB: &str = "pub struct Wrap(pub u8);\n\npub enum E {\n    V(u8),\n}\n\npub fn wrapped() -> u8 {\n    let mapped = Some(1u8).map(Wrap);\n    let variant = Some(3u8).map(E::V);\n    let direct = Wrap(4);\n    let called = E::V(5);\n    let a = match mapped {\n        Some(w) => w.0,\n        None => 0,\n    };\n    let b = match variant {\n        Some(E::V(n)) => n,\n        None => 0,\n    };\n    let E::V(d) = called;\n    a + b + direct.0 + d\n}\n\n#[cfg(test)]\nmod tests {\n    use super::*;\n\n    #[test]\n    fn ctors() {\n        assert!(wrapped() == 13);\n        let w = Some(7u8).map(Wrap);\n        assert!(matches!(w, Some(Wrap(7))));\n        assert!(matches!(Some(8u8).map(E::V), Some(E::V(8))));\n    }\n}\n";
/// The bin's own calls of the library's constructors: `Wrap(2)` and `.map(E::V)` of 6.
const CTOR_MAIN: &str = "fn main() {\n    let direct = two::Wrap(2);\n    let mapped = Some(6u8).map(two::E::V);\n    let n = match mapped {\n        Some(two::E::V(n)) => n,\n        None => 0,\n    };\n    println!(\"{} {}\", two::wrapped(), direct.0 + n);\n}\n";

/// (g) A CONSTRUCTOR RUNS after a check. THE CAUSE it guards: a constructor has no body and no
/// `BodyCheck`, so no check's covers held its key and every call of one stopped at the gate
/// ("the body ids hold nothing for ... none this fn's"). It is now gated under its ADT's head
/// task: the task's record ends in run facts of the body form whose covers are each holding
/// unit's constructor keys (engine_heads.rs `ctor_facts`), and the walk lists the files'
/// `Signature` parts as checks (run_ids.rs `workspace_of`). `check`, then `test` in a second
/// command (the test build's constructors, its own unit's keys), then `run` (the library's,
/// called by the bin). CONTROL of the cause: run_ids_tests.rs, the walk with no `Signature`
/// part, whose constructor has no row.
#[test]
#[cfg(target_os = "macos")]
fn a_tuple_struct_and_a_tuple_variant_constructor_run_after_a_check() {
    fixture("engine_start_ctors", &[("Cargo.toml", LIB), ("src/lib.rs", CTOR_LIB), ("src/main.rs", CTOR_MAIN)], |dir, home| {
        let checked = blaze(dir, home, "check", &[]);
        assert_eq!(checked.code, CLEAN_CHECK, "{}", checked.evidence(CLEAN_CHECK_RULE));
        let said = blaze(dir, home, "test", &["--", "--deadline", "20s"]);
        assert!(!said.stdout.contains("hold nothing") && !said.stderr.contains("hold nothing"), "{}", said.evidence("the test build's `Wrap` and `E::V` have rows: their ADTs' head tasks cover them"));
        assert!(!said.stdout.contains("NOT RUN"), "{}", said.evidence("nothing is NOT RUN"));
        assert!(said.stdout.contains("ran 1: 1 passed, 0 failed"), "{}", said.evidence("`ctors` ran and passed"));
        assert_eq!(said.code, Some(0), "{}", said.evidence("a passing suite exits 0"));
        let ran = blaze(dir, home, "run", &[]);
        assert!(!ran.stderr.contains("hold nothing"), "{}", ran.evidence("the library's constructors have rows"));
        assert_eq!((ran.code, ran.stdout.as_str()), (Some(0), "13 8\n"), "{}", ran.evidence("the bin calls the library's constructors, directly and as fn values"));
    });
}

/// An ADT whose head has an error (its field's type names nothing), its constructor taken as a
/// fn value by one test; another test touches nothing of it.
const BROKEN_CTOR_LIB: &str = "pub struct Wrap(pub Missing);\n\n#[test]\nfn uses() {\n    let make = Wrap;\n    let _ = make;\n}\n\n#[test]\nfn fine() {}\n";

/// (g') TWIN: a constructor of an ADT whose head did not freeze is NOT RUN, and what stops it
/// names the ADT (its head's finding: engine_head_notes.rs; the head task's run facts say
/// `Untyped`, `TYPES_NOT_CHECKED`, for its constructors: engine_heads.rs `ctor_facts`). The test
/// that does not touch it runs: the error stops only what reads it.
#[test]
#[cfg(target_os = "macos")]
fn a_constructor_of_an_adt_whose_head_has_an_error_is_not_run() {
    fixture("engine_start_broken_ctor", &[("Cargo.toml", LIB), ("src/lib.rs", BROKEN_CTOR_LIB)], |dir, home| {
        let said = blaze(dir, home, "test", &["--", "--deadline", "20s"]);
        let not_run = said.stdout.lines().find(|line| line.contains("NOT RUN") && line.contains("uses")).unwrap_or_else(|| panic!("{}", said.evidence("`uses` is NOT RUN")));
        assert!(
            not_run.contains("Wrap") || said.stdout.lines().any(|line| line.contains("Wrap") && line.contains("head")),
            "{}",
            said.evidence("what stops `uses` names the ADT `Wrap` whose head did not freeze")
        );
        assert!(!said.stdout.lines().any(|line| line.contains("NOT RUN fine") || line.starts_with("FAILED fine")), "{}", said.evidence("`fine` ran"));
        assert_ne!(said.code, Some(0), "{}", said.evidence("a test NOT RUN fails the command"));
    });
}

/// A blaze-edition binary's root `fn main` is D104's implicit `#[::std::main]` input (the whole
/// item, including its body), whose expansion is stored in `CrateScope`. Editing ONLY inside
/// `fn main`'s body leaves `Outline::surface()` unchanged; `CrateScope` must record a read of
/// `definition_cell` for the root's top-level `fn main` (engine_names.rs `finish`, rule 21) and
/// include expansion record keys in `CrateScope`'s `out` (names.rs `OUT_DOMAIN` 3), so both a
/// warm run and a run after deleting `.blaze/v2` while keeping the home store print the edited
/// output instead of the stale cached `"v1"` expansion.
#[test]
#[cfg(target_os = "macos")]
fn editing_blaze_edition_main_body_rebuilds_warm_and_after_checkout_clean() {
    let main_for = |word: &str| format!("fn main(cx: &Cx, out: Out) -> Result<(), Error> {{\n    out.line(cx, \"{word}\")?;\n    Ok(())\n}}\n");
    fixture("engine_start_blaze_edit_main", &[("Cargo.toml", BLAZE_BIN), ("src/main.rs", &main_for("v1"))], |dir, home| {
        let first = blaze(dir, home, "run", &[]);
        assert_eq!((first.code, first.stdout.as_str()), (Some(0), "v1\n"), "{}", first.evidence("initial blaze-2026.11 run prints v1"));

        // Warm edit: ONLY inside `fn main`'s body.
        write(dir, "src/main.rs", &main_for("v2"));
        let warm = blaze(dir, home, "run", &[]);
        assert_eq!((warm.code, warm.stdout.as_str()), (Some(0), "v2\n"), "{}", warm.evidence("warm edit inside fn main prints v2"));

        // Edit again and run `blaze clean` (removes `.blaze/v2` while keeping `home` store).
        write(dir, "src/main.rs", &main_for("v3"));
        let cleaned = blaze(dir, home, "clean", &[]);
        assert_eq!(cleaned.code, Some(0), "{}", cleaned.evidence("blaze clean removes .blaze/v2"));
        let after_clean = blaze(dir, home, "run", &[]);
        assert_eq!((after_clean.code, after_clean.stdout.as_str()), (Some(0), "v3\n"), "{}", after_clean.evidence("edit + remove .blaze/v2 prints v3"));
    });
}

/// Fixing a syntax error in a struct (`pub gen: u32,` -> `pub generation: u32,`) must immediately
/// invalidate every `BodyCheck` that previously failed on a head depending on that struct, even
/// when the functions' own bodies are untouched (rule 21: engine_bodies.rs places failed head
/// cells via `owners.place` instead of recording an unplaced `head_cell_id`). Tested both warm
/// and after removing `.blaze/v2` while keeping the `home` store.
#[test]
#[cfg(target_os = "macos")]
fn fixing_syntax_error_in_struct_invalidates_stale_failed_bodies() {
    let source = |field: &str| {
        format!(
            "pub struct EntityRec {{\n    pub {field}: u32,\n}}\n\n\
             fn use_rec(_e: &EntityRec) -> u32 {{\n    0\n}}\n\n\
             fn direct() -> u32 {{\n    let e = EntityRec {{ generation: 1 }};\n    e.generation\n}}\n\n\
             fn main(cx: &Cx, out: Out) -> Result<(), Error> {{\n    let e = EntityRec {{ generation: 1 }};\n    let _ = use_rec(&e) + direct();\n    out.line(cx, \"fixed\")?;\n    Ok(())\n}}\n"
        )
    };
    fixture("engine_start_fix_struct_syntax", &[("Cargo.toml", BLAZE_BIN), ("src/main.rs", &source("gen"))], |dir, home| {
        let broken = blaze(dir, home, "check", &[]);
        assert_ne!(broken.code, Some(0), "{}", broken.evidence("syntax error in EntityRec fails check"));
        assert!(broken.stdout.contains("could not be frozen") || broken.stderr.contains("could not be frozen"), "{}", broken.evidence("bodies record head freeze failure"));

        // Warm repair: ONLY fix the struct field name; leave `use_rec`, `direct`, and `main` untouched.
        write(dir, "src/main.rs", &source("generation"));
        let warm = blaze(dir, home, "run", &[]);
        assert!(!warm.stderr.contains("could not be frozen"), "{}", warm.evidence("warm repair clears stale head failure"));
        assert!(!warm.stderr.contains("pub gen"), "{}", warm.evidence("warm repair does not cite old syntax error"));
        assert_eq!((warm.code, warm.stdout.as_str()), (Some(0), "fixed\n"), "{}", warm.evidence("warm repair runs and prints fixed"));

        // Re-introduce the syntax error, check, repair again, remove `.blaze/v2` while keeping `home` store, and run.
        write(dir, "src/main.rs", &source("gen"));
        let broken2 = blaze(dir, home, "check", &[]);
        assert_ne!(broken2.code, Some(0), "{}", broken2.evidence("second broken check fails"));
        write(dir, "src/main.rs", &source("generation"));
        let cleaned = blaze(dir, home, "clean", &[]);
        assert_eq!(cleaned.code, Some(0), "{}", cleaned.evidence("blaze clean removes .blaze/v2"));
        let after_clean = blaze(dir, home, "run", &[]);
        assert!(!after_clean.stderr.contains("could not be frozen"), "{}", after_clean.evidence("after-clean repair clears stale head failure"));
        assert!(!after_clean.stderr.contains("pub gen"), "{}", after_clean.evidence("after-clean repair does not cite old syntax error"));
        assert_eq!((after_clean.code, after_clean.stdout.as_str()), (Some(0), "fixed\n"), "{}", after_clean.evidence("after-clean repair runs and prints fixed"));
    });
}
