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

#[cfg(any(test, feature = "testing"))]
extern crate std;

mod asks;
mod body;
mod columns;
mod diag;
/// Dyn compatibility: the rule's one home (facts in, verdict out).
pub mod dyn_compat;
mod error;
mod filed;
mod hash;
mod ids;
/// The real type context over blaze-ty and blaze-solve.
pub mod infer;
mod instance;
mod intern;
mod machine;
mod portable;
/// A kept check's read list asked again of today's world (read-addressed.md, "End state").
mod replay;
mod rules;
mod ty;
mod world;

#[cfg(test)]
mod tests;
#[cfg(test)]
mod overlay_tests;
#[cfg(test)]
mod suite;
/// A world and bodies written by hand, for the body suite and `examples/cost.rs`.
#[cfg(any(test, feature = "testing"))]
pub mod fixture;
/// The tests' toy world (the type context is the real one, [`InferCtx`]).
#[cfg(test)]
mod toy;

use blaze_own::Alloc;

pub use infer::{InferCtx, InferSnapshot};
pub use replay::{Replay, replay};
pub use instance::{ResolveError, Resolved, normalize_projection, resolve_instance, select_impl};
pub use asks::{Ask, AskLog, Clock, Order, QUESTION_KINDS, R_THRESHOLD, Restarts, Solve, Tally, entries_histogram, question_kind, restarts};
pub use body::{
    Anchor, Arm, BinOp, BindAnn, BodyPath, BodyView, Capture, CoroutineKind, Expr, FieldInit, FieldPat, FmtTrait, FmtUse,
    IntSuffix, Lit, Pat, Segment, Stmt, UnOp,
};
pub use columns::{AdjustKind, Adjusts, Callees, CaptureKind, Captures, Checked, Closures, ConstArgs, ExprDefs, Fields, PatDefs};
pub use diag::{Because, Diag, DiagCode, UnmetGoal};
/// The fix offered for a name that does not resolve: `Checked::suggests` and `suggest::pick`.
pub use rules::suggest;
pub use error::{CheckError, Limit, Malformed, Recorded, Stop};
pub use filed::Filed;
pub use hash::{ContentHash, StableHasher};
pub use ids::{
    Abi, Edition, ExprId, FloatTy, FnPtrHeader, IntTy, ItemKey, LangItem, List, Mutbl, Name, PatId, PathIdx, Range,
    Safety, ScopeId, StmtId, UintTy,
};
pub use portable::{MAX_OBJECT_PREDS, MAX_PATTERN_RANGES, OwnTys, PNode, PTy, PTys, PortableTys, canonical_ranges, object_principal};
pub use ty::{Cause, ClosureKind, FulfillError, FulfillErrorKind, Goal, Obligation, Ty, TyList, TyView, TypeCtx, Var, VarKind};
pub use world::{
    Adt, AdtKind, ArgsAfterSelf, SplitSelf, AssocGenerics, AssocItem, AssocKind, AssocParent, AssocTyDef, ClosureFacts, ConstHolds, ConstParams, CtorKind, Def, Fetch, Field, FnSig,
    Hidden, ImplHeader, ImplPolarity, Missing, Needs, PClause, PathRes, Predicates, Prim, Question, ReadList, Reads, RecordKind, Seal, Shape,
    TraitDef, TraitFlags, TraitMembers, TypeAttrs, DiscrRepr, Variant, World, ground_args_digest,
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
    let reads = Reads::new_in(world, alloc);
    run(body, reads, ty, hints, plant, alloc).map(|(o, _)| o)
}

/// `check` that also writes the ask log (asks.rs), reading `clock`: the measurement that
/// decides whether re-entry from the top stays. The outcome equals `check`'s.
pub fn check_logged<'a, 'w, W: World, C: TypeCtx>(
    body: &BodyView<'_>,
    world: &'w W,
    ty: &mut C,
    hints: &Needs<'_>,
    clock: &'w dyn Clock,
    alloc: Alloc<'a>,
) -> Result<(Outcome<'a>, AskLog<'a>), CheckError> {
    let reads = Reads::logged_in(world, clock, alloc);
    let (o, log) = run(body, reads, ty, hints, Plant::None, alloc)?;
    let log = log.ok_or(CheckError::Malformed(Malformed::HoleInAnswer))?;
    Ok((o, log))
}

/// `check` that also adds what it asked and what the answers cost into `tally` (asks.rs
/// `Tally`), reading `clock`. The outcome equals `check`'s.
pub fn check_counted<'a, 'w, W: World, C: TypeCtx>(
    body: &BodyView<'_>,
    world: &'w W,
    ty: &mut C,
    hints: &Needs<'_>,
    tally: &'w mut Tally,
    clock: &'w dyn Clock,
    alloc: Alloc<'a>,
) -> Result<Outcome<'a>, CheckError> {
    let start = clock.now_ns();
    let mut mine = Tally::default();
    let out = {
        let reads = Reads::counted_in(world, &mut mine, clock, alloc);
        run(body, reads, ty, hints, Plant::None, alloc).map(|(o, _)| o)
    };
    mine.checks = 1;
    mine.check_ns = clock.now_ns().saturating_sub(start);
    tally.add(&mine);
    out
}

fn run<'a, 'w, W: World, C: TypeCtx>(
    body: &BodyView<'_>,
    mut reads: Reads<'w, 'a, W>,
    ty: &mut C,
    hints: &Needs<'_>,
    plant: Plant,
    alloc: Alloc<'a>,
) -> Result<(Outcome<'a>, Option<AskLog<'a>>), CheckError> {
    if hints.questions.is_empty() {
        // A body with no record: one round of syntactic prefetches before the walk.
        reads.prefetch_syntactic(body, alloc);
    } else {
        reads.prefetch(&hints.questions);
    }
    let mut m = Machine::new_in(*body, reads, ty, plant, alloc)?;
    let walked = walk(&mut m);
    let log = m.reads.take_log();
    // A run that met a hole makes no result: not its types, not its diagnostics, not an
    // error met past the hole (world.rs `Hole`). Only a run that met none answers.
    match walked {
        Ok(()) if !m.reads.holed() => Ok((Outcome::Done(m.writeback()?), log)),
        Ok(()) | Err(Stop::Error(_)) if m.reads.holed() => Ok((Outcome::Need(m.reads.finish_need(None, alloc)), log)),
        Err(Stop::Need(q)) => Ok((Outcome::Need(m.reads.finish_need(Some(q), alloc)), log)),
        Ok(()) => Err(CheckError::Malformed(Malformed::HoleInAnswer)),
        Err(Stop::Error(e)) => Err(e),
    }
}

/// `check` with a planted bug of the hole tests (world.rs `HolePlant`).
#[cfg(test)]
pub(crate) fn check_hole_plant<'a, W: World, C: TypeCtx>(
    body: &BodyView<'_>,
    world: &W,
    ty: &mut C,
    hints: &Needs<'_>,
    plant: world::HolePlant,
    alloc: Alloc<'a>,
) -> Result<Outcome<'a>, CheckError> {
    let mut reads = Reads::new_in(world, alloc);
    reads.hole_plant = plant;
    run(body, reads, ty, hints, Plant::None, alloc).map(|(o, _)| o)
}

/// The walk and the end sequence (typeck.md, "The walk and its queues").
fn walk<W: World, C: TypeCtx>(m: &mut Machine<'_, '_, '_, '_, W, C>) -> Result<(), Stop> {
    let output = m.start_body()?;
    m.run()?;
    m.finish_body(output)?;
    m.end_sequence()
}
