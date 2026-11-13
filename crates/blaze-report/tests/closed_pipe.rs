//! A command whose reader closes the pipe early ends quietly with the status of a program
//! ended by SIGPIPE, on a real pipe: the fact needs the kernel (a write to a pipe with no
//! reader), which no simulation of a stream gives. The test runs itself as the child.

use std::{
    io::{BufRead as _, BufReader},
    process::{Command, Stdio},
};

/// What the child does, said by the parent in the environment.
const ROLE: &str = "BLAZE_REPORT_CLOSED_PIPE_CHILD";

/// The child: lines to stdout until its reader is gone. Does nothing when run as a test.
#[test]
fn child_says_lines() {
    match std::env::var(ROLE).as_deref() {
        Ok("out") => loop {
            blaze_report::outln!("a line for a reader that may be gone");
        },
        // The control's child: std's macro, which panics.
        Ok("println") => loop {
            println!("a line for a reader that may be gone");
        },
        _ => {}
    }
}

/// The child's exit status and what it wrote to stderr, after its reader took one line of its
/// stdout and closed.
fn ended(role: &str) -> (Option<i32>, String) {
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "child_says_lines", "--nocapture", "--test-threads=1"])
        .env(ROLE, role)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map(stdx::JodChild)
        .unwrap();
    let mut out = BufReader::new(child.stdout.take().unwrap());
    // Lines until the child's own: libtest writes `running 1 test` first.
    let mut line = String::new();
    while !line.starts_with("a line") {
        line.clear();
        assert!(out.read_line(&mut line).unwrap() > 0, "the child said nothing");
    }
    drop(out);
    let done = stdx::child::output_within(
        child.into_inner(),
        "the child whose stdout reader closed",
        std::time::Duration::from_secs(60),
    );
    (done.status.code(), String::from_utf8_lossy(&done.stderr).into_owned())
}

/// The invariant: a command that writes through `blaze_report::out` to a pipe whose reader
/// closed ends with status 141 and says nothing on stderr.
#[cfg(unix)]
#[test]
fn a_reader_that_closes_early_ends_the_command_quietly() {
    let (code, err) = ended("out");
    assert_eq!(code, Some(blaze_report::out::CLOSED), "{err}");
    assert!(!err.contains("panicked") && !err.contains("Broken pipe"), "{err}");
}

/// The control: std's `println!` in the same place panics, which is what the writer replaces.
#[cfg(unix)]
#[test]
fn the_oracle_catches_a_print_that_panics_on_a_closed_pipe() {
    let (code, err) = ended("println");
    assert_ne!(code, Some(blaze_report::out::CLOSED), "{err}");
    assert!(err.contains("failed printing to stdout"), "{err}");
}
