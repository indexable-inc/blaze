//! The driver: panics caught by place and message, the hook's depth, nesting, and the report.

use std::cell::Cell;
use std::panic::Location;

use super::*;
use crate::search::Passed;
use crate::tape::{Choice, Note};

#[test]
fn a_panic_in_a_body_is_caught_with_its_place_and_message() {
    let verdict = search(Config::cases(8).shrink_runs(0), |tape: &Tape| {
        if tape.below(2, "x") < 2 {
            panic!("boom on line {}", line!());
        }
    });
    let Verdict::Failed(failed) = &verdict else { panic!("{verdict:?}") };
    let Origin::Panic { at } = &failed.failure.origin else { panic!("{verdict:?}") };
    let line: u32 = failed.failure.text.strip_prefix("boom on line ").expect("the message is kept").parse().unwrap();
    let place = format!("{}:{line}:", file!());
    assert!(at.starts_with(&place), "the panic was kept at `{at}`, not at `{place}<column>`");
    assert!(at[place.len()..].parse::<u32>().is_ok(), "`{at}` does not end in a column");
    assert_eq!(depth(), 0);
}

#[test]
fn the_depth_is_back_at_zero_after_a_search_and_a_panic_outside_is_not_kept() {
    let verdict = search(Config::cases(2).shrink_runs(0), |tape: &Tape| {
        if tape.below(2, "x") < 2 {
            panic!("inside");
        }
    });
    assert!(matches!(verdict, Verdict::Failed(_)), "{verdict:?}");
    assert_eq!(depth(), 0, "a search whose body panicked left the depth raised");
    let passed = search(Config::cases(2), |tape: &Tape| {
        tape.below(2, "x");
    });
    assert!(matches!(passed, Verdict::Passed(_)), "{passed:?}");
    assert_eq!(depth(), 0, "a search that passed left the depth raised");
    take_kept();
    // Outside any search the hook hands the panic to the hook before it, and keeps nothing.
    let outside = std::panic::catch_unwind(|| {
        if depth() == 0 {
            panic!("outside");
        }
    });
    assert!(outside.is_err());
    assert_eq!(take_kept(), None, "a panic outside any search was kept as a case's");
}

#[test]
fn nested_searches_keep_their_own_panics() {
    let inner = Cell::new(None);
    let outer = search(Config::cases(1).shrink_runs(0), |tape: &Tape| {
        tape.below(2, "outer");
        let verdict = search(Config::cases(4).shrink_runs(0), |tape: &Tape| {
            if tape.below(2, "inner") < 2 {
                panic!("the inner body");
            }
        });
        let failed = matches!(verdict, Verdict::Failed(_));
        assert_eq!(depth(), 1, "inside the outer search, after the inner one");
        inner.set(Some(verdict));
        if failed {
            panic!("the outer body");
        }
    });
    let Verdict::Failed(outer) = &outer else { panic!("{outer:?}") };
    assert_eq!(outer.failure.text, "the outer body");
    let inner = inner.take().expect("the outer body ran the inner search");
    let Verdict::Failed(inner) = &inner else { panic!("{inner:?}") };
    assert_eq!(inner.failure.text, "the inner body");
    assert_ne!(inner.failure.origin, outer.failure.origin);
    assert_eq!(depth(), 0);
}

#[test]
fn a_discard_and_an_overrun_are_told_from_a_panic() {
    let verdict = search(Config::cases(3), |tape: &Tape| {
        if tape.below(2, "x") < 2 {
            tape.discard()
        }
    });
    assert_eq!(verdict, Verdict::AllDiscarded { cases: 3 });
    let verdict = search(Config::cases(1).shrink_runs(0).most_choices(3), |tape: &Tape| {
        for _ in 0..4 {
            tape.below(2, "x");
        }
    });
    let Verdict::Failed(failed) = &verdict else { panic!("{verdict:?}") };
    assert_eq!(failed.failure.origin, Origin::Overran);
    assert!(failed.failure.text.contains('3'), "`{}` does not name the bound", failed.failure.text);
}

#[test]
fn check_passes_a_passing_body_and_panics_with_the_report_otherwise() {
    check_given(Config::cases(4), Given::NOTHING, |tape: &Tape| {
        tape.below(2, "x");
    });
    let failed = std::panic::catch_unwind(|| {
        check_given(Config::cases(4).shrink_runs(0), Given::NOTHING, |tape: &Tape| -> Result<(), String> {
            tape.below(2, "x");
            Err("always".to_owned())
        });
    });
    let payload = failed.expect_err("a failing check panics");
    let said = payload.downcast_ref::<String>().expect("the report is the panic's text");
    assert!(said.contains("failed at seed 0 (case 1 of 4): always"), "{said}");
}

/// The failure the golden report is of.
fn golden_failed() -> Failed {
    let choice = |label, value, most| Choice { value, most, label };
    let mut least = Record::new();
    least.choices.extend([
        choice("op", 3, 5),
        choice("deliver", 0, 3),
        choice("deliver", 0, 3),
        choice("kill", 1, 1),
        choice("op", 0, 5),
        choice("deliver", 0, 3),
        choice("deliver", 2, 3),
    ]);
    least.notes.push(Note { at: 4, text: "killed after the second write".to_owned() });
    Failed {
        seed: Some(41),
        case: 42,
        cases: 256,
        failure: Failure { origin: Origin::Panic { at: "src/queue.rs:10:5".to_owned() }, text: "the queue lost a message".to_owned() },
        first_len: 4812,
        least,
        shrink_runs: 318,
    }
}

#[test]
fn the_report_of_a_failure_is_exactly_this() {
    let said = report(&Verdict::Failed(golden_failed()), Location::caller(), Some("queue_keeps_messages"));
    let expected = "\
blaze-sim: `queue_keeps_messages` failed at seed 41 (case 42 of 256): the queue lost a message
  at src/queue.rs:10:5
least tape: 7 choices, from 4812, in 318 shrink runs
  3 0*2 1 0*2 2
not calm: op = 3 of 0..=5 | kill = 1 of 0..=1 | deliver = 2 of 0..=3
notes:
  [4] killed after the second write
replay: BLAZE_SIM_TAPE='3 0*2 1 0*2 2'   (the case as found: BLAZE_SIM_SEED=41)";
    assert_eq!(said, expected, "\n{said}\n");
}

#[test]
fn the_report_of_every_other_verdict_names_what_it_is() {
    let at = Location::caller();
    let says = |verdict: Verdict, words: &[&str]| {
        let said = report(&verdict, at, Some("the_test"));
        assert!(said.starts_with("blaze-sim: `the_test`"), "{said}");
        for word in words {
            assert!(said.contains(word), "the report of {verdict:?} does not say `{word}`:\n{said}");
        }
    };
    says(
        Verdict::Passed(Passed { cases: 9, discarded: 2, choices: 70 }),
        &["passed", "9 cases", "2 discarded", "70 choices"],
    );
    says(
        Verdict::NotAFunction {
            seed: Some(5),
            tape: vec![2, 0, 1],
            first: Failure { origin: Origin::Oracle, text: "the first".to_owned() },
            again: Ended::Passed,
        },
        &["not a function of its tape", "`the first`", "passed", "BLAZE_SIM_TAPE='2 0 1'", "BLAZE_SIM_SEED=5"],
    );
    says(Verdict::DrewNothing { cases: 9 }, &["9 cases drew no choice"]);
    says(Verdict::AllDiscarded { cases: 9 }, &["all 9 cases were discarded"]);
    says(Verdict::Unreached { cases: 9, missing: vec!["a crash", "a torn write"] }, &["9 cases", "`a crash`", "`a torn write`"]);
    says(
        Verdict::Misfit { clamped: 2, unused: 3, ended: Ended::Discarded },
        &["does not fit", "2 values lowered", "3 never drawn", "was discarded"],
    );
    let mut given = golden_failed();
    given.seed = None;
    given.shrink_runs = 0;
    says(Verdict::Failed(given), &["failed on the given tape", "as given", "replay: BLAZE_SIM_TAPE='3 0*2 1 0*2 2'"]);
}
