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
//! The record keeps the candidate types for flow. A saved reference may need `T: Sync`
//! even when the candidate `T` satisfies `Send`. An obligation that needs the saved state is:
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
use core::mem::DropGuard;

use crate::body::{BodyView, Expr};
use crate::columns::Checked;
use crate::diag::{Diag, PendingWhyStep, UnmetGoal, Why, WhyDef, WhyReached, WhyStep};
use crate::error::{CheckError, Limit, Malformed, Stop};
use crate::ids::{ExprId, ItemKey};
use crate::infer::InferCtx;
use crate::instance::{Asked, ClosureSource, Keep, NoClosures, export_list_as, import, import_list};
use crate::portable::{PNode, PTy, PTys, PortableTys};
use crate::ty::{Cause, FulfillErrorKind, Goal, Obligation, Ty, TyView, TypeCtx};
use crate::machine::Plant;
use crate::world::{Needs, PClause, Question, ReadList, Reads, World};
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
    /// The solver left the goal open on the exact interior. `site` is the
    /// obligation's range. `storage` identifies a block and stored type when
    /// probing that type separately reproduced the ambiguity.
    /// `undecided` is the goal under it that is undecided by itself and why
    /// ([`crate::UndecidedLeaf`], the solver's undecided walk on the same goal). It is `None`
    /// when the walk found no undecided goal, or for the structural No on a local future that
    /// names no stored value.
    Ambiguous { trait_: ItemKey, site: crate::ids::Range, storage: Option<(ExprId, PTy)>, undecided: Option<crate::error::UndecidedLeaf> },
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
/// the check's (rule 21: front2 stalled_goals.rs). The findings carry no why-chain (`why`
/// is `None`): [`Checked::decide_stalled_with_storage`] gives it, in a table it extends.
pub fn decide_stalled_listed<'a, W: World>(
    body: &BodyView<'_>,
    checked: &Checked<'_>,
    types: PTys<'_>,
    exact: &[(ExprId, PTy)],
    world: &W,
    alloc: Alloc<'a>,
) -> Result<(Vec<Diag, Alloc<'a>>, ReadList<'a>), StalledError> {
    checked.decide_rows(body, types, exact, None, world, alloc, |_, _, _| {})
}

/// What [`Checked::decide_stalled_with_storage`] decided: the findings, the read list, and each
/// finding's why-chain over the exact interiors with the table its goals are in.
pub struct StalledDecision<'a> {
    /// The findings of the goals that fail on the exact interiors, in row order: each row's own
    /// E0277, its `why` (where the solver walked one) naming rows of [`StalledDecision::why`].
    pub found: Vec<Diag, Alloc<'a>>,
    /// What the decision asked the world ([`decide_stalled_listed`]).
    pub reads: ReadList<'a>,
    /// The why-chain column the findings' `Diag::why` name, as `Checked::why` is the check's:
    /// root first, each goal's types in [`StalledDecision::types`]. A `CoroutineSlot { slot }`
    /// step of a block of this body is element `slot` of the exact tuple the caller passed for
    /// the block (its sorted, deduplicated stored values).
    pub why: Vec<WhyStep, Alloc<'a>>,
    /// The caller's table read in place (every type of the findings and the exact interiors is
    /// at its own index), with the chain's goal types after it.
    pub types: PortableTys<'a>,
}

impl Checked<'_> {
    /// As [`decide_stalled_listed`], also reporting a stored type that independently
    /// fails a local future's auto trait (its portable type is in the exact table), and each
    /// finding's why-chain on the exact interiors ([`StalledDecision`]). `types` is a table of
    /// its own (no overlay: the record's table forked, then the exact interiors interned).
    pub fn decide_stalled_with_storage<'a, W: World>(
        &self, body: &BodyView<'_>, types: &'a PortableTys<'a>, exact: &[(ExprId, PTy)],
        world: &W, alloc: Alloc<'a>, failed_field: impl FnMut(Diag, ExprId, PTy),
    ) -> Result<StalledDecision<'a>, StalledError> {
        let mut table = PortableTys::over(types, alloc)?;
        let mut why = Vec::new_in(alloc);
        let chains = Chains { table: &mut table, why: &mut why };
        let (found, reads) = self.decide_rows(body, types.view(), exact, Some(chains), world, alloc, failed_field)?;
        Ok(StalledDecision { found, reads, why, types: table })
    }

    /// The decision, with the findings' chains into `chains` when given.
    #[allow(clippy::too_many_arguments)]
    fn decide_rows<'a, W: World>(
        &self, body: &BodyView<'_>, types: PTys<'_>, exact: &[(ExprId, PTy)], chains: Option<Chains<'_, '_, 'a>>,
        world: &W, alloc: Alloc<'a>, mut failed_field: impl FnMut(Diag, ExprId, PTy),
    ) -> Result<(Vec<Diag, Alloc<'a>>, ReadList<'a>), StalledError> {
        let mut reads = Reads::new_in(world, alloc);
        let mut fields = Vec::new_in(alloc);
        let result = decide(body, self, types, exact, &mut reads, alloc, &mut fields, chains);
        // Reads supplies stand-ins while a round runs. Its first missing answer
        // takes precedence over *every* decision made from those stand-ins.
        if let Some(question) = reads.first_hole() {
            return Err(StalledError::Need(question));
        }
        let found = result?;
        for (diag, block, field) in fields {
            failed_field(diag, block, field);
        }
        Ok((found, reads.finish(alloc).0))
    }
}

/// Where the decision writes its findings' why-chains: the column and the table its goals'
/// types are interned in (an overlay over the caller's).
struct Chains<'c, 't, 'a> {
    table: &'c mut PortableTys<'t>,
    why: &'c mut Vec<WhyStep, Alloc<'a>>,
}

#[allow(clippy::too_many_arguments)]
fn decide<'a, W: World>(
    body: &BodyView<'_>, checked: &Checked<'_>, types: PTys<'_>, exact: &[(ExprId, PTy)],
    reads: &mut Reads<'_, '_, W>, alloc: Alloc<'a>, fields: &mut Vec<(Diag, ExprId, PTy), Alloc<'a>>,
    mut chains: Option<Chains<'_, '_, 'a>>,
) -> Result<Vec<Diag, Alloc<'a>>, StalledError> {
    let mut out: Vec<Diag, Alloc<'a>> = Vec::new_in(alloc);
    if checked.stalled.is_empty() {
        return Ok(out);
    }
    let record = checked.types.view();
    for row in checked.stalled.iter() {
        let same = row.ty.0 < record.len() as u32 && record.node(row.ty)? == types.node(row.ty)?;
        if !same {
            return Err(StalledError::TableNotExtended { ty: row.ty });
        }
    }
    let exact = LocalWitnesses { checked, flow: exact };
    let mut cx = InferCtx::analysis_in(alloc);
    assume_where_clauses(&mut cx, reads, body.owner)?;
    for row in checked.stalled.iter() {
        let goal = {
            let mut src = Exact { asked: Asked { reads }, body, types, exact };
            match row.import_witness_goal(&mut cx, &mut src, record) {
                Ok(goal) => goal,
                Err(StalledError::Check(e)) => {
                    if let CheckError::Malformed(crate::error::Malformed::NoRecord(Question::Coroutine(owner, ordinal))) = e
                        && reads.coroutine_record(owner, ordinal)?.is_some_and(|f| f.is == crate::InteriorIs::Superset)
                    {
                        return Err(CheckError::ForeignWitness {
                            gap: crate::EXTERNAL_WITNESS_GAP, item: owner, ordinal: Some(ordinal), at: row.diag.at,
                        }.into());
                    }
                    return Err(e.at_coroutine_import(body.owner, row.diag.at).into());
                }
                Err(other) => return Err(other),
            }
        };
        cx.register(Obligation { goal, cause: Cause::Body })?;
        cx.select_all(reads)?;
        // Bounded: one obligation was registered, and each `next_error` removes one error.
        let mut failed = false;
        while let Some(err) = cx.next_error() {
            match err.kind {
                FulfillErrorKind::Unsatisfied | FulfillErrorKind::Mismatch => failed = true,
                FulfillErrorKind::Ambiguous => {
                    // Undecided because the solver stalled on another fn's opaque, at any depth
                    // (a revealed future's interior holds a third fn's future): the typed gap
                    // naming it, which the caller answers by revealing it and deciding again
                    // (front2 stalled_goals.rs `decide_stalled_goals`), never the witness gap.
                    if let Some(opaque) = cx.hidden_unknown(goal, &|_| false, reads)? {
                        return Err(StalledError::Check(CheckError::OpaqueHiddenUnknown { opaque, trait_: row.trait_ }));
                    }
                    let storage = row.unproved_storage(goal, types, exact, &mut cx, reads, FulfillErrorKind::Ambiguous)?;
                    let source = storage.map_or(row.ty, |(_, field)| field);
                    if let Some(why) = unavailable_witness(source, row.diag.at, types, exact, reads, alloc)? {
                        return Err(why);
                    }
                    // What it rests on (rule 19): the solver's undecided walk on the same goal,
                    // in a probe (nothing is bound). It is bounded by the solver's
                    // `MAX_WHY_ROWS` and `MAX_WHY_DEPTH`.
                    let mut steps: Vec<PendingWhyStep, Alloc<'_>> = Vec::new_in(alloc);
                    let undecided = cx.why_maybe_chain(goal, &mut steps, reads)?.map(|leaf| crate::error::UndecidedLeaf {
                        trait_: steps.iter().rev().find_map(|s| s.goal.map(|(t, _)| t)),
                        depth: u32::try_from(steps.len().saturating_sub(1)).unwrap_or(u32::MAX),
                        leaf,
                    });
                    return Err(StalledError::Ambiguous { trait_: row.trait_, site: row.diag.at, storage, undecided });
                }
            }
        }
        if failed {
            let stored = match row.unproved_storage(goal, types, exact, &mut cx, reads, FulfillErrorKind::Unsatisfied)? {
                Some((block, field)) => {
                    // The blamed value may be another fn's future read as its superset
                    // (`Exact::coroutine_of`): its `No` does not say what the state stores.
                    if let Some(why) = unavailable_witness(field, row.diag.at, types, exact, reads, alloc)? {
                        return Err(why);
                    }
                    Some((block, field))
                }
                None => {
                    if let Some(why) = unavailable_witness(row.ty, row.diag.at, types, exact, reads, alloc)? {
                        return Err(why);
                    }
                    if matches!(types.node(row.ty)?, PNode::Future(..))
                        && (reads.lang_item(crate::LangItem::Send)? == Some(row.trait_)
                            || reads.lang_item(crate::LangItem::Sync)? == Some(row.trait_))
                    {
                        // A structural No for a local future must have a non-Send/non-Sync
                        // capture or saved value. The root failure alone cannot identify one.
                        return Err(StalledError::Ambiguous { trait_: row.trait_, site: row.diag.at, storage: None, undecided: None });
                    }
                    unsize_storage(row, goal, body, types, exact, &mut cx, reads, alloc)?
                }
            };
            // The row's own goal walked by the solver on the exact interiors: root, the
            // interior's slot, the clauses under it, and why the last goal fails by itself.
            let diag = match chains.as_mut() {
                Some(c) => row.chained(goal, &mut cx, reads, exact, c)?,
                None => row.diag,
            };
            if let Some((block, field)) = stored {
                fields.push((diag, block, field));
            }
            out.push(diag);
        }
    }
    Ok(out)
}

impl Stalled {
    /// Isolate the stored type responsible for a negative or undecided
    /// structural auto trait. A definitive failure requires a known witness.
    fn unproved_storage<W: World>(
        &self, goal: Goal, types: PTys<'_>, exact: LocalWitnesses<'_, '_>, cx: &mut InferCtx<'_>, reads: &mut Reads<'_, '_, W>,
        kind: FulfillErrorKind,
    ) -> Result<Option<(ExprId, PTy)>, StalledError> {
        let PNode::Future(block, _, stored) = types.node(self.ty)? else { return Ok(None) };
        let Goal::Trait { trait_, args } = goal else { return Ok(None) };
        if cx.list_len(args) != 1 { return Ok(None); }
        let interior = exact.interior(block, stored, self.diag.at)?;
        let PNode::Tuple(list) = types.node(interior)? else { return Ok(None) };
        let TyView::Future(_, _, stored) = cx.view(cx.list_get(args, 0)) else { return Ok(None) };
        let TyView::Tuple(fields) = cx.view(stored) else { return Ok(None) };
        for (i, &field) in types.list(list)?.iter().enumerate() {
            let args = cx.mk_list(&[cx.list_get(fields, i as u32)])?;
            let goal = Goal::Trait { trait_, args };
            let matches = if kind == FulfillErrorKind::Ambiguous {
                ambiguous_interior_goal(cx, reads, Obligation { goal, cause: Cause::Body })?
            } else {
                !cx.may_hold(goal, reads)?
            };
            if matches && (kind != FulfillErrorKind::Unsatisfied
                || unavailable_witness(field, self.diag.at, types, exact, reads, cx.alloc())?.is_none())
            {
                return Ok(Some((block, field)));
            }
        }
        Ok(None)
    }

    /// The row's finding with its why-chain: the solver's walk of `goal` (`TypeCtx::why_chain`,
    /// in `cx`'s assumptions, the futures' interiors the exact ones), each step's goal written
    /// into the chain's table in the owner's terms, appended to its column. The finding as it
    /// is when the solver finds no `No` to explain (an ambiguity, a rule the walk does not
    /// see). A step's goal the table cannot hold (a variable the walk made, this body's
    /// closure) is `None`, as the check's are.
    fn chained<W: World>(
        &self, goal: Goal, cx: &mut InferCtx<'_>, reads: &mut Reads<'_, '_, W>, exact: LocalWitnesses<'_, '_>,
        c: &mut Chains<'_, '_, '_>,
    ) -> Result<Diag, StalledError> {
        let mut steps: Vec<PendingWhyStep, Alloc<'_>> = Vec::new_in(cx.alloc());
        let Some(leaf) = cx.why_chain(goal, &mut steps, reads)? else { return Ok(self.diag) };
        let limit = |_| CheckError::Limit(Limit::Steps);
        let start = u32::try_from(c.why.len()).map_err(limit)?;
        let len = u32::try_from(steps.len()).map_err(limit)?;
        for step in steps.iter() {
            let goal = match step.goal {
                Some((trait_, args)) => export_list_as(&mut cx.exporter(), args, c.table, Keep::Owner, 0)?
                    .map(|args| UnmetGoal::Trait { trait_, args }),
                None => None,
            };
            let reached = match step.reached {
                WhyReached::CoroutineSlot { coroutine: WhyDef::Expr(block), slot } => WhyReached::CoroutineSlot {
                    coroutine: WhyDef::Expr(block),
                    slot: exact.flow_slot(block, slot, goal, c.table.view())?,
                },
                r => r,
            };
            c.why.push(WhyStep { goal, reached });
        }
        let mut diag = self.diag;
        diag.why = Some(Why { start, len, leaf });
        Ok(diag)
    }
}

/// For a failed `Unsize` / `CoerceUnsized` row (`Pin<Box<{async}>>` to `Pin<Box<dyn Future<..> +
/// Send>>`, filed by rules/coerce_interior.rs `defer_local_unsizing`): the first value a local
/// future of the source stores, on its exact interior, that misses an auto trait the target's
/// objects name. `unproved_storage` asks only a unary auto-trait goal, so such a row's finding
/// named no stored value: tempo's zone_node rpc.rs:1180 was a bare "`Pin<Box<Future>>:
/// CoerceUnsized<..>`" (rule 19: the finding names the thing). `None` when no stored value of
/// a local future is the reason (the `Output` disagrees, or a foreign future's record is).
#[allow(clippy::too_many_arguments)]
fn unsize_storage<W: World>(
    row: &Stalled, goal: Goal, body: &BodyView<'_>, types: PTys<'_>, exact: LocalWitnesses<'_, '_>,
    cx: &mut InferCtx<'_>, reads: &mut Reads<'_, '_, W>, alloc: Alloc<'_>,
) -> Result<Option<(ExprId, PTy)>, StalledError> {
    let Goal::Trait { args, .. } = goal else { return Ok(None) };
    if cx.list_len(args) != 2 {
        return Ok(None);
    }
    // The auto traits of the target's objects, through ADT arguments and pointers (the paths
    // rules/coerce_nested.rs walks to the object).
    let mut autos: Vec<ItemKey, Alloc<'_>> = Vec::new_in(alloc);
    let mut work: Vec<Ty, Alloc<'_>> = Vec::new_in(alloc);
    work.push(cx.list_get(args, 1));
    let mut steps = 0u32;
    while let Some(t) = work.pop() {
        steps += 1;
        if steps > 65_536 {
            return Err(CheckError::Limit(Limit::Steps).into());
        }
        match cx.view(t) {
            TyView::Dynamic(rows) => {
                for i in 0..cx.list_len(rows) {
                    if let TyView::ExAuto(k) = cx.view(cx.list_get(rows, i)) {
                        autos.push(k);
                    }
                }
            }
            TyView::Adt(_, l) => {
                for i in 0..cx.list_len(l) {
                    work.push(cx.list_get(l, i));
                }
            }
            TyView::Ref(_, p) | TyView::Ptr(_, p) => work.push(p),
            _ => {}
        }
    }
    if autos.is_empty() {
        return Ok(None);
    }
    // The local futures of the source, by the record's nodes; another fn's future is its
    // record's, not a value this body stores.
    let mut nodes: Vec<PTy, Alloc<'_>> = Vec::new_in(alloc);
    nodes.push(row.ty);
    steps = 0;
    while let Some(t) = nodes.pop() {
        steps += 1;
        if steps > 65_536 {
            return Err(CheckError::Limit(Limit::Steps).into());
        }
        match types.node(t)? {
            PNode::Future(block, _, stored) => {
                let interior = exact.interior(block, stored, row.diag.at)?;
                let PNode::Tuple(list) = types.node(interior)? else { continue };
                for &field in types.list(list)?.iter() {
                    let ty = {
                        let mut src = Exact { asked: Asked { reads }, body, types, exact };
                        import(cx, &mut src, types, field, &[], 0)?
                    };
                    for &k in autos.iter() {
                        let one = cx.mk_list(&[ty])?;
                        if !cx.may_hold(Goal::Trait { trait_: k, args: one }, reads)? {
                            return Ok(Some((block, field)));
                        }
                    }
                }
            }
            PNode::Opaque(..) | PNode::Coroutine { .. } | PNode::FnDef(..) | PNode::FnPtr(..) | PNode::Dynamic(_) => {}
            n => crate::machine::children(n, types, &mut nodes)?,
        }
    }
    Ok(None)
}

/// Test just this stored value's obligation. The caller may be reporting other
/// errors already. Rolling back restores its obligations and its error cursor.
pub(crate) fn ambiguous_interior_goal<W: World, C: TypeCtx>(
    ty: &mut C, reads: &mut Reads<'_, '_, W>, obligation: Obligation,
) -> Result<bool, Stop> {
    let snapshot = ty.snapshot();
    let mut probe = DropGuard::new((ty, snapshot), |(ty, snap)| ty.rollback_to(snap));
    while probe.0.next_error().is_some() {}
    probe.0.register(obligation)?;
    probe.0.select_all(reads)?;
    while let Some(error) = probe.0.next_error() {
        if error.obligation == obligation && error.kind == FulfillErrorKind::Ambiguous {
            return Ok(true);
        }
    }
    Ok(false)
}

/// An opaque may still lack its defining body; a revealed one, or a nominal coroutine, may
/// reach another fn's record that is only a superset ([`crate::InteriorIs::Superset`]), which
/// decides no failure (`EXTERNAL_WITNESS_GAP`). For a local future inspect only stored fields.
fn unavailable_witness<W: World>(
    ty: PTy, at: crate::ids::Range, types: PTys<'_>, exact: LocalWitnesses<'_, '_>, reads: &mut Reads<'_, '_, W>, alloc: Alloc<'_>,
) -> Result<Option<StalledError>, StalledError> {
    let mut work = Vec::new_in(alloc);
    work.push(ty);
    let mut steps = 0;
    while let Some(t) = work.pop() {
        steps += 1;
        if steps > 65_536 {
            return Err(CheckError::Limit(Limit::Steps).into());
        }
        match types.node(t)? {
            PNode::Opaque(k, _) => match reads.opaque_record(k)? {
                None => {
                    let future = reads.lang_item(crate::LangItem::Future)?;
                    let bounds = reads.predicates(k)?;
                    let is_future = bounds.clauses.iter().any(|c| matches!(c,
                        PClause::Trait { trait_, .. } if Some(*trait_) == future));
                    // Another fn's future whose hidden type has no record here: that opaque,
                    // named (rule 19).
                    let gap = if is_future {
                        CheckError::ForeignWitness { gap: crate::rules::async_fn::EXTERNAL_WITNESS_GAP, item: k, ordinal: None, at }
                    } else {
                        CheckError::FutureWitness { reason: crate::INTERIOR_GAP, at }
                    };
                    return Ok(Some(StalledError::Check(gap)));
                }
                Some(h) => {
                    use crate::rules::async_fn::{SupersetFrom, superset_record};
                    if let Some((item, ordinal)) = superset_record(reads, SupersetFrom::Type(h.ty, h.types), alloc)? {
                        let gap = crate::rules::async_fn::EXTERNAL_WITNESS_GAP;
                        return Ok(Some(StalledError::Check(CheckError::ForeignWitness { gap, item, ordinal: Some(ordinal), at })));
                    }
                }
            },
            PNode::Coroutine { owner, ordinal, .. } => {
                use crate::rules::async_fn::{SupersetFrom, superset_record};
                if let Some((item, first)) = superset_record(reads, SupersetFrom::Record(owner, ordinal), alloc)? {
                    let gap = crate::rules::async_fn::EXTERNAL_WITNESS_GAP;
                    return Ok(Some(StalledError::Check(CheckError::ForeignWitness { gap, item, ordinal: Some(first), at })));
                }
            }
            PNode::Future(e, _, stored) => match exact.interior(e, stored, at) {
                Ok(interior) => work.push(interior),
                Err(why) => return Ok(Some(why)),
            },
            PNode::FnDef(..) | PNode::FnPtr(..) | PNode::Dynamic(_) => {}
            n => crate::machine::children(n, types, &mut work)?,
        }
    }
    Ok(None)
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
            PClause::ForFields { container, trait_, args } => Goal::ForFields {
                container: import(cx, &mut NoClosures, preds.types, container, &[], 0)?,
                trait_,
                args: import_list(cx, &mut NoClosures, preds.types, args, &[], 0)?,
            },
            // Outlives clauses are the borrow checker's (D148); the checker takes them as true.
            PClause::TypeOutlives { .. } | PClause::RegionOutlives { .. } => continue,
        };
        cx.assume(goal)?;
    }
    Ok(())
}

/// Both selection and its diagnostic must inspect the same state. The type
/// table from flow extends the checked table, so both answers are handles in it.
#[derive(Clone, Copy)]
struct LocalWitnesses<'s, 'c> {
    checked: &'s Checked<'c>,
    flow: &'s [(ExprId, PTy)],
}

impl LocalWitnesses<'_, '_> {
    fn interior(self, block: ExprId, stored: PTy, at: crate::ids::Range) -> Result<PTy, StalledError> {
        match self.checked.coroutine_witness(block) {
            Some(crate::CoroutineWitness::Complete) => Ok(stored),
            Some(crate::CoroutineWitness::NeedsSavedLocals) => self.flow.iter()
                .find_map(|&(e, p)| (e == block).then_some(p)).ok_or(StalledError::NoExactInterior { block }),
            Some(crate::CoroutineWitness::NeedsCapturePlaces) => Err(CheckError::FutureWitness {
                reason: "the coroutine witness needs the types and borrow modes of its captured places", at,
            }.into()),
            None => Err(StalledError::NoExactInterior { block }),
        }
    }

    /// Slot `slot` of block `block`'s interior as the decision walked it ([`Self::interior`]),
    /// numbered as the caller's exact tuple for the block (`flow`: its stored values' types,
    /// sorted and deduplicated, front2 stalled_goals.rs `storage_tuple`). The same slot where
    /// the walked interior IS that tuple (`NeedsSavedLocals`), or where the caller passed none
    /// for the block (no tuple of its to number). A `Complete` block's walked interior is the
    /// record's tuple, ordered by the check's own type handles: its slot is renumbered by its
    /// type (the step's goal's self type, `goal_types` its table), refused when that type is
    /// no element of the caller's tuple or the goal was not written.
    fn flow_slot(self, block: ExprId, slot: u32, goal: Option<UnmetGoal>, goal_types: PTys<'_>) -> Result<u32, StalledError> {
        if self.checked.coroutine_witness(block) == Some(crate::CoroutineWitness::NeedsSavedLocals) {
            return Ok(slot);
        }
        let Some(&(_, flow)) = self.flow.iter().find(|&&(e, _)| e == block) else { return Ok(slot) };
        let unheld = || StalledError::Check(CheckError::Malformed(Malformed::WhySlotUnheld { block, slot }));
        let Some(UnmetGoal::Trait { args, .. }) = goal else { return Err(unheld()) };
        let Some(&self_ty) = goal_types.list(args)?.first() else { return Err(unheld()) };
        let PNode::Tuple(list) = goal_types.node(flow)? else { return Err(unheld()) };
        match goal_types.list(list)?.iter().position(|&t| t == self_ty) {
            Some(i) => u32::try_from(i).map_err(|_| CheckError::Limit(Limit::Steps).into()),
            None => Err(unheld()),
        }
    }
}

/// The import source of a stalled type: a future of this body takes its exact interior; a
/// closure of this body its signature from the record; anything else is asked of the world.
struct Exact<'s, 'r, 'w, 'a, 'b, 'c, 'p, W: World> {
    asked: Asked<'r, 'w, 'a, W>,
    body: &'s BodyView<'b>,
    types: PTys<'p>,
    exact: LocalWitnesses<'s, 'c>,
}

impl<W: World> ClosureSource for Exact<'_, '_, '_, '_, '_, '_, '_, W> {
    type Error = StalledError;

    fn closure_of(&mut self, cx: &mut InferCtx<'_>, owner: ItemKey, ordinal: u32, args: crate::ty::TyList, depth: u32) -> Result<Ty, StalledError> {
        if owner != self.body.owner {
            return self.asked.closure_of(cx, owner, ordinal, args, depth).map_err(Into::into);
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
        let Some(e) = at else { return Err(refused.into()) };
        let checked = self.exact.checked;
        let rows = checked.closures.column::<0>();
        let Some(row) = rows.iter().position(|&c| c == e) else { return Err(refused.into()) };
        let kind = checked.closures.column::<1>()[row];
        let sig = checked.closures.column::<2>()[row];
        let n = cx.list_len(args);
        let mut subst: Vec<Ty, Alloc<'_>> = Vec::with_capacity_in(n as usize, cx.alloc());
        for i in 0..n {
            subst.push(cx.list_get(args, i));
        }
        // An async closure's result is itself a future of this body.
        let record = checked.types.view();
        let sig = import(cx, self, record, sig, &subst, depth)?;
        let upvars = checked.closures.column::<3>()[row]
            .map(|p| import(cx, self, record, p, &subst, depth)).transpose()?;
        Ok(cx.imported_closure(owner, ordinal, args, sig, kind, upvars)?)
    }

    fn future_of(&mut self, cx: &mut InferCtx<'_>, types: PTys<'_>, e: ExprId, output: PTy, stored: PTy, subst: &[Ty], depth: u32) -> Result<Ty, StalledError> {
        let interior = self.exact.interior(e, stored, self.body.expr_range(e))?;
        let o = import(cx, self, types, output, subst, depth)?;
        let interior_types = self.types;
        let i = import(cx, self, interior_types, interior, subst, depth)?;
        Ok(cx.mk(TyView::Future(e, o, i))?)
    }

    /// Another fn's future held in this body's exact interior (`c.resolve().await` stores
    /// `resolve`'s future). Its record may be only its check's superset
    /// ([`crate::InteriorIs::Superset`]: its own flow was refused); that record is imported
    /// here as it is, because a `Yes` over a superset of what the state stores holds for the
    /// state, and every `No` or open answer over one is turned into the gap
    /// `EXTERNAL_WITNESS_GAP` before it is reported ([`unavailable_witness`], called on the
    /// row and on the stored field `decide` blames). Nested coroutines inside it are read
    /// exact only, through the world ([`Asked`]). The record is read through `Reads`, so its
    /// digest, superset tag included, is in this body's read list (rule 21).
    fn coroutine_of(&mut self, cx: &mut InferCtx<'_>, owner: ItemKey, ordinal: u32, args: crate::ty::TyList, depth: u32) -> Result<Ty, StalledError> {
        let q = Question::Coroutine(owner, ordinal);
        let facts = self.asked.reads.coroutine_record(owner, ordinal)?.ok_or(CheckError::Malformed(Malformed::NoRecord(q)))?;
        let n = cx.list_len(args);
        let mut subst: Vec<Ty, Alloc<'_>> = Vec::with_capacity_in(n as usize, cx.alloc());
        for i in 0..n {
            subst.push(cx.list_get(args, i));
        }
        let nested = |e: Stop| -> StalledError {
            match e {
                Stop::Error(CheckError::Malformed(Malformed::FutureOutsideOwner { block, reader })) => {
                    CheckError::Malformed(Malformed::NestedFutureIdentity { owner, ordinal, block, reader }).into()
                }
                other => other.into(),
            }
        };
        let output = import(cx, &mut self.asked, facts.types, facts.output, &subst, depth).map_err(nested)?;
        let interior = import(cx, &mut self.asked, facts.types, facts.interior, &subst, depth).map_err(nested)?;
        Ok(cx.imported_coroutine(owner, ordinal, args, output, interior)?)
    }
}
