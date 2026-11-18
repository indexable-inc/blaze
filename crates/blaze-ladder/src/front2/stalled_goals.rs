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
//! 2. the move and borrow check (front2 borrows.rs `borrows_of`), which lowers each async
//!    block's body ONCE through [`AsyncInteriors::flow_of`]: the one lowering is the block's
//!    flow for the borrow check AND, when a goal stalled, the input of its exact interior
//!    (rule 77: nothing lowered twice);
//! 3. [`decide_stalled_goals`]: `blaze_typeck::decide_stalled_listed` re-solves each row on the
//!    exact interiors; each goal that fails there is an E0277 filed among the body's type
//!    diagnostics exactly as the check files its own (`BodyDiagnostic`, its words from
//!    check.rs `diag_about`), and a goal that holds there leaves nothing.
//!
//! A row whose future's body was not lowered (the lowering refused it, or the borrow check
//! stopped before it) stays the old refusal by name: `CheckError::Unported(INTERIOR_GAP)`,
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

use blaze_flow::Flow;
use blaze_flow_lower::{Refusal, interior, lower_async};
use blaze_own::{AString, AVec, Alloc};
use blaze_typeck as tc;

use super::body::Body;
use super::borrows_nested::{FlowOf, NestedFacts, flow_of};
use super::check::{CheckFail, MOST_ROUNDS, Records, diag_about, make_all, world_at};
use super::keys::checker_of;
use super::project::{BodyDiagnostic, BodyVerdict, Refused};
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
    /// Per async block lowered, its exact interior in `out`, in expression order.
    exact: AVec<'a, (tc::ExprId, tc::PTy)>,
}

impl<'a> AsyncInteriors<'a> {
    /// For a body whose type check gave `done`.
    pub fn new_in(done: &tc::Checked<'_>, alloc: Alloc<'a>) -> AsyncInteriors<'a> {
        AsyncInteriors { wanted: !done.stalled.is_empty(), out: None, table: None, exact: AVec::new_in(alloc) }
    }

    /// Flow `of` of the body, as borrows_nested.rs `flow_of` lowers it; an async block's
    /// lowering also gives, when a goal stalled, the block's exact interior (kept here). An
    /// interior the flow cannot give leaves the block without one: its stalled goal stays the
    /// gap ([`decide_stalled_goals`]), and the borrow check goes on with the flow.
    pub fn flow_of(
        &mut self,
        of: FlowOf,
        view: &tc::BodyView<'_>,
        done: &tc::Checked<'_>,
        facts: &NestedFacts<'_, '_>,
        alloc: Alloc<'a>,
    ) -> Result<(Flow<Alloc<'a>>, AVec<'a, blaze_flow_lower::ParamOrigin>), Refusal> {
        let block = match of {
            FlowOf::AsyncBlock(block) if self.wanted => block,
            _ => return flow_of(of, view, done, facts, alloc),
        };
        let lowered = lower_async(view, done, block, facts, alloc)?;
        if self.out.is_none() && self.table.is_none() {
            match done.types.fork_in(alloc) {
                Ok(t) => self.out = Some(t),
                Err(e) => self.table = Some(e),
            }
        }
        if let Some(out) = self.out.as_mut()
            && let Ok(t) = interior(&lowered, done, block, out, alloc)
        {
            self.exact.push((block, t));
        }
        // An async block's flow has no type-parameter placeholders (borrows_nested.rs `flow_of`).
        Ok((lowered.flow, AVec::new_in(alloc)))
    }
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
    let gap = || check(tc::CheckError::Unported(tc::INTERIOR_GAP));
    if let Some(error) = interiors.table {
        return check(error);
    }
    // No async block was lowered (the borrow check stopped before the first): not decided.
    let Some(out) = interiors.out.as_ref() else { return gap() };
    let lowered_view = body.lowered.view();
    let spell = match Spelling::of(&lowered_view) {
        Ok(s) => s,
        Err(why) => return Decision::Refused(Refused::Spell(why)),
    };
    let scope = body.scope(&spell);
    let mut last: Option<tc::Question> = None;
    for _ in 0..MOST_ROUNDS {
        let (got, fills, gaps) = {
            let tys = match body.tys.view(&r.store.tys) {
                Ok(t) => t,
                Err(error) => return check(error),
            };
            let view = body.typeck.view(tys);
            let world = world_at(r, &scope);
            let got = tc::decide_stalled_listed(&view, done, out.view(), &interiors.exact, &world, alloc);
            (got, world.fills.into_inner(), world.gaps.into_inner())
        };
        // A failed candidate makes the decision no answer, as it makes a check's.
        if !gaps.is_empty() {
            return Decision::Gaps(gaps);
        }
        let (found, list) = match got {
            Ok(decided) => decided,
            Err(tc::StalledError::Need(q)) => {
                if fills.is_empty() {
                    let mut questions = Vec::new_in(Global);
                    questions.push(q);
                    return Decision::Refused(Refused::Check(CheckFail::NothingToFill { owner, questions }));
                }
                if let Err(fail) = make_all(r, fills, owner) {
                    return Decision::Refused(Refused::Check(fail));
                }
                last = Some(q);
                continue;
            }
            Err(tc::StalledError::NoExactInterior { .. }) => return gap(),
            Err(tc::StalledError::Check(error)) => return check(error),
            Err(tc::StalledError::Ambiguous { .. }) => return check(tc::CheckError::Unported(STALLED_AMBIGUOUS)),
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
        let tys = done.types.view();
        let mut diagnostics: AVec<'a, BodyDiagnostic<'a>> = AVec::new_in(alloc);
        for d in found.iter() {
            let about = diag_about(r, scope.spell, tys, &written, d);
            diagnostics.push(BodyDiagnostic { code: d.code.code(), text: AString::from_str_in(&about, alloc), start: d.at.start, end: d.at.end });
        }
        return Decision::Findings(diagnostics);
    }
    let mut questions = Vec::new_in(Global);
    questions.extend(last);
    Decision::Refused(Refused::Check(CheckFail::Rounds { owner, last: questions }))
}
