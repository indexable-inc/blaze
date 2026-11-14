use super::*;
use crate::{Broke, Config, Tape};

/// A bug a control plants in `print`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Plant {
    None,
    /// A lone zero is written as the run `0*1`, which `parse` refuses.
    RunOfOne,
}

/// A tape's values with zeros often, alone and in runs, and the odd large value.
fn values(tape: &Tape) -> Vec<u64> {
    tape.vec(0..=40, "values", |t| match t.below(4, "kind") {
        0 | 1 => 0,
        2 => t.below(4, "small"),
        _ => t.int::<u64>("any"),
    })
}

fn round_trips(tape: &Tape, plant: Plant) -> Result<(), Broke> {
    let values = values(tape);
    let text = match plant {
        Plant::None => print(&values),
        Plant::RunOfOne => print_runs(&values, 1),
    };
    let kept = values.iter().rposition(|&value| value != 0).map_or(0, |last| last + 1);
    let back = parse(&text);
    if back.as_deref() != Ok(&values[..kept]) {
        return Err(Broke::Oracle(format!("{values:?} printed as `{text}` parsed back as {back:?}")));
    }
    Ok(())
}

/// The invariant: `parse(print(values))` is `values` with its trailing zeros cut.
#[test]
fn print_then_parse_is_the_values_cut() {
    crate::check(Config::cases(300), |tape| round_trips(tape, Plant::None));
}

#[test]
fn the_oracle_catches_a_print_that_writes_a_lone_zero_as_a_run() {
    crate::control(Config::cases(300), Plant::None, Plant::RunOfOne, "parsed back as", round_trips);
}

#[test]
fn print_writes_runs_and_cuts_trailing_zeros() {
    assert_eq!(print(&[]), "");
    assert_eq!(print(&[0, 0, 0]), "");
    assert_eq!(print(&[3, 0, 0, 1, 0, 2, 0, 0]), "3 0*2 1 0 2");
    assert_eq!(print(&[0, 0, 0, u64::MAX]), "0*3 18446744073709551615");
    assert_eq!(parse("0 0*2 7"), Ok(vec![0, 0, 0, 7]));
    assert_eq!(parse("1 0"), Ok(vec![1, 0]), "a trailing zero someone wrote is kept");
}

fn refused(text: &str) -> (usize, Problem) {
    let error = parse(text).expect_err(text);
    (error.at, error.problem)
}

#[test]
fn each_problem_is_refused_where_it_starts() {
    assert_eq!(refused("1 x 2"), (2, Problem::NotANumber));
    assert_eq!(refused("1 +2"), (2, Problem::NotANumber));
    assert_eq!(refused("-1"), (0, Problem::NotANumber));
    assert_eq!(refused("18446744073709551616"), (0, Problem::TooLarge));
    assert_eq!(refused("4 18446744073709551616"), (2, Problem::TooLarge));
    assert_eq!(refused("0*1"), (0, Problem::BadRun));
    assert_eq!(refused("3 5*2"), (2, Problem::BadRun));
    assert_eq!(refused("0*"), (0, Problem::BadRun));
    assert_eq!(refused("1 0*x"), (2, Problem::BadRun));
    assert_eq!(refused(" 1"), (0, Problem::ExtraSpace));
    assert_eq!(refused("1  2"), (2, Problem::ExtraSpace));
    assert_eq!(refused("1 "), (1, Problem::ExtraSpace));
    assert_eq!(refused("0*1048577"), (0, Problem::TooLong));
    assert_eq!(refused("1 0*1048576"), (2, Problem::TooLong));
    assert_eq!(refused("0*99999999999999999999999"), (0, Problem::TooLong));
    let error = TextError { at: 2, problem: Problem::BadRun };
    assert_eq!(error.to_string(), "the tape's text at byte 2: a run is written `0*n`, with n of 2 or more");
}
