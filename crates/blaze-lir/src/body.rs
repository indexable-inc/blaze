//! Bodies by identity, and what a direct call reaches: the seam the evaluator follows calls
//! through ([`crate::eval::run_calls_in`], in tests) and the trivial inliner and the simplify
//! stage splice callees through ([`crate::inline`], [`crate::simplify`], in a build).
//!
//! Moved out of `eval/calls.rs` unchanged (rule 30: one concept, one home): the evaluator is
//! built only in tests and under its feature, and the inliner runs in every build.

use crate::{View, func::RefId};

/// The program's data as a run asks for it: the evaluator's `data_addr`, `tls_addr`,
/// `func_addr` and `call_indirect` (eval/data.rs).
mod data;
pub use data::{Datum, DatumId, DatumReloc, Target};

/// A body the evaluator can run, by its identity: the content id of the function it is, or
/// any 32 bytes a test's [`Bodies`] chooses. Only `Bodies` reads it.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct BodyId(pub [u8; 32]);

/// What a call runs: the callee of reference `reference` in body `from`, with its identity,
/// or `None` for one the run does not have (refused as such). The defaulted methods answer the
/// program's data and the bodies of functions called through pointers; a provider that does
/// not answer them has every run reaching data refused by name.
pub trait Bodies {
    fn callee(&self, from: BodyId, reference: RefId) -> Option<(BodyId, View<'_>)>;

    /// What reference `reference` of body `from` names when it is not the body's own data: a
    /// static, thread-local or vtable as a datum, a function by identity (an outside function
    /// too); `None` for one the run does not have.
    fn target(&self, _from: BodyId, _reference: RefId) -> Option<Target> {
        None
    }

    /// Datum `id`'s initial bytes, alignment, relocations and whether it may be written.
    fn datum(&self, _id: DatumId) -> Option<Datum<'_>> {
        None
    }

    /// The body of function `id`, for a call through a pointer to it; `None` for one the run
    /// does not have, as an outside function.
    fn body(&self, _id: BodyId) -> Option<View<'_>> {
        None
    }

    /// The line and the column (each from 1, the column in chars) of byte `site` of item
    /// `item`'s text where the program's source is now: what a data row's source position
    /// ([`crate::func::DataPositionRec`]) is filled with when the row is placed. `None` for a
    /// run that knows no source, whose rows with positions are refused by name.
    fn position(&self, _item: u128, _site: u32) -> Option<(u32, u32)> {
        None
    }
}
