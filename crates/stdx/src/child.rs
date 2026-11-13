//! A wait for a child process to a deadline: how a test waits for the program it runs (rule
//! `bounded-progress`, `blaze::unbounded_test_wait` in docs/lints.md). `Child::wait` has no
//! deadline, so a child that hangs holds its test, and the whole run, for as long as nobody
//! looks; a child that has not exited by the deadline is killed here instead, and the caller's
//! test fails naming what hung.
//!
//! The wait itself is `blaze_core::wait`'s: on Linux `exited`, one wait on the child's exit to
//! the deadline; elsewhere a `try_wait` between sleeps of at most [`STEP`] to the deadline.

use std::{
    io::Read,
    process::{Child, ExitStatus, Output},
    time::Duration,
};

use blaze_core::wait::{Clock as _, Deadline, Monotonic};

/// The longest sleep between two looks at a child that has not exited, where the system has
/// no wait on a child's exit to a deadline.
#[cfg(not(target_os = "linux"))]
const STEP: Duration = Duration::from_millis(2);

/// Waits for `child` to exit, at most `within`: its status. A child still running then is
/// killed and reaped, and this panics naming `what` and how long it waited.
pub fn exit_within(child: &mut Child, what: &'static str, within: Duration) -> ExitStatus {
    let clock = Monotonic::new();
    let deadline = Deadline::after(clock.now(), within);
    match child.try_wait() {
        Ok(Some(status)) => return status,
        Ok(None) => {}
        Err(error) => panic!("looking at {what}: {error}"),
    }
    if exited_by(child, what, deadline, &clock) {
        // It has exited: this reaps it, or reads the status `try_wait` kept.
        return child.wait().unwrap_or_else(|error| panic!("reaping {what}: {error}"));
    }
    // A killed child ends, so its reap is bounded.
    _ = child.kill();
    _ = child.wait();
    panic!("{what} did not exit within {within:?}, and was killed: a hang")
}

/// [`exit_within`] for a child whose output is piped: its status with all it wrote to its
/// piped stdout and stderr, each read on its own thread while it runs so that a full pipe
/// cannot stop it.
pub fn output_within(mut child: Child, what: &'static str, within: Duration) -> Output {
    let stdout = child.stdout.take().map(|pipe| drain(pipe, "stdx.output_within.stdout", what));
    let stderr = child.stderr.take().map(|pipe| drain(pipe, "stdx.output_within.stderr", what));
    let status = exit_within(&mut child, what, within);
    // The child has exited and closed its ends, so each read has ended or ends at once.
    let read = |reader: Option<std::thread::JoinHandle<Vec<u8>>>| match reader {
        Some(it) => it.join().unwrap_or_else(|_| panic!("reading the output of {what} panicked")),
        None => Vec::new(),
    };
    Output { status, stdout: read(stdout), stderr: read(stderr) }
}

/// Reads all of `pipe` on a thread named `name`.
fn drain(
    mut pipe: impl Read + Send + 'static,
    name: &'static str,
    what: &'static str,
) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::Builder::new()
        .name(name.to_owned())
        .spawn(move || {
            let mut out = Vec::new(); // alloc: a child's output, handed to the caller
            if let Err(error) = pipe.read_to_end(&mut out) {
                panic!("reading the output of {what}: {error}");
            }
            out
        })
        .unwrap_or_else(|error| panic!("starting the reader of {what}'s output: {error}"))
}

/// Whether `child` exited by `deadline` on `clock`: one wait on its exit, leaving it
/// unreaped for `Child::wait`.
#[cfg(target_os = "linux")]
fn exited_by(child: &mut Child, what: &'static str, deadline: Deadline, clock: &Monotonic) -> bool {
    use blaze_core::wait::Waited;
    let pid = libc::pid_t::try_from(child.id()).expect("a child's pid fits a pid_t");
    match blaze_core::wait::exited(what, pid, Some(deadline), clock) {
        Ok(Waited::Ready(())) => true,
        Ok(Waited::Deadline) => false,
        Err(error) => panic!("{error}"),
    }
}

/// Whether `child` exited by `deadline` on `clock`: a `try_wait` between sleeps of at most
/// [`STEP`], until it has or the deadline has passed.
#[cfg(not(target_os = "linux"))]
fn exited_by(child: &mut Child, what: &'static str, deadline: Deadline, clock: &Monotonic) -> bool {
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return true,
            Ok(None) => {}
            Err(error) => panic!("looking at {what}: {error}"),
        }
        let now = clock.now();
        if now >= deadline.get() {
            return false;
        }
        let next = Deadline::at((now + STEP).min(deadline.get()));
        if let Err(error) = blaze_core::wait::sleep_until(what, next, clock) {
            panic!("{error}");
        }
    }
}
