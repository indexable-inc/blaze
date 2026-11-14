//! Planted-bug controls (rule dst-not-review): a test's oracle is trusted only once it has been
//! seen to catch a bug planted on purpose, and a clean run of the same code has been seen to
//! pass. [`Broke`] is what an oracle, or the harness itself, says broke a case.

use core::fmt::Debug;
use std::panic::Location;

use crate::check::{report, search};
use crate::search::{Config, Origin, Verdict};
use crate::tape::Tape;

#[cfg(test)]
mod tests;

/// What broke a case.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Broke {
    /// The oracle saw the invariant broken: a finding.
    Oracle(String),
    /// The test itself went wrong (a model out of step, a bound of the harness's own).
    Harness(String),
}

impl core::fmt::Display for Broke {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Broke::Oracle(why) => write!(f, "the oracle: {why}"),
            Broke::Harness(why) => write!(f, "the harness: {why}"),
        }
    }
}

impl std::error::Error for Broke {}

/// A planted-bug control (rule dst-not-review): `run` passes every case as the code is
/// (`clean`), and with `plant` some case is caught by the oracle, saying `says`. Reads nothing
/// from outside; panics, saying which half failed, when either does not hold.
#[track_caller]
pub fn control<P: Copy + Debug>(
    config: Config,
    clean: P,
    plant: P,
    says: &str,
    run: impl Fn(&Tape, P) -> Result<(), Broke>,
) {
    let at = Location::caller();
    let thread = std::thread::current();
    let test = thread.name().filter(|name| *name != "main");
    let verdict = search(config.clone(), |tape| run(tape, clean));
    if !matches!(verdict, Verdict::Passed(_)) {
        panic!("the control's clean run did not pass:\n{}", report(&verdict, at, test));
    }
    let verdict = search(config.shrink_runs(0), |tape| run(tape, plant));
    match &verdict {
        Verdict::Failed(failed) if failed.failure.origin == Origin::Oracle && failed.failure.text.contains(says) => {}
        Verdict::Failed(failed) => {
            panic!("{plant:?} was caught, but saying `{}`, not `{says}`", failed.failure.text)
        }
        Verdict::Passed(passed) => panic!("the oracle missed {plant:?} in {} cases", passed.cases),
        _ => panic!("the control's run of {plant:?} was not caught by the oracle:\n{}", report(&verdict, at, test)),
    }
}
