//! `#[bstd::main]` on a `main` that takes nothing (D104: every blaze `main` goes through
//! `std::main`). Its own test target: each `#[bstd::main]` exports the hosted start
//! `bstd_hosted_start`, so one test crate holds one.
//!
//! The `main` it makes ends the process when the body returns (rt.rs `run_io` exits), so it is
//! run in a child: this test binary again, asked for the one test that calls it.

#![feature(macro_attr)]

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use bstd::entry::{Error, Status};

/// A `main` that takes nothing.
mod bare {
    use super::Error;

    #[bstd::main]
    pub fn main() -> Result<(), Error> {
        Err(Error::new("bare main ran"))
    }
}

/// Set in the child: only then does [`the_bare_main_in_a_child`] call `main`.
const CHILD: &str = "BSTD_ATTRIBUTES_BARE_CHILD";

/// How long the child may take: it runs one body that returns at once.
const CHILD_MOST: Duration = Duration::from_secs(60);

/// The child's side: the process's `main` that `#[bstd::main]` made, which never returns.
#[test]
fn the_bare_main_in_a_child() {
    if std::env::var_os(CHILD).is_none() {
        return;
    }
    let _ = bare::main();
    panic!("the launcher returned instead of ending the process");
}

/// The no-parameter arm: the launcher runs the written body, and its error is the run's
/// failure (status `FAILED`, the error's words on standard error). A macro that dropped the
/// body would end OK with nothing said, and fail this.
#[test]
fn a_main_with_no_parameter_runs_through_the_launcher() {
    let exe = std::env::current_exe().expect("this test binary's path");
    let mut child = Command::new(&exe)
        .args(["--exact", "the_bare_main_in_a_child", "--nocapture", "--test-threads=1"])
        .env(CHILD, "1")
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|e| panic!("starting {} as the child: {e}", exe.display()));
    let start = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().expect("the child's status") {
            break status;
        }
        if start.elapsed() > CHILD_MOST {
            let _ = child.kill();
            panic!("the bare main's child ran past {CHILD_MOST:?}: a hang");
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let mut said = String::new();
    std::io::Read::read_to_string(&mut child.stderr.take().expect("the child's stderr was piped"), &mut said).expect("the child's stderr reads");
    assert_eq!(status.code(), Some(i32::from(Status::FAILED.0)), "stderr: {said}");
    assert!(said.contains("bare main ran"), "the body's error is the run's words: {said}");
}
