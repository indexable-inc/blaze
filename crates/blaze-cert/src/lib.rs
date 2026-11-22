//! A trait goal's check result as a checkable certificate (docs/v2/plan/design/certificates.md;
//! plan rows V1 and X2 of docs/v2/plan/design/runtime.md).
//!
//! | module | what |
//! |---|---|
//! | [`derivation`] | the format: a DAG of steps `(goal, by)` as rows in columns, written by a [`derivation::Builder`] and read in place by a [`derivation::View`] that checks every limit before a row is trusted |
//! | [`check`] | the checker: proves a derivation against today's facts by matching only, and returns the proof [`check::Checked`] |
//! | [`questions`] | the questions the solver's choice rested on, with the digests of their answers, and whether they still stand |
//! | [`emit`] | the emitter: a recording solver's choices rebuilt into a derivation with the checker's own rules, or a typed "not certifiable" |
//!
//! [`today`] is the rule a consumer applies: a certificate is valid today iff every question in
//! its list answers as recorded AND its derivation checks against today's facts.
//!
//! Everything here is sans-IO: bytes and facts in, a proof or a typed refusal out.

#![no_std]
#![feature(allocator_api)]
#![forbid(unsafe_code)]

#[cfg(test)]
extern crate std;

pub mod check;
pub mod derivation;
pub mod emit;
pub mod lean_auto;
pub mod lean_kernel;
pub mod questions;

#[cfg(test)]
mod emit_tests;
#[cfg(test)]
mod tests;

use blaze_solve::assembly::Clause;
use blaze_solve::facts::{Facts, Question};
use blaze_solve::tys::Tys;

pub use check::{CheckError, Checked, Checker, Cost};
pub use derivation::{BuildError, Builder, Builtin, By, FormatError, Step, StepId, View};
pub use emit::{Count, Emitter, NotCertifiable};
pub use lean_auto::{
    AutoConfig, AutoFailure, AutoOutcome, AutoProof, FailureReason, ProofStep, Strategy,
    auto_prove, auto_prove_and_register, auto_prove_portfolio,
};
pub use questions::{Questions, StandError};

/// What a certificate is today (the module doc's rule).
#[derive(Debug)]
pub enum Today<'d, 'a> {
    /// Every question stands and the derivation checks: the solver, run today, returns the same
    /// response; nothing is inferred.
    Valid(Checked<'d, 'a>),
    /// A question the choice rested on answers otherwise (an impl that could match was added,
    /// say): the certificate is void and the goal is solved again.
    Moved(Question),
    /// The questions stand but the derivation no longer checks (a content read moved: an impl's
    /// where-clauses, a field type): the goal is solved again.
    Refused(CheckError),
}

/// Why [`today`] could not say: a fact the world lacks, or a question that names terms of an
/// arena (not portable) and so cannot be asked of a world alone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TodayError {
    Stand(StandError),
}

/// The questions first (one digest compare each, no term walked), then the one linear check.
pub fn today<'d, 'a, T: Tys, F: Facts>(
    checker: &mut Checker<'a>,
    questions: &Questions<'_>,
    view: View<'d>,
    tys: &mut T,
    facts: &F,
    env: &[Clause],
) -> Result<Today<'d, 'a>, TodayError> {
    if let Some(q) = questions.moved(facts).map_err(TodayError::Stand)? {
        return Ok(Today::Moved(q));
    }
    Ok(match checker.check(view, tys, facts, env) {
        Ok(c) => Today::Valid(c),
        Err(e) => Today::Refused(e),
    })
}
