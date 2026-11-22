//! `differ`: a planted difference on one class of inputs is caught and shrunk to the least input
//! of the class, over two sides and over N instantiations of one generic function; a panic on
//! one side is a difference; an approved difference that never fires is reported; a run that is
//! not the declared one does not judge approvals; too few sides are refused by name.

use std::panic::{AssertUnwindSafe, catch_unwind};

use super::{Answer, Approved, Compared, Differ, Pair, Refused, Side, compare, differ};
use crate::search::{Config, Given, Origin, Verdict};
use crate::tape::Tape;

const CASES: u32 = 256;

/// The input of the two-sided tests: one value below 1,000.
fn draw_x(tape: &Tape) -> u64 {
    tape.below(1_000, "x")
}

/// The reference: twice `x`.
fn old_twice(x: &u64) -> u64 {
    x * 2
}

/// The same, by a shift: equal on every input.
fn new_twice(x: &u64) -> u64 {
    x << 1
}

/// PLANTED: one more than twice `x` for every `x` of 100 or more, and right below.
fn new_twice_planted(x: &u64) -> u64 {
    if *x >= 100 { x * 2 + 1 } else { x * 2 }
}

/// Twice `x`, panicking for every `x` of 500 or more.
fn twice_below_500(x: &u64) -> u64 {
    assert!(*x < 500, "x {x} is past the table");
    x * 2
}

/// The same refusal, by another implementation.
fn shift_below_500(x: &u64) -> u64 {
    assert!(*x < 500, "no entry for {x}");
    x << 1
}

/// The least tape's values, as drawn.
fn least_values(compared: &Compared) -> Vec<u64> {
    match &compared.verdict {
        Verdict::Failed(failed) => failed.least.choices.iter().map(|choice| choice.value).collect(),
        other => panic!("expected a difference, got {other:?}"),
    }
}

/// The text of the difference a run failed with, checked to come from the oracle.
fn difference_text(compared: &Compared) -> &str {
    match &compared.verdict {
        Verdict::Failed(failed) => {
            assert_eq!(failed.failure.origin, Origin::Oracle, "a difference is the oracle's");
            &failed.failure.text
        }
        other => panic!("expected a difference, got {other:?}"),
    }
}

/// Prints `test differ::tests::two_sides_planted_difference_shrinks_to_its_least_input ... ok`.
///
/// The planted side differs on the class `x >= 100` alone: the search finds it, the shrinker
/// lowers `x` to the least member of the class, 100, and the failure prints the input and both
/// answers. The clean pair passes every case, all of them equal.
#[test]
fn two_sides_planted_difference_shrinks_to_its_least_input() {
    let planted = compare(
        Config::cases(CASES),
        draw_x,
        &[Side::new("old", &old_twice), Side::new("new", &new_twice_planted)],
        &Differ::plain(),
    )
    .expect("two sides are a comparison");
    assert_eq!(least_values(&planted), [100], "shrunk to the least input that differs");
    assert_eq!(
        difference_text(&planted),
        "`new` differs from `old`\n  input  100\n  old    200\n  new    201"
    );

    let clean = compare(
        Config::cases(CASES),
        draw_x,
        &[Side::new("old", &old_twice), Side::new("new", &new_twice)],
        &Differ::plain(),
    )
    .expect("two sides are a comparison");
    assert!(matches!(clean.verdict, Verdict::Passed(_)), "the clean pair passes: {clean:?}");
    assert_eq!(clean.summary.tally.equal, CASES);
    assert!(clean.summary.judged);
}

/// A strategy of summing a list: rule 78's seam.
trait Order {
    fn total(xs: &[u64]) -> u64;
}

struct Forward;
struct Backward;
/// PLANTED: leaves out every element of 200 or more.
struct Capped;

impl Order for Forward {
    fn total(xs: &[u64]) -> u64 {
        xs.iter().sum()
    }
}

impl Order for Backward {
    fn total(xs: &[u64]) -> u64 {
        xs.iter().rev().sum()
    }
}

impl Order for Capped {
    fn total(xs: &[u64]) -> u64 {
        xs.iter().filter(|x| **x < 200).sum()
    }
}

/// The one generic function, instantiated per strategy.
fn total<S: Order>(xs: &[u64]) -> u64 {
    S::total(xs)
}

/// A list of at most 8 values below 256.
fn draw_list(tape: &Tape) -> Vec<u64> {
    tape.vec(0..=8, "xs", |tape| tape.below(256, "x"))
}

/// Prints `test differ::tests::instantiations_compared_to_the_first ... ok`.
///
/// Three instantiations of `total` over its strategy parameter, each compared to the first: the
/// planted `Capped` differs on lists holding an element of 200 or more, and is shrunk to the
/// least such list, `[200]`, named by its strategy's type. `Forward` and `Backward` alone pass.
#[test]
fn instantiations_compared_to_the_first() {
    let forward = |xs: &Vec<u64>| total::<Forward>(xs);
    let backward = |xs: &Vec<u64>| total::<Backward>(xs);
    let capped = |xs: &Vec<u64>| total::<Capped>(xs);
    let planted = compare(
        Config::cases(CASES),
        draw_list,
        &[
            Side::of::<Forward>(&forward),
            Side::of::<Backward>(&backward),
            Side::of::<Capped>(&capped),
        ],
        &Differ::plain(),
    )
    .expect("three sides are a comparison");
    let text = difference_text(&planted);
    assert!(
        text.starts_with("`Capped` differs from `Forward`\n  input    [200]\n"),
        "the least list that differs, named by strategy: {text}"
    );

    let clean = compare(
        Config::cases(CASES),
        draw_list,
        &[Side::of::<Forward>(&forward), Side::of::<Backward>(&backward)],
        &Differ::plain(),
    )
    .expect("two sides are a comparison");
    assert!(matches!(clean.verdict, Verdict::Passed(_)), "the clean pair passes: {clean:?}");
}

/// Prints `test differ::tests::a_panic_on_one_side_is_a_difference ... ok`.
///
/// One side panics from 500 up and the other does not: a difference from the oracle (not a
/// panic of the case), shrunk to 500, its answer printed as the panic. Both sides panicking on
/// the same inputs agree, and the summary counts those pairs.
#[test]
fn a_panic_on_one_side_is_a_difference() {
    let one_panics = compare(
        Config::cases(CASES),
        draw_x,
        &[Side::new("old", &old_twice), Side::new("new", &twice_below_500)],
        &Differ::plain(),
    )
    .expect("two sides are a comparison");
    assert_eq!(least_values(&one_panics), [500]);
    assert_eq!(
        difference_text(&one_panics),
        "`new` differs from `old`\n  input  500\n  old    1000\n  new    panicked: `x 500 is past the table`"
    );

    let both_panic = compare(
        Config::cases(CASES),
        draw_x,
        &[Side::new("old", &twice_below_500), Side::new("new", &shift_below_500)],
        &Differ::plain(),
    )
    .expect("two sides are a comparison");
    assert!(matches!(both_panic.verdict, Verdict::Passed(_)), "they agree: {both_panic:?}");
    assert!(both_panic.summary.tally.panicked_alike > 0, "{:?}", both_panic.summary);
    assert_eq!(
        both_panic.summary.tally.equal, CASES,
        "a case whose sides agree by panicking is an equal case"
    );
}

/// Allows the planted difference: from 100 up, the new side gives one more.
fn one_more_from_100(pair: &Pair<'_, u64, u64>) -> bool {
    match (pair.first_answer, pair.other_answer) {
        (Answer::Returned(old), Answer::Returned(new)) => *pair.input >= 100 && *new == old + 1,
        (Answer::Returned(_) | Answer::Panicked { .. }, _) => false,
    }
}

/// An approval no drawn input reaches: inputs are below 1,000.
fn from_2000(pair: &Pair<'_, u64, u64>) -> bool {
    *pair.input >= 2_000
}

const ALLOWED: &str = "x of 100 or more is one more now (planted)";
const STALE: &str = "x of 2000 or more (no such input is drawn)";

/// Prints `test differ::tests::an_approval_that_never_fires_is_reported ... ok`, and, from the
/// `differ` inside it, `blaze-sim: differ at crates/blaze-sim/src/differ/tests.rs:<line>: 256
/// cases: <n> equal, <m> approved ("x of 100 or more is one more now (planted)" <m>, "x of 2000
/// or more (no such input is drawn)" 0), 0 discarded`.
///
/// The planted difference is approved, so the run passes; the second approval matches none of
/// the declared cases and is reported: by `compare` as stale, by `differ` as a panic naming it.
/// Without the stale entry `differ` passes.
#[test]
fn an_approval_that_never_fires_is_reported() {
    let approved = [Approved::new(ALLOWED, one_more_from_100), Approved::new(STALE, from_2000)];
    let compared = compare(
        Config::cases(CASES),
        draw_x,
        &[Side::new("old", &old_twice), Side::new("new", &new_twice_planted)],
        &Differ::plain().approved(&approved),
    )
    .expect("two sides are a comparison");
    assert!(matches!(compared.verdict, Verdict::Passed(_)), "approved: {compared:?}");
    let fired = compared.summary.tally.fired();
    assert_eq!(fired.len(), 2);
    assert!(fired[0].1 > 0, "the planted difference was drawn and approved: {fired:?}");
    assert_eq!(fired[1], (STALE, 0));
    assert_eq!(compared.summary.tally.approved, fired[0].1);
    assert_eq!(compared.summary.stale().collect::<Vec<_>>(), [STALE]);

    let panicked = catch_unwind(AssertUnwindSafe(|| {
        differ(
            Config::cases(CASES),
            draw_x,
            Side::new("old", &old_twice),
            Side::new("new", &new_twice_planted),
            Differ::plain().approved(&approved),
        );
    }));
    let payload = panicked.expect_err("a stale approval fails the differential test");
    let message = payload.downcast_ref::<String>().expect("the panic's text");
    assert!(
        message.contains(&format!(
            "approved differences that matched none of the {CASES} declared cases: \"{STALE}\""
        )),
        "{message}"
    );

    let current = [Approved::new(ALLOWED, one_more_from_100)];
    differ(
        Config::cases(CASES),
        draw_x,
        Side::new("old", &old_twice),
        Side::new("new", &new_twice_planted),
        Differ::plain().approved(&current),
    );
}

/// Prints `test differ::tests::a_given_tape_does_not_judge_approvals ... ok`.
///
/// One given tape is not the declared run: an approval it did not reach is not stale.
#[test]
fn a_given_tape_does_not_judge_approvals() {
    let approved = [Approved::new(STALE, from_2000)];
    let given = Given { tape: Some(vec![5]), ..Given::NOTHING };
    let compared = compare(
        Config::cases(CASES).given(given),
        draw_x,
        &[Side::new("old", &old_twice), Side::new("new", &new_twice)],
        &Differ::plain().approved(&approved),
    )
    .expect("two sides are a comparison");
    assert!(matches!(compared.verdict, Verdict::Passed(_)), "{compared:?}");
    assert!(!compared.summary.judged);
    assert_eq!(compared.summary.stale().count(), 0);
    let line = compared.summary.to_string();
    assert!(line.contains("approvals not judged: 1 cases ran, not the 256 declared"), "{line}");
}

/// Prints `test differ::tests::one_side_is_refused_by_name ... ok`.
#[test]
fn one_side_is_refused_by_name() {
    let refused = compare(
        Config::cases(CASES),
        draw_x,
        &[Side::new("old", &old_twice)],
        &Differ::plain(),
    );
    assert_eq!(refused.err(), Some(Refused::TooFewSides { given: 1 }));
}
