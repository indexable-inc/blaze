//! `control`: a caught plant passes, and each way a control can fail panics saying which.

use std::panic::{AssertUnwindSafe, catch_unwind};

use super::*;

/// The code under a control: a counter that a planted bug lets go past its bound.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Bug {
    None,
    /// The bound is not held.
    Unbounded,
    /// The clean code is itself wrong: it reports every case.
    AlwaysReports,
}

fn counter(tape: &Tape, bug: Bug) -> Result<(), Broke> {
    let steps = tape.below(8, "steps");
    let bound = match bug {
        Bug::Unbounded => u64::MAX,
        Bug::None | Bug::AlwaysReports => 4,
    };
    let count = steps.min(bound);
    if bug == Bug::AlwaysReports || count > 4 {
        return Err(Broke::Oracle(format!("the count {count} is past its bound of 4")));
    }
    Ok(())
}

/// The text of the panic `control` raised, or `None` when it passed.
fn panic_of(control: impl FnOnce()) -> Option<String> {
    let payload = catch_unwind(AssertUnwindSafe(control)).err()?;
    match (payload.downcast_ref::<String>(), payload.downcast_ref::<&str>()) {
        (Some(text), _) => Some(text.clone()),
        (None, Some(text)) => Some((*text).to_owned()),
        (None, None) => Some("a panic whose payload is not text".to_owned()),
    }
}

#[test]
fn a_plant_caught_saying_the_words_passes() {
    control(Config::cases(64), Bug::None, Bug::Unbounded, "past its bound", counter);
}

#[test]
fn an_oracle_that_misses_the_plant_panics_saying_so() {
    let said = panic_of(|| control(Config::cases(64), Bug::None, Bug::None, "past its bound", counter));
    let said = said.expect("a control whose plant is the clean code panics");
    assert_eq!(said, "the oracle missed None in 64 cases");
}

#[test]
fn a_plant_caught_with_other_words_panics_quoting_them() {
    let said = panic_of(|| control(Config::cases(64), Bug::None, Bug::Unbounded, "lost a message", counter));
    let said = said.expect("a control caught with other words panics");
    assert!(said.starts_with("Unbounded was caught, but saying `the count "), "{said}");
    assert!(said.ends_with("is past its bound of 4`, not `lost a message`"), "{said}");
}

#[test]
fn a_plant_caught_by_a_panic_is_not_the_oracle_catching_it() {
    let said = panic_of(|| {
        control(Config::cases(64), Bug::None, Bug::Unbounded, "past its bound", |tape, bug| {
            let steps = tape.below(8, "steps");
            if bug == Bug::Unbounded && steps > 4 {
                panic!("the count is past its bound");
            }
            Ok(())
        })
    });
    let said = said.expect("a plant caught by a panic, not the oracle, fails the control");
    assert!(said.starts_with("Unbounded was caught, but saying `the count is past its bound`"), "{said}");
}

#[test]
fn a_control_whose_clean_run_fails_panics_with_the_report() {
    let said = panic_of(|| control(Config::cases(64), Bug::AlwaysReports, Bug::Unbounded, "past its bound", counter));
    let said = said.expect("a control whose clean run fails panics");
    assert!(said.starts_with("the control's clean run did not pass:\nblaze-sim: `"), "{said}");
    assert!(said.contains("failed at seed 0 (case 1 of 64): the count 0 is past its bound of 4"), "{said}");
    assert!(said.contains("replay: BLAZE_SIM_TAPE="), "{said}");
}
