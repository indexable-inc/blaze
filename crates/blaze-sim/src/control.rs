//! Planted-bug controls (rule dst-not-review): a test's oracle is trusted only once it has been
//! seen to catch a bug planted on purpose, and a clean run of the same code has been seen to
//! pass. [`Broke`] is what an oracle, or the harness itself, says broke a case.

use core::alloc::Allocator;
use core::fmt::Debug;
use std::cell::Cell;
use std::panic::Location;

use crate::check::{observed_lines, report_here, say_observed, search, search_observing};
use crate::observe::{Caught, plant_lines_into};
use crate::search::{Broke, Config, Origin, Verdict};
use crate::tape::Tape;

#[cfg(test)]
mod tests;

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
/// When it passes it prints its observe block (observe.rs) on standard error: the case that
/// caught the plant ("plant <plant>: caught at case k of n", and "weak: .." when that was in the
/// last tenth of the cases) after what the clean run never varied. Fails nothing for it.
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
    let mut observed = observed_lines();
    control_observed(at, config, clean, plant, says, run, &mut observed);
    say_observed(at, &observed);
}

/// `control` from the place `at`, appending the lines of its observe block (observe.rs) to `out`
/// in place of printing them: what the clean run's passing cases never varied, then where the
/// planted run caught `plant` ("plant <plant>: caught at case k of n"), and "weak" when that was
/// in the last tenth of the cases. Panics as `control` does.
#[expect(
    blaze::error_debug,
    reason = "a plant is a value of the caller's own type, which the signature asks only to be `Debug`: its `Debug` form is the name the messages give it"
)]
pub(crate) fn control_observed<P: Copy + Debug, A: Allocator>(
    at: &Location<'_>,
    config: Config,
    clean: P,
    plant: P,
    says: &str,
    run: impl Fn(&Tape, P) -> Result<(), Broke>,
    out: &mut blaze_own::String<A>,
) {
    let (clean_run, clean_search) = search_observing(config.clone(), |tape| run(tape, clean));
    clean_search.tally().lines_into(out);
    if !matches!(clean_run, Verdict::Passed(_)) {
        panic!("the control's clean run did not pass:\n{}", report_here(&clean_run, at));
    }
    // Oracle findings in other words: how many, and the first one's text.
    let others = Cell::new(0u32);
    let first_other: Cell<Option<String>> = Cell::new(None);
    let planted_run = search(config.shrink_runs(0), |tape| match run(tape, plant) {
        Err(Broke::Oracle(why)) if !why.contains(says) => {
            others.set(others.get() + 1);
            let first = first_other.take();
            first_other.set(Some(first.unwrap_or(why)));
            Ok(())
        }
        ran => ran,
    });
    match &planted_run {
        Verdict::Failed(failed)
            if failed.failure.origin == Origin::Oracle && failed.failure.text.contains(says) =>
        {
            let caught = Caught { case: failed.case, cases: failed.cases };
            plant_lines_into(&plant, caught, out);
        }
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
        Verdict::NotAFunction { .. }
        | Verdict::DrewNothing { .. }
        | Verdict::AllDiscarded { .. }
        | Verdict::Unreached { .. }
        | Verdict::Misfit { .. } => panic!(
            "the control's run of {plant:?} was not caught by the oracle:\n{}",
            report_here(&planted_run, at)
        ),
    }
}
