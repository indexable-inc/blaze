//! The linear simplify stage (docs/v2/plan/design/backend-next.md, D100 point 1): blaze's own
//! LIR-to-LIR passes in front of the Fast tier, each one walk. After the trivial inliner has
//! spliced `next` or `deref` in, the `Option`, the range and the iterator still live in stack
//! slots; these passes put them in values, propagate copies and constants, and drop dead code,
//! so the Fast tier's code keeps them in registers. Each pass is validated as a tier is:
//! evaluator equality on generated programs, and a planted wrong rewrite caught.
//!
//! - [`sroa`]: scalar replacement of slots whose address does not escape.
//! - `prop`: copy and constant propagation, folding by the evaluator's arithmetic ([`propagate`]).
//! - `dce`: dead code and trivial branches ([`dead`]), and the rewrite every pass makes its output with.
//! - `stage`: [`simplify`], the inliner, `scalar_replace`, `propagate` and `dead`, once each.

use core::fmt;

use crate::{BuildError, VerifyError};

mod dce;
mod prop;
pub mod sroa;
mod stage;

pub use dce::{coalesce_cfg, dead};
pub use prop::{Plant, propagate, propagate_planted};
pub use sroa::scalar_replace;
pub use stage::simplify;

#[cfg(test)]
mod prop_tests;
#[cfg(test)]
mod sroa_tests;

/// Why a simplify pass made no function.
#[derive(Debug)]
pub enum SimplifyError {
    /// A builder the pass drove refused what it was given.
    Build(BuildError),
    /// The pass's output is not LIR: a bug of the pass, named by the verifier's rule.
    Verify(VerifyError),
    /// Instruction `inst` of the input is a form the pass does not take, or the output would
    /// not fit LIR's columns: `what` says which.
    Refused { inst: usize, what: &'static str },
}

impl fmt::Display for SimplifyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SimplifyError::Build(error) => write!(f, "the simplified function: {error}"),
            SimplifyError::Verify(error) => write!(f, "the simplified function is not LIR: {error}"),
            SimplifyError::Refused { inst, what } => {
                write!(f, "instruction {inst}: the simplify stage refuses {what}")
            }
        }
    }
}

impl core::error::Error for SimplifyError {}
