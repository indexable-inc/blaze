//! A tape as text: what a failure prints and `BLAZE_SIM_TAPE` gives back. Values in decimal with
//! one space between, a run of two or more zeros as `0*n`, trailing zeros cut, so a shrunk tape
//! of a few choices that are not calm prints as a short line.

#![blaze::sans_io]

use std::alloc::Global;
use std::fmt::Write as _;

use crate::tape::Tape;

#[cfg(test)]
mod tests;

/// A tape as text: its values in decimal, a space between, a run of `n >= 2` zeros as `0*n`,
/// trailing zeros cut. The empty tape is the empty string.
pub fn print(values: &[u64]) -> String {
    print_runs(values, 2)
}

/// `print`, writing a run of zeros as `0*n` from `least_run` zeros on. `print` is 2; a control
/// passes 1, the planted bug its test catches.
fn print_runs(values: &[u64], least_run: usize) -> String {
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
            // Writing to a `String` cannot fail.
            let _ = write!(out, "0*{zeros}");
            at += zeros;
        } else {
            let _ = write!(out, "{}", values[at]);
            at += 1;
        }
    }
    out
}

/// The inverse, strict: one space between tokens, no leading or trailing space, `0*n` only with
/// `n >= 2`, no other run form.
pub fn parse(text: &str) -> Result<Vec<u64>, TextError> {
    let mut values = Vec::new_in(Global);
    if text.is_empty() {
        return Ok(values);
    }
    let most = Tape::MOST_CHOICES as usize;
    let mut at = 0;
    for token in text.split(' ') {
        if token.is_empty() {
            // The space that should not be there: the one at `at`, or for a trailing space, the
            // last byte.
            let space = if at < text.len() { at } else { at - 1 };
            return Err(TextError { at: space, problem: Problem::ExtraSpace });
        }
        let refuse = |problem| Err(TextError { at, problem });
        match token.split_once('*') {
            Some((zero, count)) => {
                if zero != "0" || count.is_empty() || !count.bytes().all(|b| b.is_ascii_digit()) {
                    return refuse(Problem::BadRun);
                }
                let Ok(count) = count.parse::<usize>() else {
                    return refuse(Problem::TooLong);
                };
                if count < 2 {
                    return refuse(Problem::BadRun);
                }
                if count > most - values.len() {
                    return refuse(Problem::TooLong);
                }
                values.resize(values.len() + count, 0);
            }
            None => {
                if !token.bytes().all(|b| b.is_ascii_digit()) {
                    return refuse(Problem::NotANumber);
                }
                let Ok(value) = token.parse::<u64>() else {
                    return refuse(Problem::TooLarge);
                };
                if values.len() == most {
                    return refuse(Problem::TooLong);
                }
                values.push(value);
            }
        }
        at += token.len() + 1;
    }
    Ok(values)
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
            Problem::ExtraSpace => "one space goes between values, and none before the first or after the last",
            Problem::TooLong => "more values than a case draws (Tape::MOST_CHOICES, 1048576), a run counting its length",
        };
        write!(f, "the tape's text at byte {}: {problem}", self.at)
    }
}

impl std::error::Error for TextError {}
