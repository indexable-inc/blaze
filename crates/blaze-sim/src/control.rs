//! Planted-bug controls (rule dst-not-review): a test's oracle is trusted only once it has been
//! seen to catch a bug planted on purpose, and a clean run of the same code has been seen to
//! pass. [`Broke`] is what an oracle, or the harness itself, says broke a case.

use core::fmt::Debug;
use std::cell::Cell;
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

/// A planted-bug control (rule dst-not-review): the check named by `says` catches `plant` in
/// some case, and the code as it is (`clean`) passes every case.
///
/// The clean run, `run(tape, clean)` over `config`, has to end `Verdict::Passed`. The planted
/// run goes through the cases of `config`, unshrunk, until one fails as `Broke::Oracle(why)`
/// with `why` containing `says`: that is the catch, and the control passes. A case the oracle
/// fails with other words (another of its checks) is counted, the first such text kept, and the
/// run goes on as if that case had passed. A case that fails any other way (`Broke::Harness`, a
/// panic, an overrun) ends the control at once: the plant was not caught by the oracle.
///
/// Reads nothing from outside. Panics, saying which half failed: the clean run's report; a
/// planted failure of another kind, quoting it; `<plant> was caught <n> times in <cases> cases,
/// never saying ..; first: ..` when the oracle spoke but never the words; `the oracle missed
/// <plant> in <cases> cases` when it never spoke; or the report of a planted run that ended
/// neither failed nor passed (nothing drawn, all discarded, a point unreached).
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
    // Oracle findings in other words: how many, and the first one's text.
    let others = Cell::new(0u32);
    let first_other: Cell<Option<String>> = Cell::new(None);
    let verdict = search(config.shrink_runs(0), |tape| match run(tape, plant) {
        Err(Broke::Oracle(why)) if !why.contains(says) => {
            others.set(others.get() + 1);
            let first = first_other.take();
            first_other.set(Some(first.unwrap_or(why)));
            Ok(())
        }
        ran => ran,
    });
    match &verdict {
        Verdict::Failed(failed) if failed.failure.origin == Origin::Oracle && failed.failure.text.contains(says) => {}
        Verdict::Failed(failed) => {
            panic!("{plant:?} was caught, but saying `{}`, not `{says}`", failed.failure.text)
        }
        Verdict::Passed(passed) => match first_other.take() {
            Some(first) => panic!(
                "{plant:?} was caught {} times in {} cases, never saying `{says}`; first: `{first}`",
                others.get(),
                passed.cases
            ),
            None => panic!("the oracle missed {plant:?} in {} cases", passed.cases),
        },
        _ => panic!("the control's run of {plant:?} was not caught by the oracle:\n{}", report(&verdict, at, test)),
    }
}
