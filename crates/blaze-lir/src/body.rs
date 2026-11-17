//! Bodies by identity, and what a direct call reaches: the seam the evaluator follows calls
//! through ([`crate::eval::run_calls_in`], in tests) and the trivial inliner and the simplify
//! stage splice callees through ([`crate::inline`], [`crate::simplify`], in a build).
//!
//! Moved out of `eval/calls.rs` unchanged (rule 30: one concept, one home): the evaluator is
//! built only in tests and under its feature, and the inliner runs in every build.

use crate::{View, func::RefId};

/// A body the evaluator can run, by its identity: the content id of the function it is, or
/// any 32 bytes a test's [`Bodies`] chooses. Only `Bodies` reads it.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct BodyId(pub [u8; 32]);

/// What a call runs: the callee of reference `reference` in body `from`, with its identity,
/// or `None` for one the run does not have (refused as such).
pub trait Bodies {
    fn callee(&self, from: BodyId, reference: RefId) -> Option<(BodyId, View<'_>)>;
}
