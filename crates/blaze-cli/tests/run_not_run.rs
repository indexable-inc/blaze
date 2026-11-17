//! `blaze run` that cannot start its program exits 125 (`program::NOT_RUN`, decision D86 and
//! the ladder's L6), never a status the program itself could have ended with.
//!
//! Rust's rule: a binary crate's status is its `main`'s. The Reference ("Crates and source
//! files", the main function): "All crates which contain a `main` function are executed ...
//! the `main` function ... may return a type implementing `Termination`", and the library's
//! own `std/src/rt.rs` (`lang_start_internal` returns `main`'s `report().to_i32()`) and
//! `std/src/process.rs` (`impl Termination for ()` is `ExitCode::SUCCESS`, `for Result<T, E>`
//! an `Err` is `ExitCode::FAILURE`, 1; a panic leaving `main` is 101). So 0, 1 and 101 are
//! all statuses a program reports: a tool that also says 1 for "I could not build it" cannot
//! be told from a program that failed. 125 is no status a Rust program's start gives on its
//! own, and is what `env`, `timeout` and container runners use for the tool's own failure.
//!
//! The case: a package whose manifest names `src/main.rs` as its binary's root, and no such
//! file. Nothing can be built, so nothing runs: the status is 125, standard output (the
//! program's alone) is empty, and standard error carries blaze's `error: ` line. Before L6's
//! wiring this exits 1 (an error before the program left through bstd's exit path, or the
//! missing root's finding through `close`'s `Status::FAILED`): the status assertion fails
//! with "status Some(1)".
//!
//! Expected (filter run_not_run): "test blaze-cli: exit 0; 1 passed, 0 failed".

mod support;

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use support::fresh_home;

/// blaze's status for "the program did not run" (crates/blaze-cli/src/program.rs `NOT_RUN`).
const NOT_RUN: i32 = 125;

/// How long the refused `blaze run` may take: it stops at the manifest's missing root, before
/// any body is built; a hang is a bug (rule 18), named, never waited out.
const DEADLINE: Duration = Duration::from_secs(120);

/// How long the child killed at [`DEADLINE`] may take to be reaped (the kill is SIGKILL).
const REAP: Duration = Duration::from_secs(10);

/// A binary target whose root file is not there.
const MANIFEST: &str = "[package]\nname = \"absent-main\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[[bin]]\nname = \"absent-main\"\npath = \"src/main.rs\"\n";

/// `blaze run` on a package whose `src/main.rs` is missing exits [`NOT_RUN`], says why on
/// standard error, and writes nothing on standard output.
///
/// The child cannot outlive this test: it is waited for in a loop bounded by [`DEADLINE`]; at
/// the deadline it is killed and reaped within [`REAP`] before the test fails by name; every
/// panic below the spawn comes after the child was reaped.
#[test]
fn run_not_run_a_missing_main_exits_125() {
    let base = fresh_home("run_not_run", true);
    let home = base.join("home");
    let dir = base.join("checkout");
    std::fs::create_dir_all(&dir).expect("the checkout's folder");
    std::fs::write(dir.join("Cargo.toml"), MANIFEST).expect("the manifest");
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
            Ok(None) => std::thread::sleep(Duration::from_millis(5)),
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("waiting for `blaze run` on a missing src/main.rs: {error}");
            }
        }
    }
    if hung {
        let _ = child.kill();
        let killed = Instant::now();
        // Waiting for: the killed child's reaping, at most REAP.
        while child.try_wait().ok().flatten().is_none() {
            assert!(killed.elapsed() <= REAP, "`blaze run` on a missing src/main.rs: not reaped {REAP:?} after SIGKILL");
            std::thread::sleep(Duration::from_millis(5));
        }
        let _ = std::fs::remove_dir_all(&base);
        panic!("`blaze run` on a missing src/main.rs gave no answer in {DEADLINE:?}; it was killed");
    }
    let output = child.wait_with_output().expect("blaze's output");
    let _ = std::fs::remove_dir_all(&base);
    let said = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(NOT_RUN), "status {:?}; standard error:\n{said}", output.status.code());
    assert!(said.lines().any(|line| line.starts_with("error: ")), "an `error: ` line names why nothing ran:\n{said}");
    assert_eq!(String::from_utf8_lossy(&output.stdout), "", "standard output is the program's alone, and nothing ran");
}
