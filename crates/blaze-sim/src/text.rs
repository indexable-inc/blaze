//! A tape as text: what a failure prints and `BLAZE_SIM_TAPE` gives back. Values in decimal with
//! one space between, a run of two or more zeros as `0*n`, trailing zeros cut, so a shrunk tape
//! of a few choices that are not calm prints as a short line.

#![blaze::sans_io]

use std::alloc::Global;
use std::fmt::Write as _;

use crate::tape::Tape;

/// The fewest zeros written as a run `0*n`: a lone zero is shorter written `0` than `0*1`, so
/// `print` writes runs from two zeros on and `parse` takes no shorter run.
const LEAST_RUN: usize = 2;

/// What a run repeats, the part of a `0*n` token before its `*`: runs are of zeros only.
const RUN_OF: &str = "0";

/// [`Tape::MOST_CHOICES`] as a count of values.
#[expect(
    blaze::as_cast,
    reason = "u32 to usize widens: usize is at least 32 bits on every target blaze builds for, and std has no `From<u32>` for `usize`"
)]
const MOST: usize = Tape::MOST_CHOICES as usize;

/// A tape as text: its values in decimal, a space between, a run of `n >= 2` zeros as `0*n`,
/// trailing zeros cut. The empty tape is the empty string.
#[blaze::pure]
#[blaze::nonblocking]
pub fn print(values: &[u64]) -> String {
    print_runs(values, LEAST_RUN)
}

/// `print`, writing a run of zeros as `0*n` from `least_run` zeros on. `print` is 2; a control
/// passes 1, the planted bug its test catches.
#[blaze::pure]
#[blaze::nonblocking]
pub(crate) fn print_runs(values: &[u64], least_run: usize) -> String {
    let kept = values.iter().rposition(|&value| value != 0).map_or(0, |last| last + 1);
    let values = &values[..kept];
    let mut out = String::new();
    let mut at = 0;
    // Each turn writes one token and moves past at least one value.
    while at < values.len() {
        if !out.is_empty() {
            out.push(' ');
        }
        let zeros = values[at..].iter().take_while(|&&value| value == 0).count();
        if zeros >= least_run {
            #[expect(clippy::let_underscore_must_use, reason = "writing to a `String` cannot fail")]
            let _ = write!(out, "{RUN_OF}*{zeros}");
            at += zeros;
        } else {
            #[expect(clippy::let_underscore_must_use, reason = "writing to a `String` cannot fail")]
            let _ = write!(out, "{}", values[at]);
            at += 1;
        }
    }
    out
}

/// The inverse, strict: one space between tokens, no leading or trailing space, `0*n` only with
/// `n >= 2`, no other run form.
#[blaze::pure]
#[blaze::nonblocking]
pub fn parse(text: &str) -> Result<Vec<u64>, TextError> {
    let mut values = Vec::new_in(Global);
    if text.is_empty() {
        return Ok(values);
    }
    let mut at = 0;
    for token in text.split(' ') {
        if token.is_empty() {
            // The space that should not be there: the one at `at`, or for a trailing space, the
            // last byte.
            let space = if at < text.len() { at } else { at - 1 };
            return Err(TextError { at: space, problem: Problem::ExtraSpace });
        }
        take_token(token, &mut values).map_err(|problem| TextError { at, problem })?;
        at += token.len() + 1;
    }
    Ok(values)
}

/// Appends what one token (not empty) stands for to `values`: a run's zeros or one value; or
/// why the token is refused.
fn take_token(token: &str, values: &mut Vec<u64>) -> Result<(), Problem> {
    match token.split_once('*') {
        Some((zero, count)) => take_run(zero, count, values),
        None => take_value(token, values),
    }
}

/// Appends the zeros of the run `{zero}*{count}`: `zero` is `0`, `count` decimal digits of at
/// least `LEAST_RUN`, and the tape stays within `MOST` values.
fn take_run(zero: &str, count: &str, values: &mut Vec<u64>) -> Result<(), Problem> {
    if zero != RUN_OF || count.is_empty() || !count.bytes().all(|b| b.is_ascii_digit()) {
        return Err(Problem::BadRun);
    }
    let Ok(count) = count.parse::<usize>() else {
        return Err(Problem::TooLong);
    };
    if count < LEAST_RUN {
        return Err(Problem::BadRun);
    }
    // `values` never holds more than `MOST`, so the difference does not wrap.
    if count > MOST - values.len() {
        return Err(Problem::TooLong);
    }
    values.resize(values.len() + count, 0);
    Ok(())
}

/// Appends the value `token` spells in decimal, while the tape holds fewer than `MOST` values.
fn take_value(token: &str, values: &mut Vec<u64>) -> Result<(), Problem> {
    if !token.bytes().all(|b| b.is_ascii_digit()) {
        return Err(Problem::NotANumber);
    }
    let Ok(value) = token.parse::<u64>() else {
        return Err(Problem::TooLarge);
    };
    if values.len() == MOST {
        return Err(Problem::TooLong);
    }
    values.push(value);
    Ok(())
}

/// Why a tape's text was refused, and where.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TextError {
    /// The byte the problem starts at.
    pub at: usize,
    pub problem: Problem,
}

/// What is wrong with a tape's text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Problem {
    /// A token that is not decimal digits or a run.
    NotANumber,
    /// A value above `u64::MAX`.
    TooLarge,
    /// A `*` token that is not `0*n` with `n >= 2`.
    BadRun,
    /// A space before the first value, after the last, or beside another.
    ExtraSpace,
    /// More than `Tape::MOST_CHOICES` values, a run counting its length.
    TooLong,
}

impl core::fmt::Display for TextError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let problem = match self.problem {
            Problem::NotANumber => "not a decimal number or a run `0*n`",
            Problem::TooLarge => "a value above u64::MAX (18446744073709551615)",
            Problem::BadRun => "a run is written `0*n`, with n of 2 or more",
            Problem::ExtraSpace => {
                "one space goes between values, and none before the first or after the last"
            }
            Problem::TooLong => {
                "more values than a case draws (Tape::MOST_CHOICES, 1048576), a run counting its length"
            }
        };
        write!(f, "the tape's text at byte {}: {problem}", self.at)
    }
}

impl std::error::Error for TextError {}
