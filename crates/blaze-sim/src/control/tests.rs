//! `control`: a caught plant passes, and each way a control can fail panics saying which.

use std::cell::Cell;
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
    assert!(said.starts_with("Unbounded was caught "), "{said}");
    assert!(said.contains(" times in 64 cases, never saying `lost a message`; first: `the count "), "{said}");
    assert!(said.ends_with(" is past its bound of 4`"), "{said}");
}

/// A plant that trips two of an oracle's checks: check A on the first ten calls, check B on a
/// later call that draws more than 4 steps. Clean, it trips neither.
fn two_checks(calls: &Cell<u32>) -> impl Fn(&Tape, Bug) -> Result<(), Broke> + '_ {
    move |tape: &Tape, bug: Bug| {
        let steps = tape.below(8, "steps");
        if bug == Bug::None {
            return Ok(());
        }
        calls.set(calls.get() + 1);
        let call = calls.get();
        match (call <= 10, steps > 4) {
            (true, _) => Err(Broke::Oracle(format!("check A on call {call}"))),
            (false, true) => Err(Broke::Oracle(format!("check B on call {call}"))),
            (false, false) => Ok(()),
        }
    }
}

#[test]
fn a_plant_caught_by_another_check_first_passes_a_control_naming_the_later_one() {
    let calls = Cell::new(0);
    control(Config::cases(64), Bug::None, Bug::Unbounded, "check B", two_checks(&calls));
    assert!(calls.get() > 10, "the planted run stopped before check B could trip");
}

#[test]
fn a_control_whose_words_no_case_says_panics_with_the_count_and_the_first_other_text() {
    let calls = Cell::new(0);
    let said = panic_of(|| control(Config::cases(64), Bug::None, Bug::Unbounded, "check C", two_checks(&calls)));
    let said = said.expect("a control whose words no case says panics");
    let count: u32 = said
        .strip_prefix("Unbounded was caught ")
        .and_then(|rest| rest.split(' ').next())
        .and_then(|count| count.parse().ok())
        .unwrap_or_else(|| panic!("no count in `{said}`"));
    assert!(count > 10 && count <= 64, "{said}");
    assert!(said.ends_with(" times in 64 cases, never saying `check C`; first: `check A on call 1`"), "{said}");
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
