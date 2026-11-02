//! Tests (docs/v2/plan/lanes/usercode.md section 6.5; #1443, #227, #1330): run only the tests an edit
//! reached, each in a process of its own forked from its binary, on one queue over all cores.
//!
//! - [`select`]: whether a test runs. Each test's reached functions are recorded by code hash
//!   ([`crate::Reached`], from codegen's walk); a pass is kept in the store by the test's
//!   binary-independent identity and what it read at run time, so a test whose code and inputs
//!   are as they were when it passed is reported unreached, in every view and worktree at the
//!   same code. [`select::Why`] says why one runs (`--explain`).
//! - [`wire`]: the fork server's protocol, and [`fork_server`] its harness side, which blaze's
//!   generated test harness includes.
//! - [`queue`]: the sans-IO queue over every binary's fork server: critical first, timeouts,
//!   retries for flaky tests, fail-fast, output, reads and the execution record per test; or a
//!   binary run whole as libtest's threads for a package that opts out.
//!
//! A request's tests go: [`select::wanted`] and [`select::select`] for each test, a
//! [`queue::Queue`] of what runs, and for each [`queue::Finished`] what it read
//! ([`queue::Trace::traced`]) and [`select::record`]; [`report`] gives each result as the agent
//! API reports it. [`queue::Finished::executed`] is what blaze-testsel keeps a pass by
//! (`Record::of_hits`, `Record::incomplete`); once cargo selects through it
//! (`blaze_testsel::select::{choose, keep}`), [`select::select`], [`select::record`] and
//! [`select::Test::identity`] retire.
//!
//! The costs and the fastest known way of each part: docs/v2/plan/lanes/usercode/tests.md.

pub mod queue;
pub mod select;
#[allow(dead_code)]
pub mod wire;

use std::{io::Write as _, time::Duration};

pub use queue::{Binary, Config, Failure, Finished, Job, Lines, Loaded, Mode, OUTSIDE, Outcome, Queue, Trace};
pub use select::{
    Attrs, Decision, Filter, Ignored, Input, Passes, ShouldPanic, Suite, Test, Wanted, Why, changed_input,
};
pub use wire::Executed;

/// The harness side of the fork server: a module `fork_server` for blaze's generated test
/// harness to put beside its `main` (inside the module that has `extern crate test`), with
/// `pub fn serve(tests: &'static [&'static test::TestDescAndFn], socket: OsString) -> !` and
/// `pub const SOCKET: &str` ([`wire::SOCKET`]). The harness's `main` calls `serve` when the
/// variable is set and `test::test_main_env_args` otherwise.
///
/// With `record` (a binary with jump-table slots), the server asks the runtime's
/// `__blaze_exec_slots` once, maps each test's execution record before its fork, has the child
/// arm it (`__blaze_exec_arm`) and sends what it executed ([`wire::read_region`]); without, it
/// names neither export, and every end says [`wire::UNRECORDED`]. Both texts are constants:
/// nothing is made per binary or per test.
pub fn fork_server(record: bool) -> &'static str {
    match record {
        true => RECORDED,
        false => UNRECORDED,
    }
}

const RECORDED: &str = concat!(
    "\n    #[allow(dead_code)]\n    pub mod fork_server {\n",
    include_str!("test/wire.rs"),
    include_str!("test/fork_server.rs"),
    include_str!("test/fork_server/recorded.rs"),
    "    }\n"
);

const UNRECORDED: &str = concat!(
    "\n    #[allow(dead_code)]\n    pub mod fork_server {\n",
    include_str!("test/wire.rs"),
    include_str!("test/fork_server.rs"),
    include_str!("test/fork_server/unrecorded.rs"),
    "    }\n"
);

/// What the agent API's `TestOutcome` says: the same variants, for whoever maps this crate's
/// results onto `blaze_api::TestResult` (this crate can't depend on `blaze-api`, which runs
/// tests through it).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Reported {
    Passed,
    Failed,
    Ignored,
    /// Not run: the edit reached none of its code; its last result stands.
    Unreached,
}

/// A result in the shape of `blaze_api::TestResult`: the fields map one to one.
#[derive(PartialEq, Eq, Debug)]
pub struct Report {
    pub binary: String,
    /// The test; empty for a binary run whole as libtest's threads.
    pub name: String,
    pub outcome: Reported,
    pub output: Vec<u8>,
    pub millis: u64,
}

/// A test that ran, as reported; `None` for one not run (fail-fast, a cancel), which the API
/// does not report.
pub fn report(binary: &str, finished: &Finished) -> Option<Report> {
    let outcome = match finished.outcome {
        Outcome::Passed => Reported::Passed,
        Outcome::Failed(_) => Reported::Failed,
        Outcome::NotRun => return None,
    };
    let mut output = finished.output.clone(); // clone: the API's report owns its output
    if let Outcome::Failed(failure) = &finished.outcome {
        // Into the output's own buffer, with no string of its own.
        _ = match failure {
            Failure::Code(_) => Ok(()),
            Failure::Signal(signal) => write!(output, "\n[blaze: ended by signal {signal}]\n"),
            Failure::TimedOut(after) => write!(output, "\n[blaze: killed after its timeout of {after:?}]\n"),
            Failure::Lost(why) => write!(output, "\n[blaze: {why}]\n"),
        };
    }
    Some(Report {
        binary: binary.to_owned(), // alloc: the API's report owns its names (the edge)
        name: finished.test.clone().unwrap_or_default(), // clone: the API's report owns its names (the edge)
        outcome,
        output,
        millis: millis(finished.took),
    })
}

/// A test that did not run: ignored, or skipped because its pass is kept.
pub fn report_unrun(binary: &str, name: &str, outcome: Reported, took: Option<Duration>) -> Report {
    Report {
        binary: binary.to_owned(), // alloc: the API's report owns its names (the edge)
        name: name.to_owned(), // alloc: the API's report owns its names (the edge)
        outcome,
        output: Vec::new(), // alloc: empty, allocates nothing
        millis: took.map_or(0, millis),
    }
}

fn millis(took: Duration) -> u64 {
    took.as_millis().min(u128::from(u64::MAX)) as u64
}

#[cfg(test)]
mod tests;

/// Counts allocations for the budget tests of the hot paths here (the wire, the region read,
/// the interner, the wheel, the queue's steady state, a test's identity).
#[cfg(test)]
#[global_allocator]
static ALLOC: blaze_alloc_count::Counting = blaze_alloc_count::Counting;
