//! One job: one call of an extension's function, bounded and caught.
//!
//! A panic in a job fails that job, with an error naming the extension, the lint or fact and
//! the item (rule `panics-fail-one-job`); a job past a bound is stopped the same way (rule
//! `bounded-progress`, `blaze_ext::host`). Either way what the job reported is dropped whole:
//! half a job's findings would differ run to run with where it was stopped. The caller goes
//! on to the next job.

use std::{fmt, panic::AssertUnwindSafe};

use blaze_ext::{
    Cx,
    host::{Finding, HostKey, Job, Limits, Source, Stop},
};
use blaze_mem::{Tag, frame};

/// Which function of an extension a job runs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum What {
    Lint(String),
    Fact(String),
}

impl fmt::Display for What {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            What::Lint(name) => write!(f, "lint `{name}`"),
            What::Fact(name) => write!(f, "fact `{name}`"),
        }
    }
}

/// How a job failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Failure {
    /// The extension's code panicked, with this message.
    Panicked(String),
    /// A bound, or a misuse of the API, stopped it.
    Stopped(Stop),
    /// Run twice, it did not give the same result: something in it depends on an address.
    Unrepeatable,
}

/// A job that failed: the extension, its function and the item it was given.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JobError {
    pub extension: String,
    pub what: What,
    pub item: String,
    pub failure: Failure,
}

impl fmt::Display for JobError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let JobError { extension, what, item, failure } = self;
        match failure {
            Failure::Panicked(message) => {
                write!(f, "extension `{extension}`: {what} panicked on `{item}`: {message}")
            }
            Failure::Stopped(stop) => {
                write!(f, "extension `{extension}`: {what} was stopped on `{item}`: {stop}")
            }
            Failure::Unrepeatable => write!(
                f,
                "extension `{extension}`: {what} gave two results for `{item}` on two runs: it depends on something \
                 that is not its input, such as an address (a pointer cast to an integer, compared or formatted)"
            ),
        }
    }
}

impl std::error::Error for JobError {}

/// How far the second run's arena is moved from the first's.
const SKEW: usize = 4096 + 40;

/// Runs `call` as one job: under `limits`, asking `facts`, in a frame of its own, with any
/// panic or stop caught. `lint` is the lint's index for a lint's job. With `replay` the job
/// runs a second time with its arena elsewhere, and two results that differ are a failure.
/// What `call` returns must own its memory: the arena ends with the job.
pub fn run<R: PartialEq>(
    key: &HostKey,
    limits: Limits,
    // dyn: asked once per `Cx::fact`
    facts: &dyn Source,
    lint: Option<u32>,
    replay: bool,
    call: impl for<'c> Fn(&mut Cx<'c>) -> R,
) -> Result<(R, Vec<Finding>), Failure> {
    let first = once(key, limits, facts, lint, 0, &call)?;
    if replay {
        let second = once(key, limits, facts, lint, SKEW, &call)?;
        if first != second {
            return Err(Failure::Unrepeatable);
        }
    }
    Ok(first)
}

fn once<R>(
    key: &HostKey,
    limits: Limits,
    // dyn: as `run`'s
    facts: &dyn Source,
    lint: Option<u32>,
    skew: usize,
    call: &impl for<'c> Fn(&mut Cx<'c>) -> R,
) -> Result<(R, Vec<Finding>), Failure> {
    frame::task(Tag::Diagnostics, |frame| {
        let job = Job::new(key, frame, limits, facts, skew);
        // The job's state is dropped whole on a failure, so nothing half-changed is seen.
        let outcome = std::panic::catch_unwind(AssertUnwindSafe(|| {
            let _entered = job.enter();
            let mut cx = job.cx(lint);
            call(&mut cx)
        }));
        match outcome {
            Ok(result) => Ok((result, job.take_findings())),
            Err(payload) => Err(match payload.downcast::<Stop>() {
                Ok(stop) => Failure::Stopped(*stop),
                Err(payload) => Failure::Panicked(message(&*payload)),
            }),
        }
    })
}

/// What a panic said.
// dyn: a panic's payload, as `catch_unwind` gives it
fn message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(text) = payload.downcast_ref::<&'static str>() {
        (*text).to_owned() // alloc: error path
    } else if let Some(text) = payload.downcast_ref::<String>() {
        text.clone() // clone: error path
    } else {
        "a panic with no message".to_owned() // alloc: error path
    }
}
