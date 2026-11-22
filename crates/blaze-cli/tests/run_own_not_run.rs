//! `blaze run` of a Rust-edition program that the run parent cannot start exits 125
//! (`program::NOT_RUN`, decisions D86 and D103, the ladder's L6), through the new path: the
//! program is never run inside blaze's process (LADDER.md L1b, `RunError::OwnProcess`), the
//! parent lays its code records out under `<HOME>/.blaze/v2/run/records`
//! (crates/blaze-cli/src/run_parent.rs `RECORDS`, own_process.rs `run_own`), and here a FILE
//! stands where that folder's parent should be, so the records cannot be laid out: a failure of
//! the parent's own work before the program's first instruction.
//!
//! Rust's rule for why 125: a program's own status is its `main`'s (the Reference, "Crates and
//! source files"; std `rt.rs` `lang_start_internal`, `process.rs` `Termination`): 0, 1 and 101
//! are statuses a program reports, and 125 is none a Rust start gives on its own; `env`,
//! `timeout` and container runners use it for the tool's own failure.
//!
//! Before this wiring the program `fn main() {}` runs in blaze's own process and `blaze run`
//! exits 0 (or, once L1b alone is in, with `RunError::OwnProcess` through `CliError::Ran`): the
//! assertion on the records folder's words fails, naming the status and standard error.
//!
//! COST: a whole `blaze run` of a std program (RESTART-1545.md point 6: about 27 GB and 40 s of
//! CPU today); run it alone on the machine.
//!
//! Expected (`--test run_own_not_run`): "test blaze-cli: exit 0; 1 passed, 0 failed".

mod support;

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use support::fresh_home;

/// blaze's status for "the program did not run" (crates/blaze-cli/src/program.rs `NOT_RUN`).
const NOT_RUN: i32 = 125;

/// How long the refused `blaze run` may take: its check and the in-process front end's build
/// of a std hello world come first (about 40 s of CPU today); a hang is a bug (rule 18), named,
/// never waited out.
const DEADLINE: Duration = Duration::from_secs(300);

/// How long the child killed at [`DEADLINE`] may take to be reaped (the kill is SIGKILL).
const REAP: Duration = Duration::from_secs(10);

/// A Rust-edition binary: its start is `lang_start` through the C entry shim.
const MANIFEST: &str = "[package]\nname = \"own-not-run\"\nversion = \"0.1.0\"\nedition = \"2024\"\n";

/// The program: it would exit 0.
const MAIN: &str = "fn main() {}\n";

/// `blaze run` of a Rust-edition program whose records folder cannot be made exits [`NOT_RUN`],
/// names the records folder on standard error, and writes nothing on standard output.
///
/// The child cannot outlive this test: it is waited for in a loop bounded by [`DEADLINE`]; at
/// the deadline it is killed and reaped within [`REAP`] before the test fails by name.
#[test]
fn run_own_not_run_a_blocked_records_folder_exits_125() {
    let base = fresh_home("run_own_not_run", true);
    let home = base.join("home");
    // A file where the run's folder should be: `<home>/.blaze/v2/run/records` cannot be made.
    std::fs::write(home.join(".blaze/v2/run"), "not a folder").expect("the blocking file");
    let dir = base.join("checkout");
    std::fs::create_dir_all(dir.join("src")).expect("the checkout's folders");
    std::fs::write(dir.join("Cargo.toml"), MANIFEST).expect("the manifest");
    std::fs::write(dir.join("src/main.rs"), MAIN).expect("the program");
    let mut child = Command::new(env!("CARGO_BIN_EXE_blaze"))
        .arg("run")
        .current_dir(&dir)
        .env("HOME", &home)
        .env("CARGO_HOME", home.join(".cargo"))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("blaze starts");
    let started = Instant::now();
    // Waiting for: the refused `blaze run`'s exit, at most DEADLINE.
    let mut hung = false;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if started.elapsed() > DEADLINE => {
                hung = true;
                break;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("waiting for `blaze run` with a blocked records folder: {error}");
            }
        }
    }
    if hung {
        let _ = child.kill();
        let killed = Instant::now();
        // Waiting for: the killed child's reaping, at most REAP.
        while child.try_wait().ok().flatten().is_none() {
            assert!(killed.elapsed() <= REAP, "`blaze run` with a blocked records folder: not reaped {REAP:?} after SIGKILL");
            std::thread::sleep(Duration::from_millis(5));
        }
        let _ = std::fs::remove_dir_all(&base);
        panic!("`blaze run` with a blocked records folder gave no answer in {DEADLINE:?}; it was killed");
    }
    let output = child.wait_with_output().expect("blaze's output");
    let _ = std::fs::remove_dir_all(&base);
    let said = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(NOT_RUN), "status {:?}; standard error:\n{said}", output.status.code());
    assert!(
        // The run parent's own words (run_parent.rs `ParentError`: the records folder refused
        // as it is opened, or the first record's write under it, by where bstd `Fs::home`
        // finds the file in the way).
        said.lines().any(|line| line.starts_with("error: ") && line.contains("`blaze run` did not start or serve the program (run.md piece 6, D124)")),
        "an `error: ` line says the run parent did not start the program:\n{said}"
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout), "", "standard output is the program's alone, and nothing ran");
}
