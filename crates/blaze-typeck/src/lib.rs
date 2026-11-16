//! blaze's type-check driver for one body, written as the ideal one (docs/v2/plan/design/typeck.md).
//!
//! [`check`] is a pure function of the body, the world's answers and the hints: it walks the
//! body with an explicit work stack in rustc's order of inference, asks the world only through
//! the sealed recorder (`world::Reads`), and returns a record in columns with its read list
//! ([`Outcome::Done`]), or the questions it could not have answered ([`Outcome::Need`]). The
//! type tables and the solver are behind [`TypeCtx`], which blaze-ty implements over
//! `rustc_type_ir` and the next solver; the tests implement it over a toy type language.
//!
//! Layout: the data (`ids`, `body`, `portable`, `ty`, `world`, `columns`, `diag`, `error`), the
//! machine (`machine`: frames, queues, coercion sites, lowering, autoderef), and one rule
//! module per family of nodes under `rules`, each naming the rustc file it follows.

#![no_std]
#![feature(allocator_api, register_tool, custom_inner_attributes)]
#![register_tool(blaze)]
#![forbid(unsafe_code)]

#[cfg(any(test, feature = "toy"))]
extern crate std;

mod body;
mod columns;
mod diag;
mod error;
mod hash;
mod ids;
mod intern;
mod machine;
mod portable;
mod rules;
mod ty;
mod world;

#[cfg(test)]
mod tests;
/// The toy type context: a hash-consed type table, union-find variables and a small solver.
/// Public under feature `toy` for the `blaze run` walking skeleton (crates/blaze-cli/tests/ladder/
/// README.md) until crates/blaze-ty implements [`TypeCtx`]; never the real checker's context.
#[cfg(any(test, feature = "toy"))]
pub mod toy;

use blaze_own::Alloc;

pub use body::{Arm, BinOp, BindAnn, BodyPath, BodyView, Expr, FieldInit, FieldPat, IntSuffix, Lit, Pat, Segment, Stmt, UnOp};
pub use columns::{AdjustKind, Adjusts, Callees, CaptureKind, Captures, Checked, Fields};
pub use diag::{Diag, DiagCode};
pub use error::{CheckError, Limit, Malformed, Stop};
pub use hash::{ContentHash, StableHasher};
pub use ids::{
    Edition, ExprId, FloatTy, IntTy, ItemKey, LangItem, List, Mutbl, Name, PatId, PathIdx, Range, ScopeId, StmtId,
    UintTy,
};
pub use portable::{PNode, PTy, PTys, PortableTys};
pub use ty::{Cause, FulfillError, FulfillErrorKind, Goal, Obligation, Ty, TyList, TyView, TypeCtx, Var, VarKind};
pub use world::{
    Adt, AdtKind, AssocItem, AssocKind, CtorKind, Def, Fetch, Field, FnSig, Hidden, ImplHeader, Missing, Needs,
    PClause, PathRes, Predicates, Question, ReadList, Reads, Seal, Shape, TypeAttrs, Variant, World,
};

use machine::{Machine, Plant};

/// What a check gives: the record, or the questions it needed and the world could not answer.
pub enum Outcome<'a> {
    Done(Checked<'a>),
    /// Every question asked before the stop and the one the world missed. The caller fetches
    /// them and checks again with these as the hints; that run equals one that never stopped.
    Need(Needs<'a>),
}

/// Checks one body (typeck.md, "The function").
///
/// `hints` is the previous record's `needs` (or empty): it goes to the world's `prefetch`
/// before the walk and changes nothing else. A user's type error is a diagnostic in the record;
/// `Err` is only a malformed body or a bound passed.
pub fn check<'a, W: World, C: TypeCtx>(
    body: &BodyView<'_>,
    world: &W,
    ty: &mut C,
    hints: &Needs<'_>,
    alloc: Alloc<'a>,
) -> Result<Outcome<'a>, CheckError> {
    check_with(body, world, ty, hints, Plant::None, alloc)
}

/// `check` with a planted bug for the read-list control; `Plant::None` outside tests.
pub(crate) fn check_with<'a, W: World, C: TypeCtx>(
    body: &BodyView<'_>,
    world: &W,
    ty: &mut C,
    hints: &Needs<'_>,
    plant: Plant,
    alloc: Alloc<'a>,
) -> Result<Outcome<'a>, CheckError> {
    let mut reads = Reads::new_in(world, alloc);
    reads.prefetch(&hints.questions);
    let mut m = Machine::new_in(*body, reads, ty, plant, alloc)?;
    match walk(&mut m) {
        Ok(()) => Ok(Outcome::Done(m.writeback()?)),
        Err(Stop::Need(q)) => Ok(Outcome::Need(m.reads.finish_need(q, alloc))),
        Err(Stop::Error(e)) => Err(e),
    }
}

/// The walk and the end sequence (typeck.md, "The walk and its queues").
fn walk<W: World, C: TypeCtx>(m: &mut Machine<'_, '_, '_, '_, W, C>) -> Result<(), Stop> {
    let output = m.start_body()?;
    m.run()?;
    m.finish_body(output)?;
    m.end_sequence()
}
