//! The auto-trait goals of an async block's future that the type check left stalled, decided
//! on each block's EXACT interior after the block's body is lowered (async plan P13's driver;
//! port27/tempo-run/ASYNC.md; blaze-typeck stalled.rs).
//!
//! The rule: the Reference, "Auto traits": an auto trait holds for a type when it holds for
//! everything the type holds; "Async blocks": an async block's future is "roughly equivalent to
//! an enum with one variant per await point, where each variant stores the data needed to
//! resume from its corresponding point". So `async { let r = Rc::new(1); f().await; drop(r) }`
//! is not `Send` (`r` is held across the `.await`: the library's alloc/src/rc.rs `impl<T:
//! ?Sized, A: Allocator> !Send for Rc<T, A>`), and with `drop(r)` before the `.await` it is.
//!
//! What a value held across an `.await` is, is the flow's answer (blaze-flow-lower `interior`),
//! and the flow is lowered from the check's own record. So `blaze check`'s path for one body
//! ([`super::project::ProjectWorld::check_borrows`]) is:
//!
//! 1. the type check by `blaze_typeck::check_deferring` ([`Stalls::Decide`]): a goal that fails
//!    only on the check's superset interior is a `Checked::stalled` row, not the typed gap;
//! 2. the move and borrow check (front2 borrows.rs `borrows_of`), which, when a goal stalled,
//!    first lowers each async block's WITNESS ([`AsyncInteriors::witness_all`], blaze-flow-lower
//!    `lower_async_witness`: no region related, so no region refusal) for its exact interior,
//!    then lowers each body for the borrow check (`lower_async`). Two lowerings of a block, only
//!    when a goal stalled: the witness must exist where the borrow lowering refuses a region
//!    (typeck.md "Witness records", W1);
//! 3. [`decide_stalled_goals`]: `blaze_typeck::decide_stalled_listed` re-solves each row on the
//!    exact interiors; each goal that fails there is an E0277 filed among the body's type
//!    diagnostics exactly as the check files its own (`BodyDiagnostic`, its words from
//!    check.rs `diag_about`), and a goal that holds there leaves nothing.
//!
//! A row whose future's body was not lowered (the lowering refused it, or the borrow check
//! stopped before it) gives `CheckError::FutureWitness` at its item-relative range,
//! never a pass (rule 3). Every other caller of the check ([`Stalls::Gap`]) keeps the gap.
//!
//! KEYS (rule 21). The verdict now reads, beyond the type check's list:
//! - the async block's lowering and its interior: a function of the body's own text (the body's
//!   key), its `Checked` record (made from the type check's read list, already the body's) and
//!   the borrow facts the lowering asks through `NestedFacts` (the same `BorrowFacts`, whose
//!   every fact is a `BorrowAdt`/`BorrowHead` row of the list: front2 borrow_reads.rs);
//! - what the decision asks the world (the owner's predicates, the impls of each auto trait
//!   for each type of the exact interior, e.g. `T: Sync` for a captured `&T`, which the
//!   superset check may never have asked): `decide_stalled_listed`'s own read list, made into
//!   rows by reads.rs `facts_with_buckets` exactly as the check's list is, and pushed into the
//!   borrow rows that `check_borrows` files with `file_borrow_reads` (one row per fact; a fact
//!   with two answers refuses the list by name).

use std::alloc::Global;
use std::vec::Vec;

use blaze_flow_lower::identity::{Identity, Role};
use blaze_flow_lower::{AsyncWitness, lower_async_witness};
use blaze_own::{AString, AVec, Alloc};
use blaze_typeck as tc;

use super::body::Body;
use super::borrows_nested::NestedFacts;
use super::check::{CheckFail, HeldSlot, MOST_ROUNDS, Records, make_all, world_at};
use super::keys::checker_of;
use super::project::{BodyDiagnostic, BodyVerdict, DiagNote, NoteKind, Refused};
use super::reads::{Read, ReadsError, facts_with_buckets};
use super::source::NamesSource;
use super::spell::Spelling;
use super::world_items::Gap;

/// The decision met a goal the solver leaves open on the exact interior (a variable, an alias
/// it could not normalize): not built, refused by name, never taken as met.
pub const STALLED_AMBIGUOUS: &str = "an auto trait (`Send`, `Sync`) of an async block's future that the solver leaves open on its exact interior";

/// The exact interiors' table does not hold the record's table as its prefix: a bug of this
/// driver (it forks the record's own table), refused by name.
pub const STALLED_TABLE: &str = "an auto trait of an async block's future decided over a type table that does not extend the body's record";

/// Which entry of the type check a body's check takes for an auto-trait goal of an async
/// block's future that fails only on the check's superset interior.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stalls {
    /// `blaze_typeck::check`: the typed gap `INTERIOR_GAP` (every caller that lowers no async
    /// body: `ProjectWorld::check`, the census, the counted checks).
    Gap,
    /// `blaze_typeck::check_deferring`: a `Checked::stalled` row, decided by
    /// [`decide_stalled_goals`] after the async bodies are lowered. Only
    /// `ProjectWorld::check_borrows` asks it, and it decides every row.
    Decide,
}

/// The exact interiors of a body's async blocks, made from the lowering the borrow check makes
/// of each (front2 borrows.rs `borrows_of`): only when the check stalled a goal.
pub struct AsyncInteriors<'a> {
    /// Whether the check stalled a goal: otherwise no interior is computed (rule 77).
    wanted: bool,
    /// The record's table forked (`Checked::types` as its prefix): the interiors' types.
    out: Option<tc::PortableTys<'a>>,
    /// Why the record's table could not be forked, when it could not.
    table: Option<tc::CheckError>,
    /// Per async block lowered, its exact interior in `out`, in the order lowered.
    exact: AVec<'a, (tc::ExprId, tc::PTy)>,
    /// The same stored fields before their types are deduplicated into the tuple.
    fields: AVec<'a, (tc::ExprId, tc::PTy, Storage)>,
    awaits: AVec<'a, (tc::ExprId, tc::ExprId)>,
    refused: AVec<'a, (tc::ExprId, tc::CheckError)>,
}

#[derive(Clone, Copy)]
enum Storage {
    Capture(tc::PatId),
    Binding(tc::PatId),
    Temporary(tc::ExprId),
    Awaitee(tc::ExprId),
}

impl<'a> AsyncInteriors<'a> {
    /// For a body whose type check gave `done`.
    pub fn new_in(done: &tc::Checked<'_>, alloc: Alloc<'a>) -> AsyncInteriors<'a> {
        AsyncInteriors {
            wanted: !done.stalled.is_empty(), out: None, table: None, exact: AVec::new_in(alloc),
            fields: AVec::new_in(alloc), awaits: AVec::new_in(alloc), refused: AVec::new_in(alloc),
        }
    }

    /// Each async block's exact interior, when a goal stalled, from its WITNESS
    /// (blaze-flow-lower `lower_async_witness`, one_call.rs `Erased`: the borrow lowering's
    /// statements, loans and saved bits, relating no region). Made before and apart from the
    /// borrow check's lowering, so a region refusal there (or a borrow check stopped before the
    /// block) no longer leaves a stalled goal the gap (typeck.md "Witness records", W1; tempo's
    /// 45 rows "no lowered async block supplied its captures and saved locals"). A witness
    /// refused by form or layout leaves its block without one: its stalled goal stays the gap
    /// ([`decide_stalled_goals`]).
    pub fn witness_all(&mut self, view: &tc::BodyView<'_>, done: &tc::Checked<'_>, facts: &NestedFacts<'_, '_>, alloc: Alloc<'a>) {
        if !self.wanted {
            return;
        }
        for (i, x) in view.exprs.iter().enumerate() {
            if !matches!(x, tc::Expr::AsyncBlock { .. }) {
                continue;
            }
            let block = tc::ExprId(i as u32);
            match lower_async_witness(view, done, block, facts, alloc) {
                Ok(w) => self.keep(&w, done, block, alloc),
                Err(refusal) => self.refused.push((block, tc::CheckError::FutureWitness {
                    reason: "the async block's witness lowering refused a form or a layout", at: refusal.range,
                })),
            }
        }
    }

    /// Keeps async block `block`'s exact interior from its witness `w`.
    fn keep(&mut self, w: &AsyncWitness<Alloc<'a>>, done: &tc::Checked<'_>, block: tc::ExprId, alloc: Alloc<'a>) {
        if self.out.is_none() && self.table.is_none() {
            match done.types.fork_in(alloc) {
                Ok(t) => self.out = Some(t),
                Err(e) => self.table = Some(e),
            }
        }
        let Some(out) = self.out.as_mut() else { return };
        let first_field = self.fields.len();
        match storage_tuple(w, done, block, out, &mut self.fields, alloc) {
            Ok(t) => {
                self.exact.push((block, t));
                self.awaits.extend(w.point_of().iter().map(|&e| (block, e)));
            }
            Err(why) => {
                self.fields.truncate(first_field);
                self.refused.push((block, why));
            }
        }
    }
}

/// The captured PLACES (the Reference, "Async blocks": captures as closures do; RFC 2229), each
/// typed by how its field holds it (`T`, `&T`, `&mut T`), then the saved locals, each retaining
/// its identity for the finding, interned as the sorted, deduplicated tuple the solver takes.
/// The places are read under `Places` whatever `ActiveEnv` is, as blaze-flow-lower coroutine.rs
/// `parts_with` reads them: the witness's capture ARGUMENTS are its layout's (whole bindings
/// before the stage-2 switch) and are not taken; a saved local is a block local under either
/// layout. An owner the place analysis did not decide gives its whole-binding rows.
fn storage_tuple<'a>(
    w: &AsyncWitness<Alloc<'a>>, done: &tc::Checked<'_>, block: tc::ExprId,
    out: &mut tc::PortableTys<'a>, fields: &mut AVec<'a, (tc::ExprId, tc::PTy, Storage)>, alloc: Alloc<'a>,
) -> Result<tc::PTy, tc::CheckError> {
    let invalid = tc::CheckError::Malformed(tc::Malformed::Expr(block));
    let saved = w.saved(alloc).map_err(|_| tc::CheckError::FutureWitness {
        reason: "saved-local analysis exceeded its bound", at: w.range(),
    })?;
    // The witness lays one argument per whole-binding row of the block (its layout's count).
    let rows = done.captures.column::<0>().iter().filter(|b| **b == block).count();
    if rows != w.args() as usize {
        return Err(invalid);
    }
    let mut types = AVec::new_in(alloc);
    for f in done.env_fields_by::<tc::Places>(block) {
        let place = f.place_ty(done).ok_or(invalid)?;
        done.types.view().node(place)?;
        let ty = match f.how {
            tc::CaptureKind::ByValue => place,
            tc::CaptureKind::ByRef => out.intern(tc::PNode::Ref(tc::Mutbl::Not, place))?,
            tc::CaptureKind::ByMutRef => out.intern(tc::PNode::Ref(tc::Mutbl::Mut, place))?,
        };
        fields.push((block, ty, Storage::Capture(f.base)));
        types.push(ty);
    }
    // A binding the layout still moves whole beside its places (blaze-typeck env_fields.rs
    // `moved_whole_beside_places`): what the state holds until the stage-2 switch.
    for p in done.moved_whole_beside_places(block) {
        let ty = *done.local_ty.get(p.ix()).ok_or(invalid)?;
        done.types.view().node(ty)?;
        fields.push((block, ty, Storage::Capture(p)));
        types.push(ty);
    }
    for l in w.args() + 1..w.locals() {
        let source = if saved.is_saved(blaze_flow::Local(l)) {
            match w.identity().get(l as usize).ok_or(invalid)? {
                Identity::Binding(p) => Storage::Binding(*p),
                Identity::Temp(e, Role::Awaitee) => Storage::Awaitee(*e),
                Identity::Temp(e, _) => Storage::Temporary(*e),
                _ => return Err(invalid),
            }
        } else {
            continue;
        };
        let ty = *w.local_ty().get(l as usize).ok_or(invalid)?;
        done.types.view().node(ty)?;
        let ty = match *w.local_wrap().get(l as usize).ok_or(invalid)? {
            Some(m) => out.intern(tc::PNode::Ref(m, ty))?,
            None => ty,
        };
        fields.push((block, ty, source));
        types.push(ty);
    }
    types.sort_unstable();
    types.dedup();
    let list = out.intern_list(&types)?;
    out.intern(tc::PNode::Tuple(list))
}

/// What deciding a body's stalled goals gave, filed into its type verdict by [`Decision::file`].
pub enum Decision<'a> {
    /// No goal stalled.
    Nothing,
    /// Every stalled goal holds on its exact interior: nothing to file.
    Proved,
    /// The goals that fail there: an E0277 each, as the check files a diagnostic.
    Findings(AVec<'a, BodyDiagnostic<'a>>),
    /// The decision asked questions the world cannot answer yet (as a check's gaps).
    Gaps(Vec<Gap, Global>),
    /// Not decided: `INTERIOR_GAP` for a future whose body was not lowered, or why.
    Refused(Refused),
    /// The decision's read list named no fact.
    Reads(ReadsError),
}

impl<'a> Decision<'a> {
    /// Files the decision into the body's type verdict (after `file_borrow_reads`, which made
    /// the digest over the decision's rows). A verdict that is not an answer has no digest.
    pub fn file(self, verdict: &mut BodyVerdict<'a>) {
        match self {
            Decision::Nothing | Decision::Proved => {}
            Decision::Findings(found) => {
                for d in found {
                    verdict.diagnostics.push(d);
                }
                verdict.diagnostics.sort_by_key(|d| (d.start, d.end, d.code));
            }
            Decision::Gaps(gaps) => {
                for g in gaps {
                    verdict.gaps.push(g);
                }
                verdict.reads = None;
            }
            Decision::Refused(why) => {
                if verdict.refused.is_none() {
                    verdict.refused = Some(why);
                }
                verdict.reads = None;
            }
            Decision::Reads(why) => {
                verdict.facts = Err(why);
                verdict.reads = None;
            }
        }
    }
}

/// Decides `done`'s stalled goals on the exact interiors `interiors` made: round by round as the
/// check is (a record the decision needs is made, at most [`MOST_ROUNDS`] times), its read
/// rows pushed into `reads` (the module doc).
pub fn decide_stalled_goals<'n, 'a, S: NamesSource<'n>>(
    r: &mut Records<'_, 'n, S>,
    body: &Body<'n>,
    done: &tc::Checked<'_>,
    interiors: &AsyncInteriors<'a>,
    reads: &mut AVec<'a, Read>,
    alloc: Alloc<'a>,
) -> Decision<'a> {
    if done.stalled.is_empty() {
        return Decision::Nothing;
    }
    let owner = body.typeck.owner;
    let check = |error: tc::CheckError| Decision::Refused(Refused::Check(CheckFail::Check { owner, error }));
    let gap = |at| check(tc::CheckError::FutureWitness { reason: tc::INTERIOR_GAP, at });
    if let Some(error) = interiors.table {
        return check(error);
    }
    // No block was lowered. Use its own range for a direct future obligation.
    let Some(out) = &interiors.out else {
        let row = done.stalled[0];
        let at = match done.types.view().node(row.ty) {
            Ok(tc::PNode::Future(block, _, _)) => match body.typeck.expr_ranges.get(block.ix()) {
                Some(&at) => at,
                None => return check(tc::CheckError::Malformed(tc::Malformed::Expr(block))),
            },
            Ok(_) => row.diag.at,
            Err(why) => return check(why),
        };
        return check(tc::CheckError::FutureWitness { reason: "no lowered async block supplied its captures and saved locals", at });
    };
    let lowered_view = body.lowered.view();
    let spell = match Spelling::of(&lowered_view) {
        Ok(s) => s,
        Err(why) => return Decision::Refused(Refused::Spell(why)),
    };
    let scope = body.scope(&spell);
    let mut last: Option<tc::Question> = None;
    // The records made between rounds, for a refusal past the last (check.rs `rounds_fail`).
    let mut made = 0u64;
    // The decision's own leak list, from the check's last round's: grown only by the
    // decision's own `OpaqueHiddenUnknown` gaps (opaque_auto.rs `after_auto_gap`), each round
    // adding one opaque or ending, so the rounds are bounded by the opaques it can name.
    let mut leaks = r.store.hidden.new_leaks();
    leaks.extend_from_slice(r.store.hidden.leaking());
    for _ in 0..MOST_ROUNDS {
        r.store.hidden.set_leaking(&leaks);
        let mut failed_fields = AVec::new_in(alloc);
        let (got, fills, gaps) = {
            let tys = match body.tys.view(&r.store.tys) {
                Ok(t) => t,
                Err(error) => return check(error),
            };
            let view = body.typeck.view(tys);
            let world = world_at(r, &scope);
            let got = done.decide_stalled_with_storage(&view, out, &interiors.exact, &world, alloc, |d, block, field| {
                failed_fields.push((d, block, field));
            });
            (got, world.fills.into_inner(), world.gaps.into_inner())
        };
        // A failed candidate makes the decision no answer, as it makes a check's.
        if !gaps.is_empty() {
            return Decision::Gaps(gaps);
        }
        let tc::StalledDecision { found, reads: list, why: why_column, types: decided_tys } = match got {
            Ok(decided) => decided,
            Err(tc::StalledError::Need(q)) => {
                if fills.is_empty() {
                    let mut questions = Vec::new_in(Global);
                    questions.push(q);
                    return Decision::Refused(Refused::Check(CheckFail::NothingToFill { owner, questions }));
                }
                made += fills.len() as u64;
                if let Err(fail) = make_all(r, fills, owner) {
                    return Decision::Refused(Refused::Check(fail));
                }
                last = Some(q);
                continue;
            }
            Err(tc::StalledError::NoExactInterior { block }) => {
                if let Some(&(_, why)) = interiors.refused.iter().find(|&&(b, _)| b == block) {
                    return check(why);
                }
                return match body.typeck.expr_ranges.get(block.ix()) {
                    Some(&at) => gap(at),
                    None => check(tc::CheckError::Malformed(tc::Malformed::Expr(block))),
                };
            }
            // Another fn's future, at any depth, whose hidden type this decision is not given
            // yet: its defining body filed, the opaque leaked, the decision again (W3). Already
            // leaked and still unknown, the gap stands, naming the opaque.
            Err(tc::StalledError::Check(error @ tc::CheckError::OpaqueHiddenUnknown { .. })) => {
                match super::opaque_auto::after_auto_gap(r, owner, &error, &mut leaks) {
                    Ok(true) => continue,
                    Ok(false) => return check(error),
                    Err(fail) => return Decision::Refused(Refused::Check(fail)),
                }
            }
            Err(tc::StalledError::Check(error @ tc::CheckError::ForeignWitness { item, ordinal: None, .. })) => {
                let probe = tc::CheckError::OpaqueHiddenUnknown { opaque: item, trait_: tc::ItemKey(0) };
                match super::opaque_auto::after_auto_gap(r, owner, &probe, &mut leaks) {
                    Ok(true) => continue,
                    Ok(false) => return check(error),
                    Err(fail) => return Decision::Refused(Refused::Check(fail)),
                }
            }
            Err(tc::StalledError::Check(error)) => return check(error),
            Err(tc::StalledError::Ambiguous { trait_, site, storage, undecided }) => {
                let stored_at = match storage {
                    None => None,
                    Some((block, field)) => {
                        let Some(&(_, _, source)) = interiors.fields.iter().find(|&&(b, ty, _)| (b, ty) == (block, field)) else {
                            return check(tc::CheckError::Malformed(tc::Malformed::Expr(block)));
                        };
                        let range = match source {
                            Storage::Binding(p) | Storage::Capture(p) => body.typeck.pat_ranges.get(p.ix()).ok_or(tc::Malformed::Pat(p)),
                            Storage::Temporary(e) | Storage::Awaitee(e) => body.typeck.expr_ranges.get(e.ix()).ok_or(tc::Malformed::Expr(e)),
                        };
                        match range {
                            Ok(&at) => Some(at),
                            Err(bad) => return check(tc::CheckError::Malformed(bad)),
                        }
                    }
                };
                return check(tc::CheckError::FutureWitnessGoal { trait_, at: site, stored_at, undecided });
            }
            Err(tc::StalledError::TableNotExtended { .. }) => return check(tc::CheckError::Unported(STALLED_TABLE)),
        };
        let module = checker_of(body.module);
        let written = body.typeck.view(tc::PTys::EMPTY);
        // Kept to the crates the body sees: the decision asks in its check world (`world_at`,
        // `Reveal::InBody`: buckets.rs `Seen`).
        let buckets = match r.buckets.as_ref() {
            Some(b) => match super::buckets::Seen::of_checked_body(r.names, body.module) {
                Ok(seen) => Some((b, seen)),
                Err(why) => return Decision::Reads(ReadsError::Unseen(why)),
            },
            None => None,
        };
        match facts_with_buckets(list.questions(), Some(list.rows.column::<1>()), module, written.paths, written.segments, buckets, alloc) {
            Ok(rows) => reads.extend_from_slice(&rows),
            Err(why) => return Decision::Reads(why),
        }
        if found.is_empty() {
            return Decision::Proved;
        }
        // The decision's table: the interiors' (the record's as its prefix, and every stored
        // field's type) read in place, and the why-chain's goal types after it.
        let tys = decided_tys.view();
        // The why column the findings' `Diag::why` name: the decision's own, walked on the
        // exact interiors (`StalledDecision::why`).
        let why: &[tc::WhyStep] = &why_column;
        // Each held slot said as the value it is, for a chain's `CoroutineSlot` step.
        let held = match interiors.held_slots(r, tys, &written, lowered_view.item()) {
            Ok(h) => h,
            Err(why) => return check(why),
        };
        let mut diagnostics: AVec<'a, BodyDiagnostic<'a>> = AVec::new_in(alloc);
        for d in found.iter() {
            let about = super::check::diag_about_held(r, scope.spell, tys, &written, d, why, &held);
            let mut text = AString::from_str_in(&about, alloc);
            let mut notes = AVec::new_in(alloc);
            // The places the chain's held values name, as data (rule 19: which value, where).
            let steps = d.why.and_then(|w| why.get(w.start as usize..(w.start as usize).saturating_add(w.len as usize))).unwrap_or(&[]);
            for step in steps {
                let tc::WhyReached::CoroutineSlot { coroutine, slot } = step.reached else { continue };
                let Some(h) = held.iter().find(|h| h.coroutine == coroutine && h.slot == slot) else { continue };
                notes.extend(h.declared.iter().map(|at| DiagNote { what: NoteKind::HeldValue, start: at.start, end: at.end }));
                notes.extend(h.awaits.iter().map(|at| DiagNote { what: NoteKind::HeldAcross, start: at.start, end: at.end }));
            }
            // With no chain, the failed field's storage as the decision named it.
            if steps.is_empty()
                && let Some(&(_, block, field)) = failed_fields.iter().find(|(finding, _, _)| finding == d)
                && let Err(why) = interiors.describe_storage(block, field, &written, lowered_view.item(), &mut text)
            {
                return check(why);
            }
            diagnostics.push(BodyDiagnostic { code: d.code.code(), text, start: d.at.start, end: d.at.end, notes });
        }
        return Decision::Findings(diagnostics);
    }
    let mut questions = Vec::new_in(Global);
    questions.extend(last);
    Decision::Refused(Refused::Check(super::check::rounds_fail(r, owner, "deciding the stalled goals of an async body", made, questions)))
}

impl AsyncInteriors<'_> {
    /// Each slot of each exact interior said as the value it is: slot `i` of block `b` is the
    /// tuple's element `i` (the witness's types sorted and deduplicated, [`storage_tuple`]),
    /// which the solver's chain names as `CoroutineSlot { coroutine: b, slot: i }` (blaze-solve
    /// why_no.rs `fold_tuples`). Its words name each value stored with that type (a capture, a
    /// binding, a temporary, an awaited future) by its text and its type, and the awaits of
    /// the block it is held across; the ranges are kept as data.
    fn held_slots<'n, S: NamesSource<'n>>(
        &self, r: &Records<'_, 'n, S>, tys: tc::PTys<'_>, body: &tc::BodyView<'_>, item: &str,
    ) -> Result<Vec<HeldSlot>, tc::CheckError> {
        let snippet = |at: tc::Range| item.get(at.start as usize..at.end as usize);
        let mut out = Vec::new();
        for &(block, interior) in self.exact.iter() {
            let tc::PNode::Tuple(list) = tys.node(interior)? else {
                return Err(tc::CheckError::Malformed(tc::Malformed::Expr(block)));
            };
            let mut awaits = Vec::new();
            let mut across = String::new();
            for (i, &(_, e)) in self.awaits.iter().filter(|&&(b, _)| b == block).enumerate() {
                let &at = body.expr_ranges.get(e.ix()).ok_or(tc::CheckError::Malformed(tc::Malformed::Expr(e)))?;
                if i > 0 {
                    across.push_str(" or ");
                }
                across.push('`');
                across.push_str(snippet(at).ok_or(tc::CheckError::Malformed(tc::Malformed::Expr(e)))?);
                across.push('`');
                awaits.push(at);
            }
            for (slot, &ty) in tys.list(list)?.iter().enumerate() {
                let shown = super::check::ty_shown(r, tys, ty);
                let mut words = String::new();
                let mut declared = Vec::new();
                let mut stored = false;
                for &(b, t, source) in self.fields.iter() {
                    if (b, t) != (block, ty) {
                        continue;
                    }
                    let (at, bad) = match source {
                        Storage::Capture(p) | Storage::Binding(p) => (body.pat_ranges.get(p.ix()), tc::Malformed::Pat(p)),
                        Storage::Temporary(e) | Storage::Awaitee(e) => (body.expr_ranges.get(e.ix()), tc::Malformed::Expr(e)),
                    };
                    let &at = at.ok_or(tc::CheckError::Malformed(bad))?;
                    let text = snippet(at).ok_or(tc::CheckError::Malformed(bad))?;
                    if !words.is_empty() {
                        words.push_str(" and ");
                    }
                    words.push_str(match source {
                        Storage::Capture(_) => "the capture `",
                        Storage::Binding(_) => "`",
                        Storage::Temporary(_) => "the temporary `",
                        Storage::Awaitee(_) => "the awaited future `",
                    });
                    words.push_str(text);
                    words.push('`');
                    stored |= !matches!(source, Storage::Capture(_));
                    declared.push(at);
                }
                if words.is_empty() {
                    // A slot with no stored value of its type (never, by construction): said
                    // by its type alone.
                    words.push_str("a value");
                }
                words.push_str(" (`");
                words.push_str(&shown);
                words.push_str("`)");
                if stored && !across.is_empty() {
                    words.push_str(" is held across ");
                    words.push_str(&across);
                } else {
                    words.push_str(" is captured");
                }
                let slot = u32::try_from(slot).map_err(|_| tc::CheckError::Malformed(tc::Malformed::Expr(block)))?;
                out.push(HeldSlot { coroutine: tc::WhyDef::Expr(block), slot, words, declared, awaits: if stored { awaits.clone() } else { Vec::new() } });
            }
        }
        Ok(out)
    }

    /// The flow gives the union of saved locals over its suspension points. When
    /// there are several points, list the alternatives rather than guessing one.
    fn describe_storage(
        &self, block: tc::ExprId, field: tc::PTy, body: &tc::BodyView<'_>, item: &str, text: &mut AString<'_>,
    ) -> Result<(), tc::CheckError> {
        let snippet = |at: Option<&tc::Range>, bad| {
            at.and_then(|r| item.get(r.start as usize..r.end as usize)).ok_or(tc::CheckError::Malformed(bad))
        };
        let mut saved = false;
        for &(b, ty, source) in &self.fields {
            if (b, ty) != (block, field) { continue; }
            let value = match source {
                Storage::Capture(p) | Storage::Binding(p) => snippet(body.pat_ranges.get(p.ix()), tc::Malformed::Pat(p))?,
                Storage::Temporary(e) | Storage::Awaitee(e) => snippet(body.expr_ranges.get(e.ix()), tc::Malformed::Expr(e))?,
            };
            if matches!(source, Storage::Capture(_)) {
                text.push_str(", captured `");
            } else if matches!(source, Storage::Awaitee(_)) {
                text.push_str(", awaiting `");
            } else {
                saved = true;
                text.push_str(", stored `");
            }
            text.push_str(value);
            text.push_str("`");
        }
        if saved {
            text.push_str(" across suspension at ");
            for (i, &(_, e)) in self.awaits.iter().filter(|&&(b, _)| b == block).enumerate() {
                if i > 0 { text.push_str(" or "); }
                text.push_str("`");
                text.push_str(snippet(body.expr_ranges.get(e.ix()), tc::Malformed::Expr(e))?);
                text.push_str("`");
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use blaze_own::AVec;
    use blaze_typeck as tc;

    use super::{AsyncInteriors, Decision, Stalls, decide_stalled_goals};
    use crate::crates::{DirShelf, Inputs, Toolchain, UserCrate};
    use crate::front2::project::{BodyRef, ProjectWorld};
    use crate::refusal::SourceFile;

    /// WHEN an async block's interior is exact: its witness was lowered (blaze-flow-lower
    /// `lower_async_witness`) and kept ([`AsyncInteriors::keep`]), or the check's own record is
    /// already complete (`CoroutineWitness::Complete`); another fn's future is exact only by
    /// its record when that record is not `InteriorIs::Superset`. A `Send` goal that stalled on
    /// the superset is decided ONLY on such an interior (blaze-typeck stalled.rs
    /// `LocalWitnesses::interior`: `NoExactInterior` otherwise).
    ///
    /// The cause removed (rule 23): `local_send` is clean when its block's witness is lowered
    /// (opaque_auto.rs `an_async_fns_send_bound_waits_for_its_exact_interior`). Here the same
    /// body's decision is made with every async block's witness REFUSED, as the witness
    /// lowering refuses a form or a layout: the verdict is the refusal naming the witness,
    /// never clean. rustc accepts the program; blaze, without the exact interior, says it did
    /// not decide.
    #[test]
    fn a_refused_witness_never_decides_a_local_send() {
        const SOURCE: &str = r#"
fn require_send<T: Send>(_: T) {}
fn local_send() {
    require_send(async {
        let b = 1u8;
        async {}.await;
        drop(b);
    });
}
fn main() {}
"#;
        let alloc = blaze_own::testing::heap().alloc();
        let home = std::env::var("HOME").expect("HOME");
        let tc = Toolchain::open(&mut DirShelf::new(format!("{home}/.blaze/v2/toolchain"))).expect("fixture toolchain");
        let files = [("/refused/src/main.rs".to_owned(), SOURCE.to_owned())];
        let user = UserCrate {
            name: "refused", dir: "/refused/", root: "/refused/src/main.rs", files: &files,
            edition: edition::PackageEdition::of_manifest("2024").expect("Rust 2024"), panic: blaze_build::Panic::Unwind,
        };
        let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("fixture frame");
        let erased = blaze_own::Erased::new(&frame);
        let inputs = Inputs::new(&user, &tc).expect("fixture crate");
        let crates = inputs.crates();
        let names = inputs.resolve(&crates, erased.bump()).expect("fixture names");
        let sources = [SourceFile { path: "src/main.rs", text: SOURCE }];
        let mut world = ProjectWorld::new(&names, &frame, &sources, alloc).expect("fixture world");
        let bodies = world.bodies(alloc);
        let local = bodies.iter().find_map(|&(body, _)| match body {
            BodyRef::Free(k) if names.item_text(k)?.name == Some("local_send") => Some(body),
            _ => None,
        }).expect("missing fixture function `local_send`");

        const PLANTED: &str = "planted: the async block's witness lowering refused";
        let mut reads = AVec::new_in(alloc);
        let mut decision: Option<Decision<'_>> = None;
        let mut stalled = 0;
        let mut verdict = world.check_then(local, Stalls::Decide, alloc, |r, lowered, done| {
            stalled = done.stalled.len();
            let mut interiors = AsyncInteriors::new_in(done, alloc);
            // The record's table forked as `keep` forks it, and no block's interior kept: each
            // block's witness refused by name, as `witness_all` files a refusal.
            interiors.out = Some(done.types.fork_in(alloc).expect("the record's table"));
            let view = lowered.typeck.view(tc::PTys::EMPTY);
            for (i, x) in view.exprs.iter().enumerate() {
                if let tc::Expr::AsyncBlock { .. } = x {
                    let block = tc::ExprId(i as u32);
                    let at = *view.expr_ranges.get(block.ix()).expect("the block's range");
                    interiors.refused.push((block, tc::CheckError::FutureWitness { reason: PLANTED, at }));
                }
            }
            decision = Some(decide_stalled_goals(r, lowered, done, &interiors, &mut reads, alloc));
        });
        // The check stalled the block's `Send` on its superset interior: the `Stalls::Gap`
        // entry gave `INTERIOR_GAP` for this body before the witness decided it.
        assert!(stalled > 0, "local_send stalls no goal at the check: {verdict:?}");
        let decision = decision.expect("the check finished");
        assert!(matches!(decision, Decision::Refused(_)), "a refused witness decided the goal: {verdict:?}");
        decision.file(&mut verdict);
        assert!(!verdict.is_clean() && verdict.reads.is_none(), "{verdict:?}");
        let shown = format!("{:?}", verdict.refused);
        assert!(shown.contains(PLANTED), "the refusal names the witness: {shown}");
    }
}
