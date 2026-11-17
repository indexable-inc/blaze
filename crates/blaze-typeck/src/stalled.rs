//! Auto-trait goals on an async block's future, decided on its EXACT interior after the body's
//! flow (async plan P13, D-A12; port27/tempo-run/ASYNC.md).
//!
//! The rule: the Reference, "Auto traits": "`Send`, `Sync`, `Unpin`, `UnwindSafe`, and
//! `RefUnwindSafe` are auto traits ... a closure is `Send` if all of its captures are `Send`"
//! and, for an async block, its future's state is "roughly equivalent to an enum with one variant
//! per await point, where each variant stores the data needed to resume from its corresponding
//! point" ("Async blocks"): its auto traits are its interior's, its captures and every value held
//! across an `.await` (blaze-solve structural.rs: `Coroutine(_, [output, interior])` asks the
//! interior). Which values are held across an `.await` is the flow's answer (blaze-flow-lower
//! `interior`, P2), and the flow runs after the check, on the check's record.
//!
//! So the check keeps its SUPERSET interior (rules/coroutine.rs: every type the block names): a
//! goal that holds there holds on the exact one. A goal that FAILS there, and is not known to
//! fail on the exact one (rules/async_fn.rs `coroutine_interior_gap`), is:
//!
//! - under [`check`](crate::check) (and every entry that does not defer), the typed gap
//!   `INTERIOR_GAP`, as before: a caller that never decides it gets no answer, never a pass;
//! - under [`check_deferring`], a [`Stalled`] row of the record holding the E0277 finding the
//!   check would make there. The caller lowers each async block's flow, computes its exact
//!   interior, and calls [`decide_stalled`]: each row's goal re-solved with every future's
//!   interior replaced by its exact one; the finding is returned when it fails there.
//!
//! The decision is the solver's own (the same `register` and `select_all` a check makes), in the
//! owner's where-clauses as the check assumes them (rules/control.rs `start_body`).

use blaze_own::{Alloc, Vec};

use crate::body::{BodyView, Expr};
use crate::columns::Checked;
use crate::diag::Diag;
use crate::error::{CheckError, Limit, Stop};
use crate::ids::{ExprId, ItemKey};
use crate::infer::InferCtx;
use crate::instance::{Asked, ClosureSource, NoClosures, import, import_list};
use crate::portable::{PTy, PTys};
use crate::ty::{Cause, FulfillErrorKind, Goal, Obligation, Ty, TyView, TypeCtx};
use crate::machine::Plant;
use crate::world::{Needs, PClause, Question, Reads, World};
use crate::Outcome;

/// What the check does with an auto-trait goal of an async block's future that failed on the
/// superset interior and is not known to fail on the exact one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Interior {
    /// The typed gap `INTERIOR_GAP` (every entry but [`check_deferring`]).
    Gap,
    /// A [`Stalled`] row of the record, decided by [`decide_stalled`] after the flow.
    Defer,
}

/// A stalled goal while the walk runs: the goal's trait and self type as handles, and the E0277
/// finding the check would make, exported at writeback.
#[derive(Clone, Copy, Debug)]
pub(crate) struct PendingStalled {
    pub trait_: ItemKey,
    pub ty: Ty,
    pub diag: crate::diag::PendingDiag,
}

/// An auto-trait goal `ty: trait_` on a type holding an async block's future of this body, left
/// for the exact interior (the module doc).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stalled {
    /// The auto trait (`Send`, `Sync`, ..).
    pub trait_: ItemKey,
    /// The goal's self type, in the record's table, its futures' interiors the superset.
    pub ty: PTy,
    /// The finding when the goal fails on the exact interior: E0277 at the obligation's site,
    /// the goal entire, as the check makes it from an unmet goal.
    pub diag: Diag,
}

/// Why stalled goals could not be decided. Each names the thing it is about.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StalledError {
    /// A future in a stalled goal's type has no exact interior among those passed: the async
    /// block at `block` was not lowered as its own flow.
    NoExactInterior { block: ExprId },
    /// The table passed does not hold the record's table as its prefix: the stalled type `ty`
    /// is not the same node in both (P2's `interior` extends a fork of the record's table).
    TableNotExtended { ty: PTy },
    /// The goal on the exact interior is neither met nor unmet (a variable or an alias the
    /// solver left open): refused by name, never taken as met. `site` is the finding's place.
    Ambiguous { trait_: ItemKey, site: crate::ids::Range },
    /// The world had no answer to `Question` (a closure's facts, the owner's predicates).
    Need(Question),
    /// The check's own error while importing or solving.
    Check(CheckError),
}

impl From<CheckError> for StalledError {
    fn from(e: CheckError) -> StalledError {
        StalledError::Check(e)
    }
}

impl From<Stop> for StalledError {
    fn from(s: Stop) -> StalledError {
        match s {
            Stop::Need(q) => StalledError::Need(q),
            Stop::Error(e) => StalledError::Check(e),
        }
    }
}

/// [`check`](crate::check), with each auto-trait goal of an async block's future that failed
/// only on the superset interior filed as a [`Stalled`] row (`Checked::stalled`) instead of the
/// typed gap. The caller MUST pass every row to [`decide_stalled`] with the exact interiors
/// before the record's findings are whole: a row is an undecided goal, not a pass.
pub fn check_deferring<'a, W: World, C: TypeCtx>(
    body: &BodyView<'_>,
    world: &W,
    ty: &mut C,
    hints: &Needs<'_>,
    alloc: Alloc<'a>,
) -> Result<Outcome<'a>, CheckError> {
    let reads = Reads::new_in(world, alloc);
    crate::run(body, reads, ty, hints, Plant::None, Interior::Defer, alloc).map(|(o, _)| o)
}

/// Decides each stalled goal of `checked` on the exact interiors: `exact` holds, per async
/// block of the body lowered as its own flow, its exact interior (blaze-flow-lower
/// `interior`), both of `types`, a table holding `checked.types` as its prefix (the table
/// `interior` extended). Returns the findings of the goals that fail there, in row order: each
/// row's own E0277. A goal that holds adds nothing.
pub fn decide_stalled<'a, W: World>(
    body: &BodyView<'_>,
    checked: &Checked<'_>,
    types: PTys<'_>,
    exact: &[(ExprId, PTy)],
    world: &W,
    alloc: Alloc<'a>,
) -> Result<Vec<Diag, Alloc<'a>>, StalledError> {
    decide_stalled_listed(body, checked, types, exact, world, alloc).map(|(found, _)| found)
}

/// [`decide_stalled`], with the read list of the decision: what it asked the world (the
/// owner's predicates, each auto trait's impls for the exact interiors' types, which the check
/// on the superset may never have asked). A caller filing the verdict files this list with
/// the check's (rule 21: front2 stalled_goals.rs).
pub fn decide_stalled_listed<'a, W: World>(
    body: &BodyView<'_>,
    checked: &Checked<'_>,
    types: PTys<'_>,
    exact: &[(ExprId, PTy)],
    world: &W,
    alloc: Alloc<'a>,
) -> Result<(Vec<Diag, Alloc<'a>>, crate::world::ReadList<'a>), StalledError> {
    let mut out: Vec<Diag, Alloc<'a>> = Vec::new_in(alloc);
    let mut reads = Reads::new_in(world, alloc);
    if checked.stalled.is_empty() {
        return Ok((out, reads.finish(alloc).0));
    }
    let record = checked.types.view();
    for row in checked.stalled.iter() {
        let same = row.ty.0 < record.len() as u32 && record.node(row.ty)? == types.node(row.ty)?;
        if !same {
            return Err(StalledError::TableNotExtended { ty: row.ty });
        }
    }
    let mut cx = InferCtx::new_in(alloc);
    assume_where_clauses(&mut cx, &mut reads, body.owner)?;
    for row in checked.stalled.iter() {
        let t = {
            let mut src = Exact { asked: Asked { reads: &mut reads, need: None }, body, checked, types, exact, missing: None };
            match import(&mut cx, &mut src, types, row.ty, &[], 0) {
                Ok(t) => t,
                Err(e) => {
                    if let Some(block) = src.missing {
                        return Err(StalledError::NoExactInterior { block });
                    }
                    if let Some(q) = src.asked.need {
                        return Err(StalledError::Need(q));
                    }
                    return Err(StalledError::Check(e));
                }
            }
        };
        let args = cx.mk_list(&[t])?;
        let goal = Goal::Trait { trait_: row.trait_, args };
        cx.register(Obligation { goal, cause: Cause::Body })?;
        cx.select_all(&mut reads)?;
        // Bounded: one obligation was registered, and each `next_error` removes one error.
        let mut failed = false;
        while let Some(err) = cx.next_error() {
            match err.kind {
                FulfillErrorKind::Unsatisfied | FulfillErrorKind::Mismatch => failed = true,
                FulfillErrorKind::Ambiguous => {
                    return Err(StalledError::Ambiguous { trait_: row.trait_, site: row.diag.at });
                }
            }
        }
        if failed {
            out.push(row.diag);
        }
    }
    Ok((out, reads.finish(alloc).0))
}

/// The owner's where-clauses as assumptions of `cx`, as the check's `start_body` makes them:
/// each clause's types imported with the owner's parameters left as parameters.
fn assume_where_clauses<W: World>(cx: &mut InferCtx<'_>, reads: &mut Reads<'_, '_, W>, owner: ItemKey) -> Result<(), StalledError> {
    let preds = reads.predicates(owner)?;
    for clause in preds.clauses {
        let goal = match *clause {
            PClause::Trait { trait_, args } => Goal::Trait { trait_, args: import_list(cx, &mut NoClosures, preds.types, args, &[], 0)? },
            PClause::Projection { trait_, args, name, term } => Goal::Projection {
                trait_,
                args: import_list(cx, &mut NoClosures, preds.types, args, &[], 0)?,
                name,
                term: import(cx, &mut NoClosures, preds.types, term, &[], 0)?,
            },
            PClause::Const { cond, args } => Goal::ConstHolds { cond, args: import_list(cx, &mut NoClosures, preds.types, args, &[], 0)? },
        };
        cx.assume(goal)?;
    }
    Ok(())
}

/// The import source of a stalled type: a future of this body takes its exact interior; a
/// closure of this body its signature from the record; anything else is asked of the world.
struct Exact<'s, 'r, 'w, 'a, 'b, 'c, 'p, W: World> {
    asked: Asked<'r, 'w, 'a, W>,
    body: &'s BodyView<'b>,
    checked: &'s Checked<'c>,
    types: PTys<'p>,
    exact: &'s [(ExprId, PTy)],
    /// The block whose exact interior was not passed, when that stopped the import.
    missing: Option<ExprId>,
}

impl<W: World> ClosureSource for Exact<'_, '_, '_, '_, '_, '_, '_, W> {
    fn closure_of(&mut self, cx: &mut InferCtx<'_>, owner: ItemKey, ordinal: u32, args: crate::ty::TyList, depth: u32) -> Result<Ty, CheckError> {
        if owner != self.body.owner {
            return self.asked.closure_of(cx, owner, ordinal, args, depth);
        }
        // The `ordinal`-th closure expression of this body (rules/writeback.rs
        // `closure_ordinal`), and its row of `Checked::closures`.
        let mut seen = 0u32;
        let mut at: Option<ExprId> = None;
        for (i, x) in self.body.exprs.iter().enumerate() {
            if matches!(x, Expr::Closure { .. }) {
                if seen == ordinal {
                    at = Some(ExprId(i as u32));
                    break;
                }
                seen += 1;
            }
        }
        let refused = CheckError::Malformed(crate::error::Malformed::BodyTypeInRecord(Question::Closure(owner, ordinal)));
        let Some(e) = at else { return Err(refused) };
        let rows = self.checked.closures.column::<0>();
        let Some(row) = rows.iter().position(|&c| c == e) else { return Err(refused) };
        let kind = self.checked.closures.column::<1>()[row];
        let sig = self.checked.closures.column::<2>()[row];
        let n = cx.list_len(args);
        let mut subst: Vec<Ty, Alloc<'_>> = Vec::with_capacity_in(n as usize, cx.alloc());
        for i in 0..n {
            subst.push(cx.list_get(args, i));
        }
        let sig = import(cx, &mut NoClosures, self.types, sig, &subst, depth)?;
        cx.mk(TyView::ClosureOf { owner, ordinal, args, sig, kind })
    }

    fn future_of(&mut self, cx: &mut InferCtx<'_>, types: PTys<'_>, e: ExprId, output: PTy, _superset: PTy, subst: &[Ty], depth: u32) -> Result<Ty, CheckError> {
        let Some(&(_, interior)) = self.exact.iter().find(|&&(b, _)| b == e) else {
            self.missing = Some(e);
            return Err(CheckError::Limit(Limit::Export));
        };
        let o = import(cx, self, types, output, subst, depth)?;
        let i = import(cx, self, types, interior, subst, depth)?;
        cx.mk(TyView::Future(e, o, i))
    }
}
