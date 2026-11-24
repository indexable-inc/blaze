//! The check loop (step 3's driver): check one body in a [`BodyWorld`]; on `Outcome::Need`
//! make every [`Fill`] the world noted ([`Store::ensure`], or every impl header and then the
//! [`Buckets`]) and check again with the needs as hints, at most [`MOST_ROUNDS`] times. A
//! [`Gap`] the world noted is kept and the loop goes on while there is something to make, so
//! the failure lists every gap the body met (the work list), each naming its item.
//!
//! The body's types are in the store's table ([`super::body`] converts it there): the table
//! only grows, so the body's view is taken again each round, after the fills.

use std::alloc::Global;
use std::boxed::Box;
use std::format;
use std::string::String;
use std::vec::Vec;

use blaze_id::ItemKey as LibKey;
use super::body::Body;
use blaze_scope::resolve::Names;
use blaze_typeck as tc;
use blaze_typeck::{BodyView, InferCtx, ItemKey, Name, Needs, Outcome, Question};

use super::buckets::{AdmitError, Ask, Buckets, ImplQuestionError, Seen, TablesReadError, Unfilled, inherent_home, prepare_coherence, trait_home};
use super::heads::{HeadError, Store, Want};
use super::index::Index;
use super::keys::{Keys, Origin, checker_of};
use super::source::NamesSource;
use super::spell::Spelling;
use super::consts::{ConstFail, Consts, int_of};
use super::types::{SelfTy, Why};
use blaze_lower::scoped::Prepares;
use super::world_items::{BodyWorld, Fill, Gap};
pub use super::body_reads::{check_body_asked, check_body_filed};

/// The most checks of one body: each round makes at least one record, so past this the
/// fills do not converge.
pub const MOST_ROUNDS: u32 = 256;

/// Where a body is written: what its paths are resolved against.
pub struct BodyScope<'s> {
    /// The body's checker key (its blocks' local items are its).
    pub owner: ItemKey,
    pub module: LibKey,
    pub params: &'s [(Name, u32)],
    pub self_: &'s SelfTy,
    pub spell: &'s Spelling<'s>,
    /// The traits in scope at `module`, checker keys, sorted.
    pub traits: &'s [ItemKey],
    /// The traits in scope at the body's blocks whose list is not `traits`.
    pub blocks: &'s super::block_traits::BlockTraits,
    /// Parallel to the checked body's paths: each path's first word's mark (blaze-lower
    /// `TypeckBody::path_marks`), where `BodyWorld::path` reads it.
    pub path_marks: &'s [Option<blaze_lower::mark_site::TextMark>],
    /// Parallel to the same paths: the namespace each is resolved in (blaze-lower
    /// `TypeckBody::path_ns`).
    pub path_ns: &'s [blaze_lower::typeck::PathNs],
}

/// Why a body did not check.
#[derive(Debug)]
pub enum CheckFail {
    /// The world could not answer questions yet, each by the item it is about; every distinct
    /// gap of every round, sorted by its `Debug` text (never by the round it was met in).
    Gap { owner: ItemKey, gaps: Vec<Gap, Global> },
    /// The checker needed records and the world noted nothing to make.
    NothingToFill { owner: ItemKey, questions: Vec<Question, Global> },
    /// The store could not make a record.
    Head { owner: ItemKey, error: HeadError },
    Buckets { owner: ItemKey, error: Unfilled },
    /// The impl tables ([`NamesSource::impl_tables`]) could not file a question's impls: the
    /// tables' own fault, which every question reading them meets (a run's stop).
    Tables { owner: ItemKey, error: TablesReadError },
    /// One impl question could not be asked (its trait or self type has no library key, or its
    /// shape no row shape): this body's refusal; every other question still answers.
    ImplQuestion { owner: ItemKey, error: ImplQuestionError },
    /// The body is malformed or a bound of the checker passed.
    Check { owner: ItemKey, error: tc::CheckError },
    /// The check finished with diagnostics: the program's errors in the body, each by its
    /// code and its range in the item's text. Never counted as checked.
    /// Each with what it is about, printed from the record's types: an unmet bound (E0277) as
    /// `Self: Trait<First>` from `Because::Bound`, else its `because`, `found` and `expected`.
    /// `filed`, one per diagnostic: where it is filed (front2 expansion_origin.rs): in expanded
    /// text, at the user's tokens or at the call; elsewhere [`Filed::Own`](super::expansion_origin::Filed::Own).
    Diags { owner: ItemKey, diags: Vec<(tc::DiagCode, tc::Range, Box<str>), Global>, filed: Vec<super::expansion_origin::Filed, Global> },
    /// Still `Need` after [`MOST_ROUNDS`]. `last`: the last round's HOLES (the questions it had
    /// no answer to, never every question it asked: rt-loop a1750's `tempo_main_with` printed
    /// 4,200 asked questions and no hole). `shown`: what stopped, by path (rule 19), made by
    /// [`rounds_fail`]: the loop, the records it made, the holes left and the first of them
    /// with its item's path.
    Rounds { owner: ItemKey, last: Vec<Question, Global>, shown: Box<str, Global> },
    /// The constant `key` an array length in a head names has no value: the evaluator's
    /// refusal, or [`ConstFail::Nested`] for one met while another constant is evaluated.
    /// `shown` is what `key` names ([`key_shown`]: rule 19), beside the key tools grep.
    Const { owner: ItemKey, key: LibKey, shown: Box<str, Global>, error: Box<ConstFail, Global> },
    /// Instance resolution of the call of `item` in `owner` failed (no impl, overlap, not
    /// ground, or a question the world could not make a record for).
    Resolve { owner: ItemKey, item: ItemKey, error: tc::ResolveError, args: Box<str> },
    /// `owner`'s body, checked again in this run, read other answers than its first check:
    /// two read-list digests under one owner (front2 body_reads.rs `Digests::file`; rule 21).
    /// `diff` names the rows that differ (rule 19).
    ReadsDiffer { owner: ItemKey, diff: Box<super::body_reads::ReadsDiff> },
    /// Comptime `inline for` unrolling failed on `owner` at `at`.
    InlineFor { owner: ItemKey, at: tc::Range, why: &'static str },
}

impl core::fmt::Display for CheckFail {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            CheckFail::Gap { owner, gaps } => {
                // On ONE line: a note is read a line at a time (the run's not-checked report
                // kept "1 gaps" and lost the gap itself on the next line).
                write!(f, "checking {owner:?}: {} gaps", gaps.len())?;
                for (i, gap) in gaps.iter().enumerate() {
                    let sep = if i == 0 { ": " } else { "; " };
                    write!(f, "{sep}{gap}")?;
                }
                Ok(())
            }
            CheckFail::NothingToFill { owner, questions } => write!(f, "checking {owner:?}: the checker needs {questions:?} and the world noted nothing to make"),
            CheckFail::Head { owner, error } => write!(f, "checking {owner:?}: {error}"),
            CheckFail::Buckets { owner, error } => write!(f, "checking {owner:?}: {error}"),
            CheckFail::Tables { owner, error } => write!(f, "checking {owner:?}: {error}"),
            CheckFail::ImplQuestion { owner, error } => write!(f, "checking {owner:?}: {error}"),
            CheckFail::Check { owner, error } => write!(f, "checking {owner:?}: {error:?}"),
            CheckFail::Diags { owner, diags, filed } => {
                write!(f, "checking {owner:?}: {} diagnostics", diags.len())?;
                for (i, (code, at, about)) in diags.iter().enumerate() {
                    write!(f, "\n  {} at {}..{}: {about}", code.code(), at.start, at.end)?;
                    if let Some(place) = filed.get(i).filter(|p| !matches!(p, super::expansion_origin::Filed::Own)) {
                        write!(f, "\n    {place}")?;
                    }
                }
                Ok(())
            }
            CheckFail::Rounds { last, shown, .. } => {
                let need = last.first().map_or("a record", |question| match question.record_kind() {
                    tc::RecordKind::Body => "the checked body record of a dependency item",
                    tc::RecordKind::Item => "an item record",
                });
                write!(f, "checking the body in its source file: still needs {need} after {MOST_ROUNDS} rounds: {shown}")
            }
            CheckFail::ReadsDiffer { owner, diff } => write!(f, "checking {owner:?} again read other answers than its first check: two read-list digests under one body (rule 21): {diff}"),
            CheckFail::Resolve { owner, item, error, args } => write!(f, "in {owner:?}, the call of {item:?} at [{args}]: {error}"),
            CheckFail::Const { owner, key, shown, error } => write!(f, "checking {owner:?}: the constant {key:?} ({shown}) an array length names: {error:?}"),
            CheckFail::InlineFor { owner, at, why } => write!(f, "checking {owner:?} at {}..{}: inline for: {why}", at.start, at.end),
        }
    }
}

/// The program's records the loop reads and fills, its names from the source `S`
/// ([`NamesSource`]: the solved names by default, or each crate's records).
pub struct Records<'r, 'n, S = &'n Names<'n>> {
    pub names: S,
    pub keys: &'r mut Keys,
    pub index: &'r Index<'n>,
    pub store: &'r mut Store<'n>,
    pub buckets: &'r mut Option<Buckets>,
    /// The order a round's fills and a question's candidate headers are made in. A body's
    /// outcome must not depend on it; the determinism test checks a body under both.
    pub order: BuildOrder,
    /// The constant evaluator and the build's prepared macros, for an array length in a head
    /// that names a constant; `None` while a constant is being evaluated (and where a caller
    /// has none: such a length is then [`ConstFail::Nested`]).
    pub consts: Option<(&'r mut Consts<'n>, &'r Prepares<'n>)>,
}

/// The order the loop makes what a round noted: as asked, or reversed (a test's second run).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BuildOrder {
    AsAsked,
    Reversed,
}

/// Checks `body` (written at `scope`, its types in its own table, `Body::tys`), filling the
/// records it needs. The records are answered from `r.store`'s table, which holds no body's
/// type: a body's check reads the same records whatever was checked before it.
pub fn check_body<'n, S: NamesSource<'n>>(r: &mut Records<'_, 'n, S>,body: &Body<'_>, scope: &BodyScope<'_>) -> Result<tc::Checked<'static>, CheckFail> {
    check_body_counted(r, body, scope, None)
}

/// `check_body` that adds each round's questions and costs into a tally (blaze-typeck asks.rs
/// `Tally`), reading the caller's clock: a measurement, never an input to the outcome.
pub fn check_body_counted<'n, S: NamesSource<'n>>(
    r: &mut Records<'_, 'n, S>,
    body: &Body<'_>,
    scope: &BodyScope<'_>,
    count: Option<(&mut tc::Tally, &dyn tc::Clock)>,
) -> Result<tc::Checked<'static>, CheckFail> {
    check_body_asked(r, body, scope, count, None)
}

/// `check_body_counted` that also leaves in `root_keys` a digest of each ground root goal the
/// solver searched in the body's last round (blaze-solve `Solver::keep_root_keys`): a
/// measurement of how many goals bodies share, never an input to the outcome.
pub fn check_body_metered<'n, S: NamesSource<'n>>(
    r: &mut Records<'_, 'n, S>,
    body: &Body<'_>,
    scope: &BodyScope<'_>,
    tally: &mut tc::Tally,
    clock: &dyn tc::Clock,
    root_keys: &mut Vec<u64, Global>,
) -> Result<tc::Checked<'static>, CheckFail> {
    super::body_reads::check_body_keyed(r, body, scope, Some((tally, clock)), None, Some(root_keys))
}

/// The check loop itself: `body` checked round by round, leaving in `asked` what a check that
/// did NOT end in a clean `Checked` asked of the world (front2 reads.rs: every body has a read
/// list): the read list of a run that finished but is returned as a failure (diagnostics, or a
/// gap answered as missing), and the questions of the last round that ended in a need. A clean
/// check's list is still in its `Checked`: only [`check_body_asked`] and [`check_body_filed`]
/// (front2 body_reads.rs) call this, and each files the list out before the `Checked` is kept.
pub(crate) fn check_rounds<'n, S: NamesSource<'n>>(
    r: &mut Records<'_, 'n, S>,
    body: &Body<'_>,
    scope: &BodyScope<'_>,
    count: Option<(&mut tc::Tally, &dyn tc::Clock)>,
    asked: Option<&mut super::reads::Asked<'_>>,
    root_keys: Option<&mut Vec<u64, Global>>,
) -> Result<tc::Checked<'static>, CheckFail> {
    check_rounds_as(r, body, scope, count, asked, super::stalled_goals::Stalls::Gap, root_keys)
}

/// The E0412s and E0405s of `owner`'s OWN signature (its head's [`super::type_miss::HeadMiss`]es, in the
/// store the head was made in or one filed from its record), as findings of its body's check:
/// item-relative in the fn's text, as its body's own misses are. A caller's check never asks
/// for its callee's, so each is said once, by the fn's own body.
fn signature_misses<'r, 'n, S: NamesSource<'n>>(r: &'r Records<'_, 'n, S>, owner: ItemKey) -> impl Iterator<Item = (tc::DiagCode, tc::Range, Box<str>)> + use<'r, 'n, S> {
    let misses: &'r [super::type_miss::HeadMiss] = r.store.type_misses.get(&owner).map_or(&[], |m| &m[..]);
    misses.iter().map(super::type_miss::HeadMiss::finding)
}

/// The body `owner` finished with the findings `diags`: each filed where its tokens came from,
/// before it leaves the task that made it (drive.md, ruled 20:57), in `alloc` (the store's).
fn said<'n, S: NamesSource<'n>>(r: &Records<'_, 'n, S>, owner: ItemKey, diags: Vec<(tc::DiagCode, tc::Range, Box<str>), Global>, alloc: blaze_own::Alloc<'_>) -> CheckFail {
    let lib = r.keys.library_in(r.names, owner);
    // A member of a trait or an impl has no library key (keys.rs `library_in`): it is placed by
    // its container's, its text found in the container's (expansion_origin.rs
    // `file_member_range`), never "no place".
    let member = match lib {
        Some(_) => None,
        None => r.store.fns.get(&owner).filter(|h| h.container.is_some()).map(|h| (h.lib, h.text)),
    };
    let mut filed = Vec::with_capacity_in(diags.len(), Global);
    filed.extend(diags.iter().map(|(_, at, _)| match member {
        Some((container, text)) => super::expansion_origin::file_member_range(r.names, container, text, *at, alloc),
        None => super::expansion_origin::file_range(r.names, lib, *at, alloc),
    }));
    CheckFail::Diags { owner, diags, filed }
}

/// One round's inference context, made in this frame and boxed in `alloc`: the caller's frame
/// holds the box's two words ([`check_rounds_as`]).
#[inline(never)]
fn infer_boxed(alloc: blaze_own::Alloc<'static>) -> blaze_own::Box<InferCtx<'static>, blaze_own::Alloc<'static>> {
    blaze_own::Box::new_in(InferCtx::analysis_in(alloc), alloc)
}

const _: () = assert!(core::mem::size_of::<blaze_own::Box<InferCtx<'static>, blaze_own::Alloc<'static>>>() == 2 * core::mem::size_of::<usize>());

/// Pre-unroll probe pass for a body containing `inline for`: resolves `expr_ty` for each
/// `inline for` iterable expression without failing on downstream diagnostics that depend on
/// the unexpanded loop body, and without filing `inferred_output` / `reads` / `closures`.
pub(crate) fn check_rounds_probe<'n, S: NamesSource<'n>>(
    r: &mut Records<'_, 'n, S>,
    body: &Body<'_>,
    scope: &BodyScope<'_>,
) -> Result<tc::Checked<'static>, CheckFail> {
    let prev = r.store.inline_for_probe;
    r.store.inline_for_probe = true;
    let out = check_rounds_as(r, body, scope, None, None, super::stalled_goals::Stalls::Gap, None);
    r.store.inline_for_probe = prev;
    out
}

/// Strict type-checking pass for definition-time checking of `ForFields`-bounded generic
/// `inline for` bodies: temporarily forces `inline_for_probe = false` so any missing method
/// or unsatisfied bound fails immediately with diagnostics.
pub(crate) fn check_rounds_strict<'n, S: NamesSource<'n>>(
    r: &mut Records<'_, 'n, S>,
    body: &Body<'_>,
    scope: &BodyScope<'_>,
) -> Result<tc::Checked<'static>, CheckFail> {
    let prev = r.store.inline_for_probe;
    r.store.inline_for_probe = false;
    let out = check_rounds_as(r, body, scope, None, None, super::stalled_goals::Stalls::Gap, None);
    r.store.inline_for_probe = prev;
    out
}

/// [`check_rounds`] with the check's entry for an auto-trait goal of an async block's future
/// that fails only on the superset interior named (front2 stalled_goals.rs `Stalls`): only
/// `ProjectWorld::check_borrows` passes `Stalls::Decide`, and it decides every stalled row.
pub(crate) fn check_rounds_as<'n, S: NamesSource<'n>>(
    r: &mut Records<'_, 'n, S>,
    body: &Body<'_>,
    scope: &BodyScope<'_>,
    mut count: Option<(&mut tc::Tally, &dyn tc::Clock)>,
    mut asked: Option<&mut super::reads::Asked<'_>>,
    stalls: super::stalled_goals::Stalls,
    mut root_keys: Option<&mut Vec<u64, Global>>,
) -> Result<tc::Checked<'static>, CheckFail> {
    let allow_diags = r.store.inline_for_probe;
    let owner = body.typeck.owner;
    // The store's allocator: the checked body outlives the round (`Checked<'static>`).
    let alloc = r.store.tys.alloc();
    let mut hints = Needs::new_in(alloc);
    // The records made between rounds, for a refusal past the last ([`rounds_fail`]).
    let mut made = 0u64;
    let mut all_gaps: Vec<Gap, Global> = Vec::new_in(Global);
    // The opaques whose hidden types this check reads, grown only by its own gaps
    // (front2/opaque_auto.rs): its answers never depend on what was checked before it.
    let mut leaks = r.store.hidden.new_leaks();
    // The body task's meter (front2 tables/cost.rs): a measurement only.
    let meter = r.store.cost;
    // A seeded store answers impl questions on the spot (world_items.rs
    // `BodyWorld::impl_question`), so the body's impl index is made before round 0, the one
    // maker `make_once` calls too (`Buckets::for_source`); over a lowering store the first impl
    // fill makes it, as before.
    if r.store.seeds.is_some() && r.buckets.is_none() {
        *r.buckets = Some(Buckets::for_source(r.names, r.index));
    }
    // The body's gate (front2 asks.rs `Gate`): round 0's syntactic prefetch, before the walk.
    r.store.gate.borrow_mut().start(alloc);
    // How this body's world meets a hole (world_items.rs `BodyWorld::stops_at_holes`): the
    // same test, read once; the store's seeds and asks do not change during a check.
    let stops_at_holes = r.store.stops_at_holes();
    let mut holes = RoundHoles::new();
    for round in 0..MOST_ROUNDS {
        r.store.hidden.set_leaking(&leaks);
        // Facts made on the spot this round (world_items.rs `BodyWorld::store_want`; under the
        // one ask, what it made: `Store::imports`).
        let imported_at = r.store.imports();
        let tys = body.tys.view(&r.store.tys).map_err(|error| CheckFail::Check { owner, error })?;
        let view: BodyView<'_> = body.typeck.view(tys);
        let trace_ask = r.store.trace;
        let world = world_at(r, scope);
        // Boxed, made in a frame of its own: this frame stays on the stack under the whole
        // check (blaze-int notes t29 overflow-bt4.txt: 48,576 bytes).
        let mut cx = infer_boxed(alloc);
        if root_keys.is_some() {
            cx.keep_root_keys();
        }
        // A measurement for tests (heads.rs `Store::trace`): the last round's trace is kept.
        if let Some(ask) = trace_ask {
            cx.trace_trait_goals(ask);
        }
        let out = match &mut count {
            Some((tally, clock)) => tc::check_counted(&view, &world, &mut *cx, &hints, tally, *clock, alloc),
            // Stalled goals decided after the async bodies' flows (front2 stalled_goals.rs).
            None if stalls == super::stalled_goals::Stalls::Decide => tc::check_deferring(&view, &world, &mut *cx, &hints, alloc),
            None => tc::check(&view, &world, &mut *cx, &hints, alloc),
        };
        // Round 0's walk is over: what its prefetch named is the gate's set.
        r.store.gate.borrow_mut().freeze(alloc);
        // An auto trait of another fn's opaque: its defining body filed (checked only if no
        // clean check of it is filed yet), the opaque leaked to this check, the round again.
        let out = match out {
            Ok(out) => out,
            Err(error) => {
                drop(world);
                if trace_ask.is_some() {
                    r.store.traced = cx.take_trace();
                }
                handle_check_error(r, body, owner, error, &mut leaks, asked.as_deref_mut(), alloc)?;
                continue;
            }
        };
        if let Some((tally, _)) = count.as_mut() {
            cx.add_solver_counts(&mut tally.solve);
        }
        if let Some(cost) = meter {
            let mut solve = tc::Solve::default();
            cx.add_solver_counts(&mut solve);
            cost.round(&solve);
        }
        if let Some(keys) = root_keys.as_deref_mut() {
            keys.clear();
            keys.extend_from_slice(cx.root_keys());
        }
        let mut fills = world.fills.into_inner();
        if r.order == BuildOrder::Reversed {
            fills.reverse();
        }
        let mut gaps = world.gaps.into_inner();
        if trace_ask.is_some() {
            r.store.traced = cx.take_trace();
        }
        gaps.extend(body.typeck.const_gaps.iter().map(|&gap| Gap::BodyConst { owner, gap }));
        let round_gaps = !gaps.is_empty();
        if round_gaps {
            merge_gaps(&mut all_gaps, gaps);
        }
        let needs = match out {
            // A finished check with a diagnostic is not a checked body: each is a finding.
            // A written type that named nothing (E0412, front2 type_miss.rs) is a finding too,
            // before the checker's: the body was checked with the error type in its place. So is
            // one of the fn's own signature (its head's, `Store::type_misses`): a body whose
            // head holds the error type is never a clean check, even where the walk read none.
            Outcome::Done(done) if !round_gaps && !allow_diags && (!done.diags.is_empty() || !body.type_misses.is_empty() || r.store.type_misses.contains_key(&owner)) => {
                let tys = body.tys.view(&r.store.tys).map_err(|error| CheckFail::Check { owner, error })?;
                let view: BodyView<'_> = body.typeck.view(tys);
                return Err(collect_done_diags(r, body, scope, &view, owner, done, asked.as_deref_mut(), alloc));
            }
            Outcome::Done(done) if !round_gaps => {
                if !allow_diags
                    && let Some(inferred_out) = done.inferred_output
                {
                    let _ = r.store.file_inferred_output(owner, done.types.view(), inferred_out);
                }
                // The shared goals the finished round took (blaze-solve share.rs `GoalHit`): the
                // one read each, which the body's result names (tables/task.rs).
                let mut hits = Vec::with_capacity_in(cx.goal_hits().len(), Global);
                hits.extend_from_slice(cx.goal_hits());
                r.store.goal_hits = hits;
                return Ok(done);
            }
            Outcome::Done(done) => {
                if !r.store.inferring_output_stack.is_empty() {
                    for g in all_gaps.iter() {
                        if let Gap::InferredOutputCycle { owner: a, callee: b } = *g {
                            r.store.inferred_output_cycle = Some((a, b));
                            break;
                        }
                    }
                }
                if let Some(asked) = asked {
                    asked.finished = Some(done.reads.clone());
                }
                return Err(CheckFail::Gap { owner, gaps: all_gaps });
            }
            Outcome::Need(needs) => needs,
        };
        holes.round(round, &needs.missing);
        if let Some(cost) = meter {
            cost.again(&needs, &r.store.gate.borrow());
        }
        if let Some(asked) = asked.as_deref_mut() {
            asked.questions.clear();
            asked.questions.extend_from_slice(&needs.questions);
        }
        if fills.is_empty() && !all_gaps.is_empty() {
            if !r.store.inferring_output_stack.is_empty() {
                for g in all_gaps.iter() {
                    if let Gap::InferredOutputCycle { owner: a, callee: b } = *g {
                        r.store.inferred_output_cycle = Some((a, b));
                        break;
                    }
                }
            }
            return Err(CheckFail::Gap { owner, gaps: all_gaps });
        }
        if fills.is_empty() && r.store.imports() != imported_at {
            hints = needs;
            continue;
        }
        if fills.is_empty() {
            let mut questions = Vec::new_in(Global);
            questions.extend_from_slice(&needs.questions);
            return Err(CheckFail::NothingToFill { owner, questions });
        }
        let mut inferred_targets: blaze_own::Vec<ItemKey, blaze_own::Alloc<'static>> = blaze_own::Vec::new_in(alloc);
        fills.retain(|fill| match *fill {
            Fill::InferredOutput(target) => {
                if inferred_targets.scan_for(&target).is_none() {
                    inferred_targets.push(target);
                }
                false
            }
            _ => true,
        });
        if !inferred_targets.is_empty() && !r.store.inferring_output_stack.is_empty() {
            r.store.inferred_output_wanted = inferred_targets.first().copied();
            return Err(CheckFail::Gap { owner, gaps: all_gaps });
        }
        let filling = u64::try_from(fills.len() + inferred_targets.len()).unwrap_or(u64::MAX);
        // Under the one ask over frozen heads the run stopped at its first hole
        // (`World::stops_at_holes`, world_items.rs): no fill is speculative, so every one is
        // made and a refusal is the body's (`make_all`), never dropped as `make_round` drops one.
        let dropped = if fills.is_empty() {
            0
        } else if stops_at_holes {
            make_all(r, fills, owner).map(|()| 0)?
        } else {
            make_round(r, fills, owner)?
        };
        if !inferred_targets.is_empty()
            && let Err(fail) = resolve_inferred_outputs(r, owner, &inferred_targets)
        {
            return match fail {
                InferredErr::Gap(g) => {
                    all_gaps.push(g);
                    Err(CheckFail::Gap { owner, gaps: all_gaps })
                }
                InferredErr::Fail(e) => Err(e),
            };
        }
        made += filling - dropped;
        if let Some((tally, _)) = count.as_mut() {
            tally.dropped += dropped;
            if round > 0 {
                for &q in needs.missing.iter() {
                    if let Some(slot) = tally.late_holes.get_mut(tc::question_kind(q)) {
                        *slot += 1;
                    }
                }
            }
        }
        hints = needs;
    }
    Err(rounds_exhausted(r, owner, made, &hints, &holes))
}

#[cold]
#[inline(never)]
fn merge_gaps<A: core::alloc::Allocator>(all_gaps: &mut Vec<Gap, A>, gaps: Vec<Gap, A>) {
    for gap in gaps {
        let shown = format!("{gap:?}");
        if let Err(at) = all_gaps.binary_search_by(|g| format!("{g:?}").cmp(&shown)) {
            all_gaps.insert(at, gap);
        }
    }
}

#[cold]
#[inline(never)]
fn rounds_exhausted<'n, S: NamesSource<'n>>(
    r: &Records<'_, 'n, S>,
    owner: ItemKey,
    made: u64,
    hints: &Needs<'static>,
    holes: &RoundHoles,
) -> CheckFail {
    let mut last = Vec::new_in(Global);
    last.extend_from_slice(&hints.missing);
    let mut fail = rounds_fail(r, owner, "the body's check", made, last);
    if let CheckFail::Rounds { last, shown, .. } = &mut fail {
        *shown = format!("{shown}; {}", holes.said(last)).into_boxed_str();
    }
    fail
}

#[cold]
#[inline(never)]
fn handle_check_error<'n, S: NamesSource<'n>>(
    r: &mut Records<'_, 'n, S>,
    body: &Body<'_>,
    owner: ItemKey,
    error: tc::CheckError,
    leaks: &mut blaze_own::Vec<ItemKey, blaze_own::Alloc<'static>>,
    asked: Option<&mut super::reads::Asked<'_>>,
    alloc: blaze_own::Alloc<'static>,
) -> Result<(), CheckFail> {
    match super::opaque_auto::after_auto_gap(r, owner, &error, leaks) {
        Ok(true) => return Ok(()),
        Ok(false) => {}
        Err(why) => {
            if let (Some(asked), tc::CheckError::OpaqueHiddenUnknown { opaque, .. }) = (asked, &error) {
                asked.questions.push(tc::Question::Opaque(*opaque));
            }
            return Err(why);
        }
    }
    if matches!(error, tc::CheckError::UnknownInCleanBody { .. }) && (!body.type_misses.is_empty() || r.store.type_misses.contains_key(&owner)) {
        let mut diags = Vec::with_capacity_in(body.type_misses.len(), Global);
        diags.extend(signature_misses(r, owner));
        diags.extend(body.type_misses.iter().map(|m| (tc::DiagCode::UnresolvedType, m.range(), m.words.clone())));
        return Err(said(r, owner, diags, alloc));
    }
    Err(CheckFail::Check { owner, error })
}

#[cold]
#[inline(never)]
#[expect(clippy::too_many_arguments, reason = "outlined diagnostic formatting of check_rounds_as")]
fn collect_done_diags<'n, S: NamesSource<'n>>(
    r: &Records<'_, 'n, S>,
    body: &Body<'_>,
    scope: &BodyScope<'_>,
    view: &BodyView<'_>,
    owner: ItemKey,
    done: tc::Checked<'static>,
    asked: Option<&mut super::reads::Asked<'_>>,
    alloc: blaze_own::Alloc<'static>,
) -> CheckFail {
    let mut diags = Vec::with_capacity_in(body.type_misses.len() + done.diags.len(), Global);
    diags.extend(signature_misses(r, owner));
    diags.extend(body.type_misses.iter().map(|m| (tc::DiagCode::UnresolvedType, m.range(), m.words.clone())));
    let tys = done.types.view();
    let mut future_goals = blaze_own::Vec::with_capacity_in(done.diags.len(), alloc);
    for d in done.diags.iter() {
        if let Some(trait_) = future_goal_trait(tys, d)
            && future_goals.iter().any(|&(at, seen)| at == d.at && seen == trait_)
        {
            continue;
        }
        if let Some(trait_) = future_goal_trait(tys, d) {
            future_goals.push((d.at, trait_));
        }
        diags.push((d.code, d.at, diag_about(r, scope.spell, tys, view, d, &done.why)));
    }
    if let Some(asked) = asked {
        asked.finished = Some(done.reads.clone());
    }
    said(r, owner, diags, alloc)
}

/// The holes each round of a body's check met (item 9's measure, rt-loop a1850: `tempo_main_with`
/// made 2942 records over 256 rounds): per round how many, and how many were never met before;
/// each distinct hole with the round it was first met. What [`CheckFail::Rounds`] then says:
/// a chain found a link a round (new holes every round) or one hole met again and again (a fill
/// that does not make what is asked). Questions are `Copy` and `Ord`: no text is made per round.
struct RoundHoles {
    /// `(holes, new)` of each round, in order.
    per_round: Vec<(u32, u32), Global>,
    /// Each distinct hole and the first round it was met in, sorted by the question.
    seen: Vec<(Question, u32), Global>,
}

impl RoundHoles {
    fn new() -> RoundHoles {
        RoundHoles { per_round: Vec::new_in(Global), seen: Vec::new_in(Global) }
    }

    fn round(&mut self, round: u32, missing: &[Question]) {
        let mut new = 0u32;
        for &q in missing {
            if let Err(at) = self.seen.binary_search_by(|(s, _)| s.cmp(&q)) {
                self.seen.insert(at, (q, round));
                new += 1;
            }
        }
        self.per_round.push((missing.len() as u32, new));
    }

    /// `holes per round (new): r0 12 (12), .. ; N distinct over R rounds, the last new one in
    /// round K; the last round's holes: <kind> first met in round F, ..`.
    fn said(&self, last: &[Question]) -> String {
        use core::fmt::Write as _;
        let mut s = String::from("holes per round (new among them):");
        let n = self.per_round.len();
        for (i, (holes, new)) in self.per_round.iter().enumerate() {
            // The first 4 and the last 8 rounds: the shape at both ends.
            if i < 4 || i + 8 >= n {
                let _ = write!(s, " r{i} {holes} ({new})");
            } else if i == 4 {
                s.push_str(" ..");
            }
        }
        let last_new = self.per_round.iter().rposition(|(_, new)| *new > 0);
        let _ = write!(s, "; {} distinct holes over {n} rounds", self.seen.len());
        match last_new {
            Some(r) => { let _ = write!(s, ", the last new one in round {r}"); }
            None => s.push_str(", none new"),
        }
        s.push_str("; the last round's holes:");
        for q in last.iter().take(MOST_HOLES_SHOWN) {
            let text = format!("{q:?}");
            let kind = text.split('(').next().unwrap_or("");
            match self.seen.binary_search_by(|(s, _)| s.cmp(q)) {
                Ok(at) => { let _ = write!(s, " {kind} first met in round {}", self.seen[at].1); }
                Err(_) => { let _ = write!(s, " {kind} met in no round counted"); }
            }
        }
        s
    }
}

/// [`CheckFail::Rounds`] of `owner`: loop `what` ran [`MOST_ROUNDS`] rounds, made `made`
/// records between them, and its last round still met the holes `last`. Said by path (rule
/// 19): the owner, the count, and the first [`MOST_HOLES_SHOWN`] holes, each its question's
/// kind and its item's path ([`key_shown`]). Few records over many rounds is a chain of needs
/// each found only once the one before it is made (a round per link); many is a body that
/// needs many records.
pub(crate) fn rounds_fail<'n, S: NamesSource<'n>>(r: &Records<'_, 'n, S>, owner: ItemKey, what: &str, made: u64, last: Vec<Question, Global>) -> CheckFail {
    use core::fmt::Write as _;
    let mut shown = String::new();
    let _ = write!(shown, "{what} of {} made {made} records over {MOST_ROUNDS} rounds, and its last round met {} holes", key_shown(r, owner), last.len());
    for (i, q) in last.iter().take(MOST_HOLES_SHOWN).enumerate() {
        let sep = if i == 0 { ": " } else { "; " };
        let text = format!("{q:?}");
        let kind = text.split('(').next().unwrap_or("");
        let _ = match super::body_reads::question_item(q) {
            Some(k) => write!(shown, "{sep}{kind} of {}", key_shown(r, k)),
            None => write!(shown, "{sep}{text}"),
        };
    }
    if last.len() > MOST_HOLES_SHOWN {
        let _ = write!(shown, "; and {} more", last.len() - MOST_HOLES_SHOWN);
    }
    CheckFail::Rounds { owner, last, shown: shown.into_boxed_str() }
}

/// The most holes a [`CheckFail::Rounds`] names.
const MOST_HOLES_SHOWN: usize = 4;

/// A future-like opaque can make the same auto-trait obligation surface once through its
/// opaque and once through its coroutine interior. They are one obligation at one place, so
/// retain the first diagnostic and do not report a duplicate.
fn future_goal_trait(tys: tc::PTys<'_>, d: &tc::Diag) -> Option<ItemKey> {
    if d.code != tc::DiagCode::Unsatisfied {
        return None;
    }
    let Some(tc::Because::Unmet { goal: tc::UnmetGoal::Trait { trait_, .. }, .. }) = d.because else {
        return None;
    };
    let Some(found) = d.found else { return None };
    match tys.node(found) {
        Ok(tc::PNode::Opaque(..) | tc::PNode::Coroutine { .. }) => Some(trait_),
        _ => None,
    }
}

/// Makes every fill a round of the check noted; the number dropped.
///
/// The run went on past its holes (blaze-typeck world.rs `Hole`), so a fill noted after the
/// first hole is SPECULATIVE: the real run might never ask it. A fill that cannot be made
/// (no such item, a refused head) is therefore DROPPED WITHOUT ERROR while another fill of
/// the round was made: the next run asks it for real if it matters, and meets the real
/// refusal then. A round that made nothing new is the real refusal: the least one by its
/// `Debug` text, as `make_all` names it, so the error does not depend on the order.
fn make_round<'n, S: NamesSource<'n>>(r: &mut Records<'_, 'n, S>,fills: Vec<Fill, Global>, owner: ItemKey) -> Result<u64, CheckFail> {
    let mut made = 0u64;
    let mut refused: Option<(String, MakeFail)> = None;
    let mut dropped = 0u64;
    for fill in fills {
        match make(r, fill) {
            Ok(()) => made += 1,
            Err(e) => {
                dropped += 1;
                let shown = format!("{fill:?}");
                if refused.as_ref().is_none_or(|(at, _)| shown < *at) {
                    refused = Some((shown, e));
                }
            }
        }
    }
    match refused {
        Some((_, e)) if made == 0 => Err(e.at(r, owner)),
        _ => Ok(dropped),
    }
}

/// An item's name for a finding (a type's spelling, a path's segment): its declared identifier
/// (front2 row_path.rs `shown_ident`: a body-local item's too), else its `ItemShown`; never
/// a key's `Debug`.
pub(crate) fn item_shown<'n, S: NamesSource<'n>>(r: &Records<'_, 'n, S>, k: ItemKey) -> String {
    super::row_path::shown_ident(r.names, &*r.keys, &*r.store, k)
}

/// What `k` names, for an error beside its key (rule 19: an error names the item; the key
/// stays, as tools grep it): a library item's module path and name, an anonymous constant's
/// text as filed in `store.fns` (front2/anon.rs `file_asked`), a member's container's path
/// and its name, a body-local item's declared name and the item whose body declares it; else
/// that the store gives no path. An item named by its library key is [`lib_shown`]'s.
pub(crate) fn key_shown<'n, S: NamesSource<'n>>(r: &Records<'_, 'n, S>, k: ItemKey) -> Box<str, Global> {
    key_shown_in(r.names, &*r.keys, &*r.store, k)
}

/// Opaque type `opaque` as a person names it, by the item whose head made it (rule 19): an
/// opaque key is no item of the names source, so [`item_shown`] of it said only "an item the
/// names source holds no path for (key ..)". An `async fn`'s opaque 0 with its `Output`
/// projection is `the future of `f`` (front2 async_output.rs `of`, heads.rs `fn_head`); an
/// alias's is `an `impl Trait` of type alias `A`` (front2 tait.rs `of_alias`); any other is
/// `the `impl Trait` type #i of `f``. A trait fn's `impl Trait` (not an `async fn`'s future) is
/// its trait's synthetic associated type (RFC 3425; front2 rpitit.rs): the type of `s.make()`
/// at a generic `S: Tr` keeps the opaque, and reads as the projection it reveals does,
/// [`synthetic_shown`]. A key no head made keeps its key, said so.
pub(crate) fn opaque_shown<'n, S: NamesSource<'n>>(r: &Records<'_, 'n, S>, mut opaque: ItemKey) -> String {
    // The default of a provided trait fn's `impl Trait` (front2 rpitit.rs `Twin`).
    let mut depth: usize = 0;
    while let Some(twin) = r.store.rpitit_defaults.get(&opaque) {
        depth += 1;
        opaque = twin.source;
        if depth >= 64 {
            break;
        }
    }
    let mut out = if let Some(trait_fn) = rpitit_fn(r, opaque) {
        synthetic_shown(r, trait_fn)
    } else {
        match r.store.opaques.get(&opaque) {
            Some(&(owner, 0)) if super::async_output::of(&*r.store, owner).is_some() => format!("the future of {}", key_shown(r, owner)),
            Some(&(owner, ordinal)) if super::tait::of_alias(ordinal) => format!("an `impl Trait` of type alias {}", key_shown(r, owner)),
            Some(&(owner, ordinal)) => format!("the `impl Trait` type #{ordinal} of {}", key_shown(r, owner)),
            None => format!("an `impl Trait` type no head made (key {:032x})", opaque.0),
        }
    };
    for _ in 0..depth {
        out = format!("the default body's type for {out}");
    }
    out
}

/// The trait fn whose return type holds `opaque` when it is a trait fn's `impl Trait`, its
/// trait's synthetic associated type (front2 rpitit.rs): its fn is a member of a trait whose
/// head is made. Not an `async fn`'s future (opaque 0 with an `Output` projection), which is
/// named as `the future of `f`` wherever its fn is.
fn rpitit_fn<'n, S: NamesSource<'n>>(r: &Records<'_, 'n, S>, opaque: ItemKey) -> Option<ItemKey> {
    let &(owner, ordinal) = r.store.opaques.get(&opaque)?;
    if ordinal == 0 && super::async_output::of(&*r.store, owner).is_some() {
        return None;
    }
    let Some(Origin::Member { container, kind: super::keys::MemberKind::Fn, .. }) = r.keys.origin(owner) else { return None };
    let lib = r.keys.library_in(r.names, *container)?;
    r.store.maps.traits.contains_key(&lib).then_some(owner)
}

/// A trait fn's `impl Trait` as a person names it, the opaque and the synthetic projection
/// `<S as Tr>::{..}` alike (rustc prints both as the `impl Trait` the trait fn wrote, "found
/// associated type"): `{the impl Trait returned by `path::Tr::f`}`.
fn synthetic_shown<'n, S: NamesSource<'n>>(r: &Records<'_, 'n, S>, trait_fn: ItemKey) -> String {
    format!("{{the impl Trait returned by {}}}", key_shown(r, trait_fn))
}

/// [`key_shown`] over the parts a body's world holds (`BodyWorld`: its names, key map and
/// store), for a gap that names an item (front2 foreign_closures.rs).
pub(crate) fn key_shown_in<'n, S: NamesSource<'n>>(names: S, keys: &Keys, store: &Store<'n>, k: ItemKey) -> Box<str, Global> {
    let path_of = |lib: LibKey| -> Option<String> {
        let name = names.name_of(lib)?;
        Some(match names.home_of(lib).and_then(|m| names.module_path(m)) {
            Some(module) => format!("{module}::{name}"),
            None => String::from(name),
        })
    };
    let shown = match keys.origin(k) {
        Some(Origin::Member { kind: super::keys::MemberKind::Anon, .. }) => match store.fns.get(&k) {
            Some(h) => format!("the anonymous constant `{}`", h.text),
            None => String::from("an anonymous constant not filed in the store"),
        },
        // A member by its container's path and its name (item_shown.rs `lib_path`): a trait's
        // module path and name, an impl's header as written (`core::num::<impl u32>::MAX`, a
        // constant core's `uint_impl!` writes in that impl's body). Before, an impl container
        // (no name) printed the member as "`MAX` of" the impl's `ItemShown`.
        Some(Origin::Member { container, name, .. }) => match super::item_shown::ItemShown::of(names, keys, k).path() {
            Some(p) => format!("`{p}`"),
            None => format!("`{name}` of {}", super::row_path::shown_name(names, keys, store, *container)),
        },
        _ => match store.locals.head(k) {
            // An item declared in a body (a static a macro call writes in a block, tracing's
            // `callsite2!`'s `static __CALLSITE`) has no row of the names source, so it printed
            // "an item the names source holds no path for": its declared name (the body's own
            // local rows) and the item whose body declares it, by row_path.rs's bounded walk.
            Some(local) => match super::row_path::RowPath::of(names, keys, store, k) {
                Ok(p) => format!("`{}`, declared in the body of `{}`", p.ident(), super::row_path::shown_name(names, keys, store, local.enclosing)),
                Err(no) => no.item.to_string(),
            },
            None => match keys.library_in(names, k).and_then(path_of) {
                Some(p) => format!("`{p}`"),
                None => super::item_shown::ItemShown::of(names, keys, k).to_string(),
            },
        },
    };
    shown.into_boxed_str()
}

/// [`key_shown`] of an item named by its LIBRARY key (a head's `Why::NeedConst`, so
/// [`MakeFail::Const`] and `ConstFail::Nested`). `checker_of(lib)` is the item's checker key
/// only where the key map files it as [`Origin::Item`] (a module-level item, a body-local one);
/// a member's checker key is made from its container, kind and name (keys.rs `member_key`), so
/// `checker_of` of a member's library key has no origin, and the names source's
/// `key_beginning` searches items, modules and variants, no member: tempo's tests (2026-11-21,
/// reports/tempo-tests/t0850) printed `Const { .. shown: "an item the names source holds no path
/// for (key a606c28d..)", error: Nested { .. } }`. Such an item is named from its library key
/// through the member owners table: its container (a trait's path, an impl's header as
/// written) and its name ([`super::item_shown::ItemShown::of_lib`]), a member a macro wrote
/// included (`impl u32 { uint_impl! { .. } }`: its rows are in that table too).
pub(crate) fn lib_shown<'n, S: NamesSource<'n>>(r: &Records<'_, 'n, S>, lib: LibKey) -> Box<str, Global> {
    let k = checker_of(lib);
    if r.keys.origin(k).is_some() || r.store.locals.head(k).is_some() {
        return key_shown(r, k);
    }
    // An anonymous constant a head asked for and anon.rs `file_asked` has not filed yet (no
    // origin: `eval_len_inner` returns `ConstFail::Nested` before it files the asks), such as
    // the `size_of::<Self>()` of `[u8; size_of::<Self>()]` in a fn head core's `uint_impl!`
    // writes in `impl u32`: its text as written, and the module whose head wrote it.
    let asked = r.store.maps.anon_asked.borrow().iter().find(|a| a.need == lib).map(|a| match r.names.module_path(a.module) {
        Some("") => format!("the anonymous constant `{}` (written in a head in its crate's root module)", a.text),
        Some(module) => format!("the anonymous constant `{}` (written in a head in `{module}`)", a.text),
        None => format!("the anonymous constant `{}` (written in a head in a module the names source gives no path for)", a.text),
    });
    if let Some(text) = asked {
        return text.into_boxed_str();
    }
    let shown = super::item_shown::ItemShown::of_lib(r.names, lib);
    let text = match shown.path() {
        Some(p) => format!("`{p}`"),
        None => shown.to_string(),
    };
    text.into_boxed_str()
}

/// How many of ADT `k`'s arguments `args` (of `tys`) a type is shown with: a trailing argument
/// equal to the default its parameter writes is left out, as a person writes the type and as
/// rustc prints it (`Rc<u8>`, not `Rc<u8, Global>`: alloc's `Rc<T, A: Allocator = Global>`).
/// Only a default that names no parameter (`Global`) is compared; one that does
/// (`struct S<T, U = T>`) is in the ADT's own parameters and is kept shown. Equal by spelling:
/// both sides are said by [`ty_shown`], the default in the heads' table. An ADT whose head row
/// the store does not hold shows every argument.
fn adt_args_shown<'n, S: NamesSource<'n>>(r: &Records<'_, 'n, S>, tys: tc::PTys<'_>, k: ItemKey, args: &[tc::PTy]) -> usize {
    let Some(lib) = r.keys.library_in(r.names, k) else { return args.len() };
    let Some((_, defaults)) = r.store.maps.generics.get(&lib) else { return args.len() };
    let heads = r.store.tys.view();
    let mut shown = args.len();
    while let Some(&arg) = shown.checked_sub(1).and_then(|i| args.get(i)) {
        let Some(&Some(d)) = defaults.get(shown - 1) else { break };
        if !names_no_param(heads, d, r.store.tys.alloc()) || ty_shown(r, heads, d) != ty_shown(r, tys, arg) {
            break;
        }
        shown -= 1;
    }
    shown
}

/// Whether `t` of `tys` reaches no parameter, hole or error type (a work list, capped: a
/// malformed table is "names one").
fn names_no_param(tys: tc::PTys<'_>, t: tc::PTy, alloc: blaze_own::Alloc<'_>) -> bool {
    let mut todo: blaze_own::Vec<tc::PTy, blaze_own::Alloc<'_>> = blaze_own::Vec::new_in(alloc);
    let mut kids: blaze_own::Vec<tc::PTy, blaze_own::Alloc<'_>> = blaze_own::Vec::new_in(alloc);
    todo.push(t);
    let mut steps = 0u32;
    while let Some(t) = todo.pop() {
        steps += 1;
        let Ok(node) = tys.node(t) else { return false };
        if steps > 4096 || matches!(node, tc::PNode::Param(_) | tc::PNode::ConstParam(_) | tc::PNode::Infer | tc::PNode::Error) {
            return false;
        }
        kids.clear();
        if blaze_build::instance::each_child(tys, node, &mut kids).is_err() {
            return false;
        }
        todo.extend_from_slice(&kids);
    }
    true
}

/// `t` of `tys` as Rust spells it (`Result<Out, Missing>`), by a work list: one arm per node,
/// no `Debug` of a node (rule 19). What has no source spelling is said by what it is: a
/// closure `{closure #0 of main}`, a fn item `fn item foo`, a hole `_`.
pub(super) fn ty_shown<'n, S: NamesSource<'n>>(r: &Records<'_, 'n, S>, tys: tc::PTys<'_>, t: tc::PTy) -> String {
    enum Step {
        Ty(tc::PTy),
        Lit(&'static str),
        Text(String),
    }
    /// `items` in order, `sep` between them, onto `seq`.
    fn joined(seq: &mut Vec<Step, Global>, items: &[tc::PTy], sep: &'static str) {
        for (i, &k) in items.iter().enumerate() {
            if i > 0 {
                seq.push(Step::Lit(sep));
            }
            seq.push(Step::Ty(k));
        }
    }
    let mut out = String::with_capacity(64);
    let mut todo = Vec::new_in(Global);
    todo.push(Step::Ty(t));
    // Each node is visited once per occurrence; the cap bounds a malformed (cyclic) table.
    let mut steps = 0u32;
    while let Some(step) = todo.pop() {
        steps += 1;
        if steps > 4096 {
            out.push_str("..");
            break;
        }
        let t = match step {
            Step::Lit(s) => {
                out.push_str(s);
                continue;
            }
            Step::Text(s) => {
                out.push_str(&s);
                continue;
            }
            Step::Ty(t) => t,
        };
        // What the node prints as, in order; pushed onto the work list reversed.
        let mut seq: Vec<Step, Global> = Vec::new_in(Global);
        let list = |l: tc::List| tys.list(l).unwrap_or(&[]);
        match tys.node(t) {
            Ok(tc::PNode::Adt(k, l)) => {
                seq.push(Step::Text(item_shown(r, k)));
                let args = list(l);
                let args = &args[..adt_args_shown(r, tys, k, args)];
                if !args.is_empty() {
                    seq.push(Step::Lit("<"));
                    joined(&mut seq, args, ", ");
                    seq.push(Step::Lit(">"));
                }
            }
            Ok(tc::PNode::Tuple(l)) => {
                let kids = list(l);
                seq.push(Step::Lit("("));
                joined(&mut seq, kids, ", ");
                // `(T,)`: a one-element tuple, not a parenthesized type.
                if kids.len() == 1 {
                    seq.push(Step::Lit(","));
                }
                seq.push(Step::Lit(")"));
            }
            Ok(tc::PNode::Record(_, l)) => {
                let kids = list(l);
                seq.push(Step::Lit("record("));
                joined(&mut seq, kids, ", ");
                seq.push(Step::Lit(")"));
            }
            Ok(tc::PNode::Union(l)) => {
                let kids = list(l);
                seq.push(Step::Lit("union("));
                joined(&mut seq, kids, " | ");
                seq.push(Step::Lit(")"));
            }
            Ok(tc::PNode::Ref(m, i)) => {
                seq.push(Step::Lit(if m == tc::Mutbl::Mut { "&mut " } else { "&" }));
                seq.push(Step::Ty(i));
            }
            Ok(tc::PNode::Ptr(m, i)) => {
                seq.push(Step::Lit(if m == tc::Mutbl::Mut { "*mut " } else { "*const " }));
                seq.push(Step::Ty(i));
            }
            Ok(tc::PNode::Param(i)) => seq.push(Step::Text(format!("Param({i})"))),
            Ok(tc::PNode::ConstParam(i)) => seq.push(Step::Text(format!("const parameter #{i}"))),
            // As a person names it, `impl Trait (the future of `f`)`: the bare fn's name said
            // the type was the fn's. A trait fn's `impl Trait` reads as its synthetic
            // associated type does, one text for one type (`assoc_shown`, rule 30).
            Ok(tc::PNode::Opaque(k, _)) => match rpitit_fn(r, k) {
                Some(trait_fn) => seq.push(Step::Text(synthetic_shown(r, trait_fn))),
                None if r.store.opaques.contains_key(&k) || r.store.rpitit_defaults.contains_key(&k) => {
                    seq.push(Step::Text(format!("impl Trait ({})", opaque_shown(r, k))));
                }
                None => seq.push(Step::Lit("impl Trait")),
            },
            Ok(tc::PNode::Coroutine { owner, .. }) => seq.push(Step::Text(item_shown(r, owner))),
            Ok(tc::PNode::Future(..)) => seq.push(Step::Lit("impl Future")),
            Ok(tc::PNode::Closure { owner, ordinal, .. }) => seq.push(Step::Text(format!("{{closure #{ordinal} of {}}}", item_shown(r, owner)))),
            Ok(tc::PNode::FnDef(k, l)) => {
                seq.push(Step::Text(format!("fn item {}", item_shown(r, k))));
                if l.len != 0 {
                    seq.push(Step::Lit("::<"));
                    joined(&mut seq, list(l), ", ");
                    seq.push(Step::Lit(">"));
                }
            }
            // `unsafe extern "C" fn(A, ...) -> R`: the inputs, then the output last.
            Ok(tc::PNode::FnPtr(l, h)) => {
                if h.safety == tc::Safety::Unsafe {
                    seq.push(Step::Lit("unsafe "));
                }
                if h.abi != tc::Abi::Rust {
                    seq.push(Step::Text(format!("extern \"{}\" ", h.abi.name())));
                }
                seq.push(Step::Lit("fn("));
                let kids = list(l);
                let (output, inputs) = match kids.split_last() {
                    Some((&o, i)) => (Some(o), i),
                    None => (None, kids),
                };
                joined(&mut seq, inputs, ", ");
                if h.c_variadic {
                    seq.push(Step::Lit(if inputs.is_empty() { "..." } else { ", ..." }));
                }
                seq.push(Step::Lit(")"));
                if let Some(o) = output
                    && !matches!(tys.node(o), Ok(tc::PNode::Tuple(u)) if u.len == 0)
                {
                    seq.push(Step::Lit(" -> "));
                    seq.push(Step::Ty(o));
                }
            }
            // `<Self as Trait<..>>::Name`.
            Ok(tc::PNode::Alias { trait_, name, args }) => {
                let kids = list(args);
                seq.push(Step::Lit("<"));
                match kids.split_first() {
                    Some((&self_, rest)) => {
                        seq.push(Step::Ty(self_));
                        seq.push(Step::Text(format!(" as {}", item_shown(r, trait_))));
                        if !rest.is_empty() {
                            seq.push(Step::Lit("<"));
                            joined(&mut seq, rest, ", ");
                            seq.push(Step::Lit(">"));
                        }
                    }
                    None => seq.push(Step::Text(format!("? as {}", item_shown(r, trait_)))),
                }
                seq.push(Step::Text(format!(">::{}", assoc_shown(r, trait_, name))));
            }
            Ok(tc::PNode::Infer) => seq.push(Step::Lit("_")),
            Ok(tc::PNode::ConstValue { ty, bits }) => seq.push(Step::Text(const_shown(r, tys, ty, bits))),
            Ok(tc::PNode::Never) => seq.push(Step::Lit("!")),
            Ok(n @ (tc::PNode::Bool | tc::PNode::Char | tc::PNode::Str | tc::PNode::Int(_) | tc::PNode::Uint(_) | tc::PNode::Float(_))) => {
                // types.rs `PRIMS` is the one table of built-in names; it covers all 19.
                match super::types::PRIMS.iter().find(|(_, p)| *p == n) {
                    Some(&(name, _)) => seq.push(Step::Lit(name)),
                    None => seq.push(Step::Text(format!("{n:?} (a built-in type missing from types.rs PRIMS)"))),
                }
            }
            Ok(tc::PNode::Slice(e)) => {
                seq.push(Step::Lit("["));
                seq.push(Step::Ty(e));
                seq.push(Step::Lit("]"));
            }
            Ok(tc::PNode::Array(e, n)) => {
                seq.push(Step::Lit("["));
                seq.push(Step::Ty(e));
                seq.push(Step::Lit("; "));
                seq.push(Step::Ty(n));
                seq.push(Step::Lit("]"));
            }
            // `u8 is 1..=9 | 20..=29`: the ranges in pairs.
            Ok(tc::PNode::Pat(base, ranges)) => {
                seq.push(Step::Ty(base));
                seq.push(Step::Lit(" is "));
                for (i, pair) in list(ranges).chunks(2).enumerate() {
                    if i > 0 {
                        seq.push(Step::Lit(" | "));
                    }
                    joined(&mut seq, pair, "..=");
                }
            }
            // `dyn Trait<A, Name = T> + Send`: the principal with its arguments and
            // projections, then the auto traits (portable.rs `Dynamic`'s one order).
            Ok(tc::PNode::Dynamic(l)) => {
                let kids = list(l);
                seq.push(Step::Lit("dyn "));
                let mut first = true;
                if let Some(&p) = kids.first()
                    && let Ok(tc::PNode::ExTrait(tr, a)) = tys.node(p)
                {
                    first = false;
                    seq.push(Step::Text(item_shown(r, tr)));
                    let args = list(a);
                    let mut open = false;
                    for (i, &k) in args.iter().enumerate() {
                        seq.push(Step::Lit(if i == 0 { "<" } else { ", " }));
                        seq.push(Step::Ty(k));
                        open = true;
                    }
                    for &k in kids {
                        if let Ok(tc::PNode::ExProj { trait_, name, term, .. }) = tys.node(k) {
                            seq.push(Step::Lit(if open { ", " } else { "<" }));
                            seq.push(Step::Text(format!("{} = ", assoc_shown(r, trait_, name))));
                            seq.push(Step::Ty(term));
                            open = true;
                        }
                    }
                    if open {
                        seq.push(Step::Lit(">"));
                    }
                }
                for &k in kids {
                    if let Ok(tc::PNode::ExAuto(a)) = tys.node(k) {
                        if !first {
                            seq.push(Step::Lit(" + "));
                        }
                        first = false;
                        seq.push(Step::Text(item_shown(r, a)));
                    }
                }
            }
            // A trait object's predicate met outside its `Dynamic` (never in a well-formed
            // table): said as it reads inside one.
            Ok(tc::PNode::ExTrait(tr, a)) => {
                seq.push(Step::Text(item_shown(r, tr)));
                if a.len != 0 {
                    seq.push(Step::Lit("<"));
                    joined(&mut seq, list(a), ", ");
                    seq.push(Step::Lit(">"));
                }
            }
            Ok(tc::PNode::ExProj { trait_, name, term, .. }) => {
                seq.push(Step::Text(format!("{} = ", assoc_shown(r, trait_, name))));
                seq.push(Step::Ty(term));
            }
            Ok(tc::PNode::ExAuto(a)) => seq.push(Step::Text(item_shown(r, a))),
            // An extern type: its path, as an ADT with no arguments.
            Ok(tc::PNode::Foreign(k)) => seq.push(Step::Text(item_shown(r, k))),
            // A type the check left undecided (the error type): rustc's `{unknown}`, never
            // the node's `Debug` name `Error`, which reads as a type named `Error`.
            Ok(tc::PNode::Error) => seq.push(Step::Lit("{unknown}")),
            Err(e) => seq.push(Step::Text(format!("<a type the table does not hold: {e}>"))),
        }
        todo.extend(seq.into_iter().rev());
    }
    out
}

/// The associated item `name` of `trait_` by its declared name, from the trait's members as
/// filed (rule 19: never its hash alone).
fn assoc_shown<'n, S: NamesSource<'n>>(r: &Records<'_, 'n, S>, trait_: ItemKey, name: Name) -> String {
    if let Some(a) = r.store.trait_defs.get(&trait_).and_then(|t| t.assoc.iter().find(|a| a.name == name)) {
        return item_shown(r, a.item);
    }
    // A synthetic associated type (rpitit.rs `name_of`): the `impl Trait` of a trait fn's
    // return type, filed with its trait's rows (`Store::rpitit_traits`), not with the written
    // members, and named by that fn (no written name exists). Without this every such type
    // printed as "not filed" though it was (tempo a0700, executed_state.rs:123; rule 19).
    if let Some(s) = r.store.rpitit_traits.get(&trait_).and_then(|t| t.rows.iter().find(|s| s.name == name)) {
        return synthetic_shown(r, s.trait_fn);
    }
    format!("{name:?} (an associated type of a trait whose members are not filed, so its name is not known here)")
}

/// A const generic's value as Rust writes it: `true`, `'a'`, `-3`, `255`; an enum's variant
/// by its index (`adt_const_params`). `isize` is read as 64 bits wide (the hosts in scope).
fn const_shown<'n, S: NamesSource<'n>>(r: &Records<'_, 'n, S>, tys: tc::PTys<'_>, ty: tc::PTy, bits: u128) -> String {
    match tys.node(ty) {
        Ok(tc::PNode::Bool) => String::from(if bits != 0 { "true" } else { "false" }),
        Ok(tc::PNode::Char) => match u32::try_from(bits).ok().and_then(char::from_u32) {
            Some(c) => format!("{c:?}"),
            None => format!("a `char` of bits {bits:#x}, no scalar value"),
        },
        Ok(tc::PNode::Uint(_)) => format!("{bits}"),
        Ok(tc::PNode::Int(w)) => {
            let width: u32 = match w {
                tc::IntTy::I8 => 8,
                tc::IntTy::I16 => 16,
                tc::IntTy::I32 => 32,
                tc::IntTy::I64 | tc::IntTy::Isize => 64,
                tc::IntTy::I128 => 128,
            };
            let v = ((bits << (128 - width)) as i128) >> (128 - width);
            format!("{v}")
        }
        Ok(tc::PNode::Adt(k, _)) => format!("variant #{bits} of {}", item_shown(r, k)),
        _ => format!("a constant of bits {bits:#x}"),
    }
}

/// What a diagnostic is about, printed: an unmet goal entire, the leaf where-clause first when
/// the checker names one (`Error: From<Other>` because `Result<u8, Error>: FromResidual<..>`).
/// `v`: the body, which says whether a path that named nothing is a call's callee (a function,
/// as rustc says it) or another value.
pub(super) fn diag_about<'n, S: NamesSource<'n>>(r: &Records<'_, 'n, S>, spell: &Spelling<'_>, tys: tc::PTys<'_>, v: &BodyView<'_>, d: &tc::Diag, why: &[tc::WhyStep]) -> Box<str> {
    diag_about_held(r, spell, tys, v, d, why, &[])
}

/// An unmet goal as a finding says it: "`Self: Trait<..>`", or "`<Self as Trait<..>>::Name == T`".
fn unmet_shown<'n, S: NamesSource<'n>>(r: &Records<'_, 'n, S>, tys: tc::PTys<'_>, g: tc::UnmetGoal) -> String {
    let (tc::UnmetGoal::Trait { trait_, args } | tc::UnmetGoal::Projection { trait_, args, .. }) = g;
    let all: Vec<String> = tys.list(args).map(|l| l.iter().map(|&t| ty_shown(r, tys, t)).collect()).unwrap_or_default();
    let (self_, rest) = all.split_first().map_or(("?", &[][..]), |(s, rest)| (s.as_str(), rest));
    let tr = if rest.is_empty() { item_shown(r, trait_).to_string() } else { format!("{}<{}>", item_shown(r, trait_), rest.join(", ")) };
    match g {
        tc::UnmetGoal::Trait { .. } => format!("`{self_}: {tr}`"),
        tc::UnmetGoal::Projection { trait_, name, term, .. } => {
            format!("`<{self_} as {tr}>::{} == {}`", assoc_shown(r, trait_, name), ty_shown(r, tys, term))
        }
    }
}

/// What a value an async block holds across an `.await` is, for a why-chain's
/// `CoroutineSlot` step: known only where the exact interior was made (front2
/// stalled_goals.rs, from the witness's storage rows). `words` is the whole phrase ("`r`
/// (`Rc<u8>`) is held across `tick().await`"); the places it names are `declared` and
/// `awaits`, item-relative ranges, filed as the finding's notes (`BodyDiagnostic::notes`) for
/// whoever prints findings to turn into `file:line:col`.
pub(super) struct HeldSlot {
    pub coroutine: tc::WhyDef,
    pub slot: u32,
    pub words: String,
    pub declared: Vec<tc::Range>,
    pub awaits: Vec<tc::Range>,
}

/// [`diag_about`] with what the exact interior says of each held slot (`held`): a stalled
/// goal's finding decided on the exact interior (front2 stalled_goals.rs) names WHICH value.
pub(super) fn diag_about_held<'n, S: NamesSource<'n>>(
    r: &Records<'_, 'n, S>,
    spell: &Spelling<'_>,
    tys: tc::PTys<'_>,
    v: &BodyView<'_>,
    d: &tc::Diag,
    why: &[tc::WhyStep],
    held: &[HeldSlot],
) -> Box<str> {
    let ty = |t: Option<tc::PTy>| t.map(|t| ty_shown(r, tys, t));
    let s = match d.because {
        // E0599 in rustc's words (method/suggest.rs): "the method `get` exists for `W<B>`, but
        // its trait bounds were not satisfied", then the bound (rule 19).
        Some(tc::Because::Unmet { goal, .. }) if d.code == tc::DiagCode::NoMethod => {
            let method = d.name.map_or_else(|| String::from("the method"), |n| match spell.get(n) {
                Ok(text) => format!("the method `{text}`"),
                Err(_) => String::from("the method"),
            });
            let on = ty(d.found).map_or_else(String::new, |t| format!(" for `{t}`"));
            format!("{method} exists{on}, but its trait bounds were not satisfied: {}", unmet_shown(r, tys, goal))
        }
        Some(tc::Because::Unmet { goal, leaf }) => {
            let shown = |g: tc::UnmetGoal| unmet_shown(r, tys, g);
            let unmet = match leaf {
                Some(l) => format!("{} because {}", shown(l), shown(goal)),
                None => shown(goal),
            };
            // D104: the failed trait's own words first (the leaf's, the one a person fixes),
            // the goal after them.
            match trait_words(r, tys, leaf.unwrap_or(goal)) {
                Some(words) => format!("{words} ({unmet})"),
                None => unmet,
            }
        }
        Some(tc::Because::NoLangItem(l)) => format!("no lang item {l:?}"),
        // E0425: the segment that named nothing, by its spelling (rule 19).
        Some(tc::Because::Path(miss)) => super::path_miss_shown::path_miss_shown(r, spell, d.name, super::path_miss_shown::kind_at(v, d.at), miss),
        // E0133 in rustc's words: the operation, then where it may be written. Before 2024 an
        // `unsafe fn`'s body is itself an unsafe context (`unsafe_op_in_unsafe_fn` allowed), so
        // the words name both (the Edition Guide, "unsafe_op_in_unsafe_fn warning").
        Some(tc::Because::Unsafe(op)) => {
            let place = if v.edition >= tc::Edition::E2024 { "unsafe block" } else { "unsafe function or block" };
            format!("{} is unsafe and requires {place}", unsafe_op_shown(r, op))
        }
        // Every goal's finding (E0277, E0283) carries `Unmet`; what is left is about a node, not a
        // goal: its reason by its content, then its types and the name it looked up, each only
        // where the finding has it (diag-names-something-1: blaze-typeck builds no finding with
        // nothing in it, so this never prints an empty one).
        Some(other) => {
            let why = match other {
                tc::Because::False(k) => format!("the `where const` condition {} is false here", item_shown(r, k)),
                tc::Because::Error(k) => format!("the `where const` condition {} did not evaluate", item_shown(r, k)),
                tc::Because::Generic(k) => format!("the `where const` condition {} is over a generic parameter here", item_shown(r, k)),
                tc::Because::Dyn { trait_, by, member, rule } => {
                    let at = match member.map(|n| spell.get(n)) {
                        Some(Ok(text)) => format!(" at `{text}`"),
                        Some(Err(unspelled)) => format!(" at {unspelled:?}"),
                        None => String::from(" (`Sized` is a supertrait)"),
                    };
                    format!("the trait {} is not dyn compatible: {} breaks {rule:?}{at}", item_shown(r, trait_), item_shown(r, by))
                }
                tc::Because::ArgCount { item, expected, got } => format!("{} takes {expected} generic arguments, {got} written", item_shown(r, item)),
                tc::Because::Reference { rule, says } => format!("the Reference, `{rule}`: \"{says}\""),
                tc::Because::Count { expected, got } => format!("{expected} wanted, {got} written"),
                tc::Because::NoSegment => String::from("the path written here (the body keeps no segment of it to name)"),
                tc::Because::Unmet { .. } | tc::Because::NoLangItem(_) | tc::Because::Path(_) | tc::Because::Unsafe(_) => format!("{other:?}"),
            };
            node_about(Some(why), ty(d.found), ty(d.expected), d.name.map(|n| name_about(r, spell, tys, d.found, n)))
        }
        None => node_about(None, ty(d.found), ty(d.expected), d.name.map(|n| name_about(r, spell, tys, d.found, n))),
    };
    let mut s = s;
    // Rule 19: the finding's kind by its code, so a finding with little else in it (`found
    // `{unknown}``) still says what was refused.
    s.push_str(&format!(" ({} {:?})", d.code.code(), d.code));
    // Rule 19: a finding whose range other nodes share (every node an expansion wrote has the
    // call's range) names its node: its index, kind and shape (blaze-typeck diag/node.rs).
    if let Some(node) = d.node {
        let shared = tc::range_shared(v, node);
        if shared > 1 {
            node_words(&mut s, spell, v, node, shared);
        }
    }
    // The solver's chain of why the unmet goal has no solution (blaze-typeck `Diag::why`): each
    // step under the goal, then why the last one fails by itself.
    if let Some(w) = d.why {
        match why.get(w.start as usize..(w.start as usize).saturating_add(w.len as usize)) {
            Some(steps) => why_words(&mut s, r, spell, tys, v, steps, w.leaf, held),
            None => s.push_str(&format!("; its chain of why is rows {}..+{} of a `why` column of {} rows: not this record's", w.start, w.len, why.len())),
        }
    }
    // Rule 19: an E0282 says WHAT is undecided (where its variable was made) and which goals
    // still wait on it (blaze-typeck `Diag::undecided`).
    if let Some(u) = d.undecided {
        undecided_words(&mut s, r, spell, tys, v, u);
    }
    s.into_boxed_str()
}

/// The words [`diag_about`] adds for an E0282's undecided variable: where it was made (a
/// generic argument by its item's path, a closure's parameter or return by the closure's text),
/// then each pending goal that names it, and how many more there are.
fn undecided_words<'n, S: NamesSource<'n>>(s: &mut String, r: &Records<'_, 'n, S>, spell: &Spelling<'_>, tys: tc::PTys<'_>, v: &BodyView<'_>, u: tc::UndecidedVar) {
    use core::fmt::Write;
    // A `String`'s `write!` cannot fail.
    let closure = |s: &mut String, e| {
        s.push_str("the closure `");
        let _ = tc::sketch(v, tc::Node::Expr(e), &|n| spell.get(n).ok(), s);
        s.push('`');
    };
    s.push_str("\n  undecided: ");
    match u.origin {
        tc::VarOrigin::GenericArg { item, index } => {
            let _ = write!(s, "generic argument #{index} of {} (none written at this use)", item_shown(r, item));
        }
        tc::VarOrigin::ClosureParam { closure: e, index } => {
            let _ = write!(s, "the type of parameter #{index} of ");
            closure(s, e);
            s.push_str(" (none written, none expected)");
        }
        tc::VarOrigin::ClosureReturn { closure: e } => {
            s.push_str("the return type of ");
            closure(s, e);
            s.push_str(" (none written, none expected)");
        }
        tc::VarOrigin::Projection { trait_, name } => {
            let _ = match spell.get(name) {
                Ok(text) => write!(s, "the associated type `{text}` of {}, not normalized yet (its arguments hold a variable)", item_shown(r, trait_)),
                Err(unspelled) => write!(s, "an associated type ({unspelled:?}) of {}, not normalized yet (its arguments hold a variable)", item_shown(r, trait_)),
            };
        }
        tc::VarOrigin::Other => s.push_str("a type the check made for a value, no written place named"),
    }
    let mut shown = 0u32;
    for g in u.pending.iter().flatten() {
        shown += 1;
        let _ = write!(s, "\n  pending on it: {}", unmet_shown(r, tys, *g));
    }
    if u.pending_count == 0 {
        s.push_str("\n  no goal is pending on it: nothing written here decides it");
    } else if u.pending_count > shown {
        let _ = write!(s, "\n  and {} more goals pending on it", u.pending_count - shown);
    }
}

/// The words [`diag_about`] adds for a finding's why-chain (blaze-typeck `Diag::why`): a line
/// per step under the goal, what the step is of the one above (an impl's where-clause by the
/// impl's path, a field by its ADT's, a closure or async block by its text, another body's by
/// its owner's path, a held value by what the exact interior says of it in `held`) and the
/// goal it needs, then why the last fails by itself. Rule 19: each thing by what it is, never
/// Debug.
#[expect(clippy::too_many_arguments, reason = "diag_about's, and the chain's")]
fn why_words<'n, S: NamesSource<'n>>(s: &mut String, r: &Records<'_, 'n, S>, spell: &Spelling<'_>, tys: tc::PTys<'_>, v: &BodyView<'_>, steps: &[tc::WhyStep], leaf: tc::WhyLeaf, held: &[HeldSlot]) {
    use core::fmt::Write;
    // A `String`'s `write!` cannot fail.
    let def = |s: &mut String, what: &str, d: tc::WhyDef| match d {
        tc::WhyDef::Expr(e) => {
            let _ = write!(s, "the {what} `");
            let _ = tc::sketch(v, tc::Node::Expr(e), &|n| spell.get(n).ok(), s);
            s.push('`');
        }
        tc::WhyDef::Of { owner, ordinal } => {
            let _ = write!(s, "{what} #{ordinal} of {}", item_shown(r, owner));
        }
    };
    for step in steps.iter().skip(1) {
        s.push_str("\n  because ");
        match step.reached {
            tc::WhyReached::Root => s.push_str("the goal itself"),
            tc::WhyReached::ImplClause { impl_, clause } => {
                let _ = write!(s, "where-clause #{clause} of {}", item_shown(r, impl_));
            }
            tc::WhyReached::Field { adt, field } => {
                let _ = write!(s, "field #{field} of {}", item_shown(r, adt));
            }
            tc::WhyReached::AdtArg { adt, arg } => {
                let _ = write!(s, "type argument #{arg} of {} (its fields hold it)", item_shown(r, adt));
            }
            tc::WhyReached::RefTarget => s.push_str("the target of the reference"),
            tc::WhyReached::PtrTarget => s.push_str("the pointee of the raw pointer"),
            tc::WhyReached::Element => s.push_str("the element type"),
            tc::WhyReached::TupleElement { index } => {
                let _ = write!(s, "tuple element #{index}");
            }
            tc::WhyReached::Capture { closure, capture } => {
                let _ = write!(s, "capture #{capture} of ");
                def(s, "closure", closure);
            }
            // On the exact interior (front2 stalled_goals.rs) the slot is a value a person
            // wrote, said from its storage row; on the check's superset interior (blaze-typeck
            // rules/coroutine.rs) it names none, so the value is said by its type (the goal).
            tc::WhyReached::CoroutineSlot { coroutine, slot } => match held.iter().find(|h| h.coroutine == coroutine && h.slot == slot) {
                Some(h) => {
                    let _ = write!(s, "{} by ", h.words);
                    def(s, "async block", coroutine);
                }
                None => {
                    s.push_str("a value held by ");
                    def(s, "async block", coroutine);
                }
            },
            tc::WhyReached::Captures { closure } => {
                s.push_str("the captures of ");
                def(s, "closure", closure);
            }
            tc::WhyReached::Interior { coroutine } => {
                s.push_str("what is held by ");
                def(s, "async block", coroutine);
            }
            tc::WhyReached::Phantom => s.push_str("the type `PhantomData` stands for"),
            tc::WhyReached::Hidden { opaque } => {
                let _ = write!(s, "the hidden type of {}", opaque_shown(r, opaque));
            }
            tc::WhyReached::Other { nth: u32::MAX } => s.push_str("a goal a nested goal's answer added"),
            tc::WhyReached::Other { nth } => {
                let _ = write!(s, "requirement #{nth} of the rule that applied");
            }
        }
        match step.goal {
            Some(g) => {
                let _ = write!(s, " needs {}", unmet_shown(r, tys, g));
            }
            None => s.push_str(" needs a goal that is no trait bound, or names a type the walk left open"),
        }
    }
    s.push_str("\n  and ");
    match leaf {
        tc::WhyLeaf::NoImpl => s.push_str("no impl, bound or rule gives that"),
        tc::WhyLeaf::NegativeImpl { impl_ } => {
            let _ = write!(s, "{} says it does not hold (a negative impl)", item_shown(r, impl_));
        }
        tc::WhyLeaf::Refused { first_impl: Some(i), count } => {
            let _ = write!(s, "each of its {count} candidates is for other types (the first: {})", item_shown(r, i));
        }
        tc::WhyLeaf::Refused { first_impl: None, count } => {
            let _ = write!(s, "each of its {count} candidates was refused by its rule");
        }
        tc::WhyLeaf::BuiltinNo => s.push_str("the language's own rule for that type refuses it"),
        tc::WhyLeaf::Cycle => s.push_str("that needs itself (a cycle)"),
        tc::WhyLeaf::NotKept => s.push_str("the solver's walk stops there (its bound on rows or depth)"),
        // An undecided goal's leaves: why the last goal cannot be decided yet.
        tc::WhyLeaf::Unresolved { term } => {
            let _ = write!(s, "its type #{term} is not known yet (annotate it)");
        }
        tc::WhyLeaf::Ambiguous { first, second, count } => {
            let _ = write!(s, "{count} candidates may each hold and none is chosen: {} and {}", cand_shown(r, first), cand_shown(r, second));
        }
        tc::WhyLeaf::Limit { hit: tc::WhyLimit::Depth { depth, limit } } => {
            let _ = write!(s, "the goals stand {depth} deep, at the recursion limit {limit}");
        }
        tc::WhyLeaf::Limit { hit: tc::WhyLimit::Fixpoint { reruns } } => {
            let _ = write!(s, "a cycle's answer did not settle in {reruns} reruns");
        }
        tc::WhyLeaf::Limit { hit: tc::WhyLimit::Rounds { rounds } } => {
            let _ = write!(s, "its nested goals kept changing for {rounds} rounds");
        }
        tc::WhyLeaf::HiddenUnknown { opaque } => {
            let _ = write!(s, "the hidden type of {} is not known yet", opaque_shown(r, opaque));
        }
        tc::WhyLeaf::InteriorUnknown { coroutine } => {
            s.push_str("what is held by ");
            def(s, "async block", coroutine);
            s.push_str(" is not decided yet");
        }
        tc::WhyLeaf::CapturesUnknown { closure } => {
            s.push_str("the captures of ");
            def(s, "closure", closure);
            s.push_str(" are not decided yet");
        }
        tc::WhyLeaf::Unnormalized { alias } => {
            let _ = write!(s, "the associated type {} does not normalize yet", item_shown(r, alias));
        }
        tc::WhyLeaf::RuleUndecided { cand } => {
            let _ = write!(s, "the rule of {} cannot decide it yet", cand_shown(r, cand));
        }
    }
}

/// A candidate a why-chain's leaf names, by what it is: an impl by its path, a where-clause
/// by its place among those in scope.
fn cand_shown<'n, S: NamesSource<'n>>(r: &Records<'_, 'n, S>, c: tc::WhyCand) -> String {
    match c {
        tc::WhyCand::Impl { impl_ } => format!("the impl {}", item_shown(r, impl_)),
        tc::WhyCand::WhereClause { nth } => format!("where-clause #{nth} in scope"),
        tc::WhyCand::AliasBound => String::from("a bound of the alias or opaque type"),
        tc::WhyCand::Builtin => String::from("the language's own rule"),
        tc::WhyCand::Unknowable => String::from("an impl another crate may write"),
    }
}

/// `, at expression #i (a cast; its range is shared by n nodes): `{integer} as _``: the words
/// [`diag_about`] adds for a node its range does not name.
fn node_words(s: &mut String, spell: &Spelling<'_>, v: &BodyView<'_>, node: tc::Node, shared: usize) {
    use core::fmt::Write;
    let (what, index) = match node {
        tc::Node::Expr(e) => ("expression", e.0),
        tc::Node::Pat(p) => ("pattern", p.0),
    };
    let form = tc::node_form(v, node).unwrap_or("a node the body does not hold");
    // A `String`'s `write!` cannot fail.
    let _ = write!(s, ", at {what} #{index} ({form}; its range is shared by {shared} nodes): `");
    let _ = tc::sketch(v, node, &|n| spell.get(n).ok(), s);
    s.push('`');
}

/// An operation only `unsafe` code may do, in rustc's words (E0133 and the
/// `unsafe-op-in-unsafe-fn` lint say it alike), naming the item where there is one (rule 19).
pub(crate) fn unsafe_op_shown<'n, S: NamesSource<'n>>(r: &Records<'_, 'n, S>, op: tc::UnsafeOp) -> String {
    match op {
        tc::UnsafeOp::Call(Some(k)) => format!("call to unsafe function `{}`", item_shown(r, k)),
        tc::UnsafeOp::Call(None) => String::from("call to unsafe function"),
        tc::UnsafeOp::TargetFeatureCall(k) => format!("call to function `{}` with `#[target_feature]`", item_shown(r, k)),
        tc::UnsafeOp::RawDeref => String::from("dereference of raw pointer"),
        tc::UnsafeOp::MutStatic(k) => format!("use of mutable static `{}`", item_shown(r, k)),
        tc::UnsafeOp::ExternStatic(k) => format!("use of extern static `{}`", item_shown(r, k)),
        tc::UnsafeOp::UnionField => String::from("access to union field"),
        tc::UnsafeOp::InlineAsm => String::from("use of inline assembly"),
    }
}

/// The name a finding looked up, by its spelling: the body's where the body wrote it, else the
/// found type's own head where that is an ADT (E0063's unwritten field: the ADT declares it,
/// the body never wrote it; rule 19). Unspelled in both, it is said so with both whys.
fn name_about<'n, S: NamesSource<'n>>(r: &Records<'_, 'n, S>, spell: &Spelling<'_>, tys: tc::PTys<'_>, found: Option<tc::PTy>, n: tc::Name) -> String {
    let unspelled = match spell.get(n) {
        Ok(text) => return format!("about `{text}`"),
        Err(u) => u,
    };
    let adt = match found.map(|t| tys.node(t)) {
        Some(Ok(tc::PNode::Adt(k, _))) => k,
        _ => return format!("about {unspelled:?} (a name the body does not spell)"),
    };
    let Some(lib) = r.keys.library_in(r.names, adt) else {
        return format!("about {unspelled:?} (a name the body does not spell, in {} which is no library item)", item_shown(r, adt));
    };
    match super::spell::spell_in_item(r.names, lib, n) {
        Ok(text) => format!("about `{text}`"),
        Err(why) => format!("about {unspelled:?} (a name neither the body nor {}'s head spells: {why:?})", item_shown(r, adt)),
    }
}

/// A node's finding in words: its reason where it has one, then the type found, the type
/// expected and the name looked up (spelled by [`name_about`]), each only where the finding
/// holds it. A finding holding none of them is not built (blaze-typeck diag/about.rs); one met
/// anyway is said to be empty, never printed as `None`s.
fn node_about(why: Option<String>, found: Option<String>, expected: Option<String>, name: Option<String>) -> String {
    let mut parts: Vec<String, Global> = Vec::new_in(Global);
    parts.extend(why);
    parts.extend(found.map(|t| format!("found `{t}`")));
    parts.extend(expected.map(|t| format!("expected `{t}`")));
    parts.extend(name);
    if parts.is_empty() {
        return String::from("a finding with nothing in it (no reason, type or name: a checker bug)");
    }
    parts.join(", ")
}

/// The words a trait gives by `#[diagnostic::on_unimplemented]` for its bound unmet at `g`
/// ([`super::on_unimplemented`]): the message, `: ` the label, and `; note: ` each note, with
/// `{Self}` the self type shown and `{This}` the trait's name. `None` when `g` is a projection
/// or the trait has no such attribute (or no text). An attribute that does not read is said in
/// the finding, never passed over.
fn trait_words<'n, S: NamesSource<'n>>(r: &Records<'_, 'n, S>, tys: tc::PTys<'_>, g: tc::UnmetGoal) -> Option<String> {
    use super::on_unimplemented::{Lit, Words, find, write_value};
    let tc::UnmetGoal::Trait { trait_, args } = g else { return None };
    let lib = r.keys.library_in(r.names, trait_)?;
    let text = r.names.signature_text_of(lib)?;
    let this = item_shown(r, trait_);
    // The goal's arguments shown, `Self` first: `{Self}` is the first, a parameter's name
    // (`{Rhs}`) its own, by the index the trait's header gives it (heads.rs `param_names`).
    let shown: Vec<String, Global> = tys.list(args).map(|l| l.iter().map(|&t| ty_shown(r, tys, t)).collect()).unwrap_or_default();
    let mut params: Vec<(Name, u32), Global> = Vec::new_in(Global);
    let params_read = super::heads::lower(r.names, lib, text.text)
        .map_err(|e| format!("{e:?}"))
        .and_then(|low| super::heads::param_names(&low.view(), 1, &mut params, super::heads::Consts::Keep).map_err(|e| format!("{e:?}")));
    let unread = |why: &dyn core::fmt::Display| Some(format!("the trait `{this}`'s `#[diagnostic::on_unimplemented]` does not read ({why})"));
    if let Err(e) = params_read {
        return unread(&format_args!("its generic parameters' names: {e}"));
    }
    let fill = |name: &str| match name {
        "Self" => shown.first().map(|s| s.as_str()),
        "This" => Some(this.as_str()),
        _ => params.iter().find(|(n, _)| *n == Name::of(name)).and_then(|&(_, i)| shown.get(i as usize)).map(|s| s.as_str()),
    };
    let words = match find(text.text).and_then(|found| found.map(Words::of).transpose()) {
        Ok(Some(words)) => words,
        Ok(None) => return None,
        Err(e) => return unread(&e),
    };
    let mut out = String::new();
    let put = |lit: Lit<'_>, out: &mut String| match write_value(lit, 0, &fill, out) {
        Ok(read) => read.map_err(|e| format!("{e}")),
        Err(core::fmt::Error) => Err(String::from("a String refused a write")),
    };
    let mut wrote = Ok(());
    match words.message {
        Some(message) => wrote = put(message, &mut out),
        None => out.push_str("an unmet bound"),
    }
    if let Some(label) = words.label {
        out.push_str(": ");
        wrote = wrote.and_then(|()| put(label, &mut out));
    }
    let notes = words.notes(|note| {
        out.push_str("; note: ");
        if wrote.is_ok() {
            wrote = put(note, &mut out);
        }
    });
    match (notes, wrote) {
        (Ok(()), Ok(())) => Some(out),
        (Err(e), _) => unread(&e),
        (_, Err(e)) => unread(&e),
    }
}

/// The world a body written at `scope` is checked in, over `r`'s records as they are now.
pub(super) fn world_at<'w, 'n, S: NamesSource<'n>>(r: &'w Records<'_, 'n, S>, scope: &'w BodyScope<'w>) -> BodyWorld<'w, 'n, S> {
    BodyWorld {
        names: r.names,
        keys: &*r.keys,
        index: r.index,
        store: &*r.store,
        buckets: r.buckets.as_ref(),
        owner: scope.owner,
        module: scope.module,
        params: scope.params,
        self_: scope.self_,
        spell: scope.spell,
        traits: scope.traits,
        blocks: scope.blocks,
        path_marks: scope.path_marks,
        path_ns: scope.path_ns,
        fills: core::cell::RefCell::new(Vec::new_in(Global)),
        gaps: core::cell::RefCell::new(Vec::new_in(Global)),
        reveal: super::opaque_auto::Reveal::InBody,
    }
}

/// What a call site runs, from the checker's recorded callee and its arguments (the Reference,
/// "Method-call expressions" and "Trait objects"; instance resolution picks the body).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    /// This fn's body: a free fn, an impl's member, or, for a trait's method on a concrete
    /// receiver, the selected impl's method (or the trait's provided one it does not override).
    Body(ItemKey),
    /// A tuple struct's or variant's constructor: the language's, no body to walk.
    Ctor(ItemKey),
    /// A trait's method on a type parameter (or a projection of one): the impl is picked where
    /// the instance is made, by the caller's arguments; nothing to walk at this site.
    AtInstance(ItemKey),
    /// A trait's method on `dyn Trait`: the impl is reached where the object is made (the
    /// unsizing that builds its vtable); nothing to walk at this site.
    AtObject(ItemKey),
}

/// What the call of `item` at `args` (in `ground`, the calling body's checked types) runs, in
/// the calling body's world `scope`; the fills the selection needs are made on the way.
pub fn callee_target<'n, S: NamesSource<'n>>(r: &mut Records<'_, 'n, S>,scope: &BodyScope<'_>, owner: ItemKey, item: ItemKey, args: tc::List, ground: &mut tc::PortableTys<'_>) -> Result<Target, CheckFail> {
    if r.store.ctors.contains_key(&item) {
        return Ok(Target::Ctor(item));
    }
    // A member whose container is not an impl is a trait's.
    let of_trait = match r.keys.origin(item) {
        Some(Origin::Member { container, .. }) => !r.store.impls.contains_key(container),
        _ => false,
    };
    if !of_trait {
        return Ok(Target::Body(item));
    }
    let self_ty = ground.view().list(args).map_err(|error| CheckFail::Check { owner, error })?.first().copied();
    if let Some(t) = self_ty {
        match ground.view().node(t).map_err(|error| CheckFail::Check { owner, error })? {
            tc::PNode::Param(_) | tc::PNode::Alias { .. } => return Ok(Target::AtInstance(item)),
            tc::PNode::Dynamic(_) => return Ok(Target::AtObject(item)),
            _ => {}
        }
    }
    // A trait's fn at arguments that hold a type parameter below the top (`Clone::clone` on
    // `Vec<T>`) is selected where the instance is made, as on a bare parameter: an impl picked
    // here would be picked at types that are not the instance's. The impls of the self type's
    // shape are walked by reach.rs `Instances`.
    if holds_param(ground.view(), args).map_err(|error| CheckFail::Check { owner, error })? {
        return Ok(Target::AtInstance(item));
    }
    // A self type whose `Clone` (and `Copy`) the LANGUAGE may give (below): the build asks
    // `Self: Copy` of its clone (a `Copy` self's clone is its bytes: clone_shim.rs
    // `Clones::note`) in a world that makes no records, so it is asked here first and the
    // records its answer reads are made by this loop's fills. `Copy`'s key is filed in the key
    // map (no body need name the trait for the world to find its record).
    let builtin = match self_ty {
        Some(t) => match ground.view().node(t).map_err(|error| CheckFail::Check { owner, error })? {
            tc::PNode::Tuple(_)
            | tc::PNode::Record(..)
            | tc::PNode::Union(_)
            | tc::PNode::Closure { .. }
            | tc::PNode::FnDef(..)
            | tc::PNode::FnPtr(..) => true,
            tc::PNode::Adt(adt, _) => r.store.is_auto_copy_pub(r.names, r.keys, adt),
            _ => false,
        },
        None => false,
    };
    let copy = match builtin {
        false => None,
        true => copy_trait(r, owner)?,
    };
    let alloc = r.store.tys.alloc();
    // For a refusal past the last round ([`rounds_fail`]).
    let (mut made, mut hole) = (0u64, None);
    for _ in 0..MOST_ROUNDS {
        // A callee resolved at ground types after its body has checked is post-analysis: it
        // sees every opaque's hidden type, not only the body's leak list.
        let world = BodyWorld { reveal: super::opaque_auto::Reveal::All, ..world_at(r, scope) };
        let mut out = tc::resolve_instance(&world, item, args, ground, alloc);
        // A trait the LANGUAGE implements at this self type (the Reference, "Special types and
        // traits": `Clone` of a tuple, a closure, a fn item, a fn pointer): no impl's text holds
        // it, the solver's builtin rule does. Its fn is built at the instance (blaze-ladder
        // clone_shim.rs), and the impls its fields' calls take are reached at their shapes as
        // for a call at an instance (reach/instances.rs).
        if let Err(tc::ResolveError::NoImpl { trait_, .. }) = out
            && builtin
        {
            let mut asked: Vec<tc::PTy, Global> = Vec::new_in(Global);
            asked.extend_from_slice(ground.view().list(args).map_err(|error| CheckFail::Check { owner, error })?);
            let given = tc::holds(&world, trait_, &asked, ground, alloc).and_then(|given| match (given, copy) {
                (true, Some(copy)) => tc::holds(&world, copy, &asked[..1], ground, alloc).map(|_| true),
                (given, _) => Ok(given),
            });
            match given {
                Ok(true) if world.gaps.borrow().is_empty() => return Ok(Target::AtInstance(item)),
                Ok(_) => {}
                Err(e) => out = Err(e),
            }
        }
        let mut fills = world.fills.into_inner();
        if r.order == BuildOrder::Reversed {
            fills.reverse();
        }
        let gaps = world.gaps.into_inner();
        match out {
            Ok(_) if !gaps.is_empty() => return Err(CheckFail::Gap { owner, gaps }),
            Ok(res) => return Ok(Target::Body(res.item)),
            Err(tc::ResolveError::Missing(q)) if !fills.is_empty() => {
                made += fills.len() as u64;
                hole = Some(q);
                make_all(r, fills, owner)?
            }
            Err(_) if !gaps.is_empty() => return Err(CheckFail::Gap { owner, gaps }),
            Err(error) => return Err(CheckFail::Resolve { owner, item, error, args: shown_args(ground.view(), args) }),
        }
    }
    let mut last = Vec::new_in(Global);
    last.extend(hole);
    Err(rounds_fail(r, owner, &format!("instance resolution of the call of {}", key_shown(r, item)), made, last))
}

/// Resolves a trait method `item` at concrete ground `args` (interned in `r.store.tys`) to its
/// selected impl method `(impl_item, impl_args)` (with `impl_args` copied into `r.store.tys`).
/// Returns `Ok(None)` when `item` is not a trait method or resolves to no distinct impl body.
pub(crate) fn resolve_instance_full<'n, S: NamesSource<'n>>(
    r: &mut Records<'_, 'n, S>,
    item: ItemKey,
    args: &[tc::PTy],
) -> Result<Option<(ItemKey, blaze_own::Vec<tc::PTy, blaze_own::Alloc<'static>>)>, CheckFail> {
    let container = match r.keys.origin(item) {
        Some(Origin::Member { container, .. }) if !r.store.impls.contains_key(container) => *container,
        _ => return Ok(None),
    };
    let Some(module) = r.keys.library_in(r.names, container) else { return Ok(None) };
    let alloc = r.store.tys.alloc();
    let mut ground = tc::PortableTys::new_in(alloc);
    let mut g_args: blaze_own::Vec<tc::PTy, blaze_own::Alloc<'static>> =
        blaze_own::Vec::with_capacity_in(args.len(), alloc);
    for &t in args {
        let Some(copied) = super::inline_for::copy_ty_into(r.store.tys.view(), t, &mut ground, None, alloc) else {
            return Ok(None);
        };
        g_args.push(copied);
    }
    let Ok(args_list) = ground.intern_list(&g_args) else { return Ok(None) };
    let spell = Spelling::fixed();
    let blocks = super::block_traits::BlockTraits::module(alloc);
    let self_ = SelfTy::None;
    let scope = BodyScope {
        owner: item,
        module,
        params: &[],
        self_: &self_,
        spell: &spell,
        traits: &[],
        blocks: &blocks,
        path_marks: &[],
        path_ns: &[],
    };
    for _ in 0..MOST_ROUNDS {
        let world = BodyWorld { reveal: super::opaque_auto::Reveal::All, ..world_at(r, &scope) };
        let out = tc::resolve_instance(&world, item, args_list, &mut ground, alloc);
        let mut fills = world.fills.into_inner();
        if r.order == BuildOrder::Reversed {
            fills.reverse();
        }
        match out {
            Ok(res) => {
                if res.item == item {
                    return Ok(None);
                }
                let Ok(raw_list) = ground.view().list(res.args) else { return Ok(None) };
                let mut resolved_args: blaze_own::Vec<tc::PTy, blaze_own::Alloc<'static>> =
                    blaze_own::Vec::with_capacity_in(raw_list.len(), alloc);
                for &t in raw_list {
                    let Some(copied) = super::inline_for::copy_ty_into(ground.view(), t, &mut r.store.tys, None, alloc) else {
                        return Ok(None);
                    };
                    resolved_args.push(copied);
                }
                return Ok(Some((res.item, resolved_args)));
            }
            Err(tc::ResolveError::Missing(_)) if !fills.is_empty() => {
                make_all(r, fills, item)?;
            }
            Err(_) => return Ok(None),
        }
    }
    Ok(None)
}

/// The program's `Copy` trait, its key filed in the key map (no body need name the trait for
/// the world to find its record); `None` in a program that declares none (`no_core`).
fn copy_trait<'n, S: NamesSource<'n>>(r: &mut Records<'_, 'n, S>, owner: ItemKey) -> Result<Option<ItemKey>, CheckFail> {
    match super::langs::find(r.index, crate::resolve::Paths { names: r.names }, tc::LangItem::Copy) {
        Ok(Some(lib)) => Ok(Some(r.keys.item(lib).map_err(|clash| CheckFail::Head { owner, error: HeadError::Key(clash) })?)),
        Ok(None) => Ok(None),
        Err(why) => {
            let mut gaps = Vec::new_in(Global);
            gaps.push(Gap::Lang(tc::LangItem::Copy, why));
            Err(CheckFail::Gap { owner, gaps })
        }
    }
}

/// For a builtin `clone` at a closure of body `owner` (checked as `c`) called at `args`
/// ([`callee_target`]'s `AtInstance`): asks `Copy` of each capture held by value, so the
/// records the build's question reads are made here (clone_shim.rs `Clones::note` asks a
/// closure's `Copy` capture by capture, in a world that makes none: the solver's closure type
/// has no upvars yet). A capture by `&` is `Copy` and by `&mut` is not, with nothing to read.
/// A closure of another body, or one whose types hold a parameter, is asked at its instance,
/// where no record is made: the build refuses it by the record it missed.
pub fn reach_capture_copy<'n, S: NamesSource<'n>>(r: &mut Records<'_, 'n, S>, scope: &BodyScope<'_>, owner: ItemKey, c: &mut tc::Checked<'_>, args: tc::List) -> Result<(), CheckFail> {
    let shown = |e| CheckFail::Check { owner, error: e };
    let Some(&me) = c.types.view().list(args).map_err(shown)?.first() else { return Ok(()) };
    let tc::PNode::Closure { owner: of, ordinal, .. } = c.types.view().node(me).map_err(shown)? else { return Ok(()) };
    if of != owner || holds_param(c.types.view(), args).map_err(shown)? {
        return Ok(());
    }
    let Some(&e) = c.closures.column::<0>().get(ordinal as usize) else { return Ok(()) };
    let mut held: Vec<tc::PTy, Global> = Vec::new_in(Global);
    for (p, by) in blaze_build::layout::captures(c, e) {
        if matches!(by, tc::CaptureKind::ByValue)
            && let Some(&t) = c.local_ty.get(p.ix())
        {
            held.push(t);
        }
    }
    if held.is_empty() {
        return Ok(());
    }
    let Some(copy) = copy_trait(r, owner)? else { return Ok(()) };
    let alloc = r.store.tys.alloc();
    for &t in held.iter() {
        let mut done = false;
        // For a refusal past the last round ([`rounds_fail`]).
        let (mut made, mut hole) = (0u64, None);
        for _ in 0..MOST_ROUNDS {
            let world = BodyWorld { reveal: super::opaque_auto::Reveal::All, ..world_at(r, scope) };
            let out = tc::holds(&world, copy, &[t], &mut c.types, alloc);
            let mut fills = world.fills.into_inner();
            if r.order == BuildOrder::Reversed {
                fills.reverse();
            }
            let gaps = world.gaps.into_inner();
            match out {
                Ok(_) if gaps.is_empty() => {
                    done = true;
                    break;
                }
                Ok(_) => return Err(CheckFail::Gap { owner, gaps }),
                Err(tc::ResolveError::Missing(q)) if !fills.is_empty() => {
                    made += fills.len() as u64;
                    hole = Some(q);
                    make_all(r, fills, owner)?
                }
                Err(_) if !gaps.is_empty() => return Err(CheckFail::Gap { owner, gaps }),
                // Shown at the closure's arguments: the capture is one of its environment's.
                Err(error) => return Err(CheckFail::Resolve { owner, item: copy, error, args: shown_args(c.types.view(), args) }),
            }
        }
        if !done {
            let mut last = Vec::new_in(Global);
            last.extend(hole);
            return Err(rounds_fail(r, owner, "whether a closure's capture is `Copy`", made, last));
        }
    }
    Ok(())
}

/// Whether any type of `args` holds a type or const parameter (or a projection or opaque type,
/// which name one in a generic body), by a work list.
fn holds_param(v: tc::PTys<'_>, args: tc::List) -> Result<bool, tc::CheckError> {
    let mut work: Vec<tc::PTy, Global> = Vec::new_in(Global);
    work.extend_from_slice(v.list(args)?);
    while let Some(t) = work.pop() {
        let lists = match v.node(t)? {
            tc::PNode::Param(_) | tc::PNode::ConstParam(_) | tc::PNode::Alias { .. } | tc::PNode::Opaque(..) => return Ok(true),
            tc::PNode::Bool
            | tc::PNode::Char
            | tc::PNode::Int(_)
            | tc::PNode::Uint(_)
            | tc::PNode::Float(_)
            | tc::PNode::Str
            | tc::PNode::Never
            | tc::PNode::Infer
            | tc::PNode::Error
            | tc::PNode::ExAuto(_)
            | tc::PNode::Foreign(_) => None,
            tc::PNode::Closure { args: l, .. } | tc::PNode::Coroutine { args: l, .. } => Some(l),
            tc::PNode::Ref(_, a) | tc::PNode::Ptr(_, a) | tc::PNode::Slice(a) | tc::PNode::ConstValue { ty: a, .. } => {
                work.push(a);
                None
            }
            tc::PNode::Array(a, b) | tc::PNode::Future(_, a, b) => {
                work.extend([a, b]);
                None
            }
            tc::PNode::Pat(a, l) => {
                work.push(a);
                Some(l)
            }
            tc::PNode::ExProj { args: l, term, .. } => {
                work.push(term);
                Some(l)
            }
            tc::PNode::Tuple(l) | tc::PNode::Record(_, l) | tc::PNode::Union(l) | tc::PNode::Adt(_, l) | tc::PNode::FnDef(_, l) | tc::PNode::FnPtr(l, _) | tc::PNode::Dynamic(l) | tc::PNode::ExTrait(_, l) => Some(l),
        };
        if let Some(l) = lists {
            work.extend_from_slice(v.list(l)?);
        }
    }
    Ok(false)
}

/// `args` for a failure's text: each argument's node and, for a node over a list, its
/// children's nodes (two levels, no recursion).
fn shown_args(v: tc::PTys<'_>, args: tc::List) -> Box<str> {
    let mut out = String::new();
    let Ok(list) = v.list(args) else { return Box::from("(a malformed list)") };
    for &t in list {
        let node = v.node(t);
        out.push_str(&format!("{node:?}"));
        let kids = match node {
            Ok(tc::PNode::Adt(_, l) | tc::PNode::Dynamic(l) | tc::PNode::FnDef(_, l)) => v.list(l).ok(),
            _ => None,
        };
        if let Some(kids) = kids {
            out.push_str(" of [");
            for &k in kids {
                out.push_str(&format!("{:?}; ", v.node(k)));
            }
            out.push(']');
        }
        out.push_str(", ");
    }
    out.into_boxed_str()
}

/// A fill's failure, before the loop names the body.
pub(super) enum MakeFail {
    Head(HeadError),
    Buckets(Unfilled),
    Tables(TablesReadError),
    Question(ImplQuestionError),
    Const(LibKey, ConstFail),
}

impl From<AdmitError> for MakeFail {
    fn from(error: AdmitError) -> MakeFail {
        match error {
            AdmitError::Tables(error) => MakeFail::Tables(error),
            AdmitError::Question(error) => MakeFail::Question(error),
        }
    }
}

impl MakeFail {
    fn at<'n, S: NamesSource<'n>>(self, r: &Records<'_, 'n, S>, owner: ItemKey) -> CheckFail {
        match self {
            MakeFail::Head(error) => CheckFail::Head { owner, error },
            MakeFail::Buckets(error) => CheckFail::Buckets { owner, error },
            MakeFail::Tables(error) => CheckFail::Tables { owner, error },
            MakeFail::Question(error) => CheckFail::ImplQuestion { owner, error },
            MakeFail::Const(key, error) => CheckFail::Const { owner, key, shown: lib_shown(r, key), error: Box::new_in(error, Global) },
        }
    }
}

/// The most constants one fill may need evaluated before it is made (rule 18).
const MOST_CONSTS: u32 = 64;

/// Builds a [`CheckFail::Const`] carrying [`ConstFail::Nested`] for `key` on `owner`.
pub(super) fn const_nested_fail<'n, S: NamesSource<'n>>(r: &Records<'_, 'n, S>, owner: ItemKey, key: LibKey) -> CheckFail {
    MakeFail::Const(key, ConstFail::Nested { item: checker_of(key), shown: lib_shown(r, key) }).at(r, owner)
}

/// [`make`] for front2/const_qpath.rs (the trait's members and the impl candidates a
/// qualified constant leaf is selected from): the fill's refusal as the check's, naming
/// `owner`.
pub(super) fn make_for<'n, S: NamesSource<'n>>(r: &mut Records<'_, 'n, S>, fill: Fill, owner: ItemKey) -> Result<(), CheckFail> {
    match make(r, fill) {
        Ok(()) => Ok(()),
        Err(e) => Err(e.at(r, owner)),
    }
}

/// Makes `fill`; a head whose array length names a constant ([`Why::NeedConst`]) has that
/// constant evaluated (`Records::consts`) and the fill made again. While a constant is being
/// evaluated the evaluator is taken out of `r`, so one needed inside it is
/// [`ConstFail::Nested`] naming it: never a nested evaluation.
fn make<'n, S: NamesSource<'n>>(r: &mut Records<'_, 'n, S>,fill: Fill) -> Result<(), MakeFail> {
    let mut asked = 0;
    loop {
        let c = match make_once(r, fill) {
            Err(MakeFail::Head(HeadError::Item(_, Why::NeedConst(c, _)))) => c,
            other => return other,
        };
        asked += 1;
        if asked > MOST_CONSTS {
            return Err(MakeFail::Const(c, ConstFail::TooManyLengths { item: checker_of(c), most: MOST_CONSTS }));
        }
        match eval_len(r, c) {
            Ok(()) => {}
            // No evaluator here and none running (`Maps::evaluating` is zero): this caller
            // evaluates no constant at all (a harness, a caller made with `Records::consts`
            // `None`). An ANONYMOUS constant a head wrote (`[u8; size_of::<Self>()]` on a
            // method of `u8`, met by any probe of `u8`'s impls) is then that head's refusal
            // by name and only that head's, filed as the evaluator's own refusal of one is
            // (`Maps::anon_failed`): the fill goes on, and the body that asked reports a gap
            // only if it uses that head. While an evaluation runs the `Nested` goes up
            // unfiled: its work list evaluates the constant first (consts.rs `value_at`).
            Err(ConstFail::Nested { .. }) if r.consts.is_none() && r.store.maps.evaluating.get() == 0 && super::anon::need_key(checker_of(c)) == c => {
                r.store.maps.anon_failed.insert(c, Box::from("this caller has no constant evaluator (`Records::consts` is `None`)"));
            }
            Err(e) => return Err(MakeFail::Const(c, e)),
        }
    }
}

/// Evaluates the `usize` constant `c` an array length names and files its value in
/// `Maps::const_lens`. The evaluator is taken out of `r` meanwhile, so a length met inside
/// that evaluation is [`ConstFail::Nested`]: never a nested evaluation.
pub fn eval_len<'n, S: NamesSource<'n>>(r: &mut Records<'_, 'n, S>,c: LibKey) -> Result<(), ConstFail> {
    let Some(start) = r.store.maps.const_proofs.as_ref().map(|p| p.filled.len()) else {
        return eval_len_inner(r, c);
    };
    if r.consts.is_none() { return eval_len_inner(r, c) }
    let capture = r.store.maps.capture_const(r.store.tys.alloc());
    let result = eval_len_inner(r, c);
    let inputs = r.store.maps.finish_capture(capture).expect("the pooled world's source recorder");
    r.store.maps.const_proofs.as_mut().expect("the pooled world's source proofs").finish(start, c, inputs);
    result
}

fn eval_len_inner<'n, S: NamesSource<'n>>(r: &mut Records<'_, 'n, S>, c: LibKey) -> Result<(), ConstFail> {
    // A qualified constant leaf of a head's expression (`<B4 as Spec>::BITS`:
    // front2/const_qpath.rs) is answered by selection, with every leaf asked beside it; its
    // refusal is filed for the head that wrote it, as an anonymous constant's is below.
    if r.consts.is_some() && super::const_qpath::eval_asked(r, c) {
        return Ok(());
    }
    // An inherent constant leaf (`Self::MAX_SIZE`: front2/const_inherent.rs) is answered by
    // the impl member constant's evaluation, every asked leaf in the same round, its refusal
    // filed for the head that wrote it.
    if r.consts.is_some() && super::const_inherent::eval_asked(r, c) {
        return Ok(());
    }
    let Some((consts, prepared)) = r.consts.take() else {
        return Err(ConstFail::Nested { item: checker_of(c), shown: lib_shown(r, c) });
    };
    // An anonymous constant a head asked for (`1 << 0`, `u8::MAX`: front2/anon.rs) is made a
    // constant item's head first; `c` may be one's need key, whose checker key is that head's.
    let filed = super::anon::file_asked(&mut *r.store, &mut *r.keys).map_err(|e| ConstFail::Eval(format!("constant {:?}: {e}", checker_of(c)).into_boxed_str()));
    let (value, asked) = match filed {
        Ok(asked) => (consts.value(r, prepared, checker_of(c)).map(int_of), asked),
        Err(e) => (Err(e), Vec::new_in(Global)),
    };
    // The other anonymous constants asked with it (an enum's other discriminants) are
    // evaluated in the same round.
    // An ANONYMOUS constant the evaluator refuses is not this caller's failure: its refusal
    // is filed (`Maps::anon_failed`) and the head that wrote the expression is refused by
    // name when it is made again (`Builder::const_value_bits`), and only that head: an impl
    // whose header writes `W<{ size_of::<u8>() }>` stays a refused candidate, a named gap of
    // the questions it could answer, as before there was an evaluator behind the heads.
    let anonymous = |k: LibKey| super::anon::need_key(checker_of(k)) == k;
    let mut more: Vec<(LibKey, u128), Global> = Vec::new_in(Global);
    let mut failed: Vec<(LibKey, Box<str, Global>), Global> = Vec::new_in(Global);
    for &o in asked.iter() {
        if o == c || r.store.maps.const_bits.contains_key(&o) || r.store.maps.anon_failed.contains_key(&o) {
            continue;
        }
        match consts.value(r, prepared, checker_of(o)).map(int_of) {
            Ok(Some(bits)) => more.push((o, bits)),
            Ok(None) => failed.push((o, Box::from("its value is not an integer of at most 16 bytes"))),
            Err(e) => failed.push((o, format!("{e}").into_boxed_str())),
        }
    }
    r.consts = Some((consts, prepared));
    for &(o, bits) in more.iter() {
        r.store.maps.file_bits(o, bits);
    }
    for (o, why) in failed {
        r.store.maps.noted_const_result(o);
        r.store.maps.anon_failed.insert(o, why);
    }
    let value = match value {
        Err(e) if anonymous(c) => {
            r.store.maps.anon_failed.insert(c, format!("{e}").into_boxed_str());
            return Ok(());
        }
        other => other,
    };
    // The value's bits in its own type's width (at most 16 bytes): every reader knows the
    // type. `const_lens` keeps those of 64 bits for `Heads::const_len`.
    let bits = value?.ok_or(ConstFail::NotUsize { item: checker_of(c) })?;
    r.store.maps.file_bits(c, bits);
    if let Ok(v) = u64::try_from(bits) {
        r.store.maps.const_lens.insert(c, v);
    }
    Ok(())
}

/// Makes every fill of a round, in the given order; a refusal named is that of the least fill
/// (by its `Debug` text: `Shape` has no order), so the error does not depend on the order.
pub(super) fn make_all<'n, S: NamesSource<'n>>(r: &mut Records<'_, 'n, S>,fills: Vec<Fill, Global>, owner: ItemKey) -> Result<(), CheckFail> {
    let mut refused: Option<(String, MakeFail)> = None;
    for fill in fills {
        if let Err(e) = make(r, fill) {
            let shown = format!("{fill:?}");
            if refused.as_ref().is_none_or(|(at, _)| shown < *at) {
                refused = Some((shown, e));
            }
        }
    }
    match refused {
        Some((_, e)) => Err(e.at(r, owner)),
        None => Ok(()),
    }
}

/// Makes the answer to `ImplsOf(trait_, shape)` from the one impl index (the same buckets the
/// checker's questions fill) and returns the impls it holds; `None` when a candidate's header
/// refused: the question then stays a gap, so the back half's asking it refuses by name.
pub fn answer_impls<'n, S: NamesSource<'n>>(r: &mut Records<'_, 'n, S>,trait_: ItemKey, shape: tc::Shape) -> Option<Vec<ItemKey, Global>> {
    // Selection reads the trait's head too (`TraitDef`): made with its answer.
    if !r.store.trait_defs.contains_key(&trait_) {
        let want = super::fill::want_of_key(r.names, r.keys, trait_).ok()?;
        make(r, Fill::Store(want)).ok()?;
    }
    if r.buckets.as_ref().and_then(|b| b.impls_of(trait_, shape)).is_none() {
        // At ground arguments after checking: the whole program's impls (`Seen::Program`).
        make(r, Fill::Impls(trait_, shape, Seen::Program)).ok()?;
    }
    let b = r.buckets.as_ref()?;
    if b.trait_gap(trait_, shape).is_some() {
        return None;
    }
    let mut out = Vec::new_in(Global);
    out.extend_from_slice(b.impls_of(trait_, shape)?);
    Some(out)
}

fn make_once<'n, S: NamesSource<'n>>(r: &mut Records<'_, 'n, S>,fill: Fill) -> Result<(), MakeFail> {
    match fill {
        Fill::Store(want) => r.store.ensure(r.names, r.keys, want).map_err(MakeFail::Head),
        Fill::Impls(trait_, shape, seen) => {
            let (names, index, order) = (r.names, r.index, r.order);
            let b = &*r.buckets.get_or_insert_with(|| Buckets::for_source(names, index));
            let mut heads = Lowering { names, keys: &mut *r.keys, store: &mut *r.store };
            match trait_question(names, &mut heads, b, trait_, shape, seen, order)? {
                Made::Made => Ok(()),
                Made::Later(never) => match never {},
            }
        }
        Fill::Inherent(shape, name) => {
            let (names, index, order) = (r.names, r.index, r.order);
            let b = &*r.buckets.get_or_insert_with(|| Buckets::for_source(names, index));
            let mut heads = Lowering { names, keys: &mut *r.keys, store: &mut *r.store };
            match inherent_question(names, &mut heads, b, shape, name, order)? {
                Made::Made => Ok(()),
                Made::Later(never) => match never {},
            }
        }
        Fill::InferredOutput(_) => Ok(()),
    }
}

enum InferredErr {
    Gap(Gap),
    Fail(CheckFail),
}

/// Resolves one or more `_`-returning callees (`targets`) in demand order using an explicit
/// iterative worklist (zero recursion) and detects mutual recursion cycles cleanly.
fn resolve_inferred_outputs<'n, S: NamesSource<'n>>(
    r: &mut Records<'_, 'n, S>,
    caller: ItemKey,
    targets: &[ItemKey],
) -> Result<(), InferredErr> {
    const MOST_INFER_OUTPUTS: u32 = 256;
    let alloc = r.store.tys.alloc();
    let base_caller = r.store.has_unresolved_inferred_output(caller).then_some(caller);
    let mut stack: blaze_own::Vec<ItemKey, blaze_own::Alloc<'static>> = blaze_own::Vec::new_in(alloc);
    for &t in targets.iter().rev() {
        if stack.scan_for(&t).is_none() {
            stack.push(t);
        }
    }
    let result = (|| -> Result<(), InferredErr> {
        for _ in 0..MOST_INFER_OUTPUTS {
            let Some(&cur) = stack.last() else { return Ok(()) };
            if !r.store.has_unresolved_inferred_output(cur) {
                stack.pop();
                continue;
            }
            r.store.inferring_output_stack.clear();
            if let Some(bc) = base_caller {
                r.store.inferring_output_stack.push(bc);
            }
            r.store.inferring_output_stack.extend_from_slice(&stack);
            r.store.inferred_output_wanted = None;
            r.store.inferred_output_cycle = None;
            let Some(prepared) = r.consts.as_ref().map(|c| c.1) else {
                return Err(InferredErr::Gap(Gap::BodyRecord(tc::Question::FnSig(cur))));
            };
            let target = match r.keys.library_in(r.names, cur) {
                Some(lib) => super::lower_made::LowerTarget::Free(lib),
                None => super::lower_made::LowerTarget::Member(cur),
            };
            let mut lowered = match super::lower_made::lower_made(r, prepared, target) {
                Ok(l) => l,
                Err(_) => {
                    return Err(InferredErr::Gap(Gap::InferredOutputFailed { item: cur, why: "body could not be lowered" }));
                }
            };
            if !lowered.typeck.inline_fors.is_empty()
                && super::inline_for::expand_concrete_body(r, prepared, &mut lowered).is_err()
            {
                return Err(InferredErr::Gap(Gap::InferredOutputFailed { item: cur, why: "inline for expansion failed" }));
            }
            let view = lowered.lowered.view();
            let spell = match Spelling::of(&view) {
                Ok(s) => s,
                Err(_) => {
                    return Err(InferredErr::Gap(Gap::InferredOutputFailed { item: cur, why: "body names could not be spelled" }));
                }
            };
            let scope = lowered.scope(&spell);
            match check_rounds_as(r, &lowered, &scope, None, None, super::stalled_goals::Stalls::Gap, None) {
                Ok(_done) => {
                    if r.store.has_unresolved_inferred_output(cur) {
                        return Err(InferredErr::Gap(Gap::InferredOutputFailed { item: cur, why: "body check did not finalize return type" }));
                    }
                    stack.pop();
                }
                Err(fail) => {
                    if let Some((a, b)) = r.store.inferred_output_cycle.take() {
                        return Err(InferredErr::Gap(Gap::InferredOutputCycle { owner: a, callee: b }));
                    }
                    if let Some(next) = r.store.inferred_output_wanted.take() {
                        if base_caller == Some(next) || stack.scan_for(&next).is_some() {
                            return Err(InferredErr::Gap(Gap::InferredOutputCycle { owner: cur, callee: next }));
                        }
                        stack.push(next);
                        continue;
                    }
                    return Err(InferredErr::Fail(fail));
                }
            }
        }
        let cur = stack.last().copied().unwrap_or(caller);
        Err(InferredErr::Gap(Gap::InferredOutputCycle { owner: caller, callee: cur }))
    })();
    r.store.inferring_output_stack.clear();
    r.store.inferred_output_wanted = None;
    r.store.inferred_output_cycle = None;
    result
}

/// What making a head, or answering an impl question, came to: made, or left to a later
/// maker for the reason `L` (an [`ImplHeads`] that is never left is `L = Infallible`).
pub(super) enum Made<L> {
    Made,
    Later(L),
}

/// A head a seeded store could not file through shared borrows ([`Importing`]): its seed
/// asked for it to be lowered (`Seeded::Lower`: the cell being made up the seeding stack), or
/// failed. The question is left to its fill, which `make_once` makes between rounds over
/// `&mut`, meeting the same seed there and its refusal by name (`HeadError::Seed`, the task's
/// `TaskError::Seed`), never a different answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct NotHere;

/// How an impl question's heads are made (rule 78: the choice is the one seam `Store::seeds`,
/// as `BodyWorld::store_want`'s is; both instantiations run in the cost test, answers and read
/// lists asserted equal):
/// - [`Lowering`]: between rounds, through `&mut` (`Store::ensure`), every store;
/// - [`Importing`]: on the spot, mid-check, through shared borrows from the frozen records of
///   a seeded store (`HeadSeed::seed`), as `store_want` imports a head.
pub(super) trait ImplHeads<'n> {
    /// Why a head can be left to a later maker.
    type Later;
    fn store(&self) -> &Store<'n>;
    fn keys(&self) -> &Keys;
    /// `want` made; `Later` when it cannot be made here; `Err` refused.
    fn make(&mut self, want: Want) -> Result<Made<Self::Later>, HeadError>;
}

/// Heads lowered (or seeded) between rounds, through `&mut` ([`ImplHeads`]).
pub(super) struct Lowering<'a, 'n, S> {
    pub names: S,
    pub keys: &'a mut Keys,
    pub store: &'a mut Store<'n>,
}

impl<'n, S: NamesSource<'n>> ImplHeads<'n> for Lowering<'_, 'n, S> {
    type Later = core::convert::Infallible;

    fn store(&self) -> &Store<'n> {
        &*self.store
    }

    fn keys(&self) -> &Keys {
        &*self.keys
    }

    fn make(&mut self, want: Want) -> Result<Made<core::convert::Infallible>, HeadError> {
        self.store.ensure(self.names, self.keys, want).map(|()| Made::Made)
    }
}

/// Heads imported on the spot from a seeded store's frozen records, through shared borrows
/// ([`ImplHeads`]; world_items.rs `BodyWorld::impl_question`). Each head filed counts in
/// `Store::imported`, as `store_want`'s does.
pub(super) struct Importing<'a, 'n> {
    pub keys: &'a Keys,
    pub store: &'a Store<'n>,
    pub seeds: &'n dyn super::tables::seeds::HeadSeed<'n>,
}

impl<'n> ImplHeads<'n> for Importing<'_, 'n> {
    type Later = NotHere;

    fn store(&self) -> &Store<'n> {
        self.store
    }

    fn keys(&self) -> &Keys {
        self.keys
    }

    fn make(&mut self, want: Want) -> Result<Made<NotHere>, HeadError> {
        // As `Store::ensure` over a seeded store: a made want is made (its record filed it whole).
        if self.store.made(want) {
            return Ok(Made::Made);
        }
        // The one ask (front2 asks.rs, D161 step 3): the head's need claimed once on the store's
        // board; a refusal is kept on the store, where `BodyWorld::impl_question` makes it the
        // question's gap.
        if self.store.asks.once() {
            use super::asks::Imported;
            return Ok(match super::asks::ask_seed(self.store, self.keys, self.seeds, want) {
                Ok(Imported::Made | Imported::Read) if self.store.made(want) => Made::Made,
                Ok(_) | Err(_) => Made::Later(NotHere),
            });
        }
        let _cost = super::tables::cost::Cost::enter_want(self.store.cost, want);
        let filed = self.seeds.seed(self.store, self.keys, want, self.store.tys.alloc());
        if matches!(filed, Ok(super::heads::Seeded::Filed)) && self.store.made(want) {
            self.store.imported.set(self.store.imported.get() + 1);
            return Ok(Made::Made);
        }
        // A seed that files nothing, asks to lower, or refuses: `ensure` meets it between
        // rounds and keeps its refusal (`Store::seed_error`), which a shared borrow cannot.
        Ok(Made::Later(NotHere))
    }
}

/// Answers `ImplsOf(trait_, shape)` into `b`, its heads made by `heads`: the ONE body of this
/// question for both paths (`make_once` between rounds, `BodyWorld::impl_question` on the
/// spot). `Later` leaves `b` with the question unanswered (what was filed and imported stays;
/// filing twice is a no-op), so the fill made after it answers as one call would.
/// `seen` says whose impls may answer ([`Seen`]): a checked body's crate's link closure, or
/// the whole program's; the answer is filed under it.
pub(super) fn trait_question<'n, S: NamesSource<'n>, H: ImplHeads<'n>>(names: S, heads: &mut H, b: &Buckets, trait_: ItemKey, shape: tc::Shape, seen: Seen, order: BuildOrder) -> Result<Made<H::Later>, MakeFail> {
    let ask = Ask::Trait(trait_, shape);
    if let Made::Later(why) = prepare_coherence(names, heads, ask).map_err(MakeFail::Head)? {
        return Ok(Made::Later(why));
    }
    let _cost = super::tables::cost::Cost::enter(heads.store().cost, super::tables::cost::Part::Impls);
    // Only impls written where the orphan rules allow (`trait_home`) are candidates.
    let home = trait_home(names, heads.keys(), heads.store(), trait_, shape).map_err(MakeFail::Head)?;
    // Over a bucket index: this question's buckets of the crates `home` admits.
    b.admit_shared(names, heads.keys(), home, ask, heads.store().tys.alloc()).map_err(MakeFail::from)?;
    // And only impls of the crates the asking body sees (`Seen`): none of a crate downstream
    // of a checked body's, whose headers are then never made for it either.
    let sight = b.sight(names, seen, heads.store().tys.alloc()).map_err(|why| MakeFail::Buckets(Unfilled::Sight(why)))?;
    let mut candidates = b.trait_candidates(trait_, shape, home);
    // A negative impl is a candidate of its trait's questions (`Buckets::answer`): it
    // decides an auto trait's structural rule, and the solver matches it to no positive
    // goal; its refused header is the question's gap, never a wrong `yes`.
    candidates.extend_from_slice(&b.negative_of(trait_, shape, home));
    candidates.retain(|k| sight.sees(b.krate(*k)));
    candidates.sort_unstable();
    candidates.dedup();
    if let Made::Later(why) = lower_heads(names, heads, b, &candidates, order)? {
        return Ok(Made::Later(why));
    }
    b.answer_candidates(heads.store(), trait_, shape, home, sight, &candidates).map_err(MakeFail::Buckets)?;
    Ok(Made::Made)
}

/// Answers `Inherent(shape, name)` into `b`, as [`trait_question`] answers a trait's.
pub(super) fn inherent_question<'n, S: NamesSource<'n>, H: ImplHeads<'n>>(names: S, heads: &mut H, b: &Buckets, shape: tc::Shape, name: Name, order: BuildOrder) -> Result<Made<H::Later>, MakeFail> {
    let ask = Ask::Inherent(shape);
    if let Made::Later(why) = prepare_coherence(names, heads, ask).map_err(MakeFail::Head)? {
        return Ok(Made::Later(why));
    }
    let _cost = super::tables::cost::Cost::enter(heads.store().cost, super::tables::cost::Part::Impls);
    // Only impls written in the type's own crate (`inherent_home`) are candidates.
    let home = inherent_home(names, heads.keys(), heads.store(), shape).map_err(MakeFail::Head)?;
    b.admit_shared(names, heads.keys(), home, ask, heads.store().tys.alloc()).map_err(MakeFail::from)?;
    // The name filters first, from the impls' member rows (no header made): an impl
    // that declares no member of the name is never the answer, so its header is not
    // lowered for it. rustc finds an inherent item by name before it unifies self
    // types; without this filter `<U1792>::LIMBS` in the header of crypto-bigint's
    // `impl Uint<{ <U1792>::LIMBS / 2 }> { fn concat .. }` made that very header
    // (census 7: "an array length names: Nested", 169 fns).
    let mut candidates = b.inherent_candidates(shape, home);
    candidates.retain(|&k| super::member_names::may_declare(names, k, name));
    if let Made::Later(why) = lower_heads(names, heads, b, &candidates, order)? {
        return Ok(Made::Later(why));
    }
    b.answer_inherent(heads.store(), shape, name, &candidates).map_err(MakeFail::Buckets)?;
    Ok(Made::Made)
}

/// Makes the headers of `candidates` the store does not hold yet, counting them. `Later` at
/// the first `heads` leaves (the candidates before it are made and stay; it and those after
/// are counted by the maker that makes them).
fn lower_heads<'n, S: NamesSource<'n>, H: ImplHeads<'n>>(names: S, heads: &mut H, b: &Buckets, candidates: &[LibKey], order: BuildOrder) -> Result<Made<H::Later>, MakeFail> {
    let mut refused = None;
    let mut iter = candidates.iter();
    while let Some(&k) = if order == BuildOrder::Reversed { iter.next_back() } else { iter.next() } {
        let ck = checker_of(k);
        if heads.store().impls.contains_key(&ck)
            || (heads.store().maps.evaluating.get() > 0 && heads.store().provisional_impls.contains_key(&ck))
            || b.has_failed(k)
        {
            continue;
        }
        // A refused derive's gap (D193 point 5: buckets.rs `file_rows`): a candidate whose
        // header no text makes, so the question's answer is unknown, by the refusal's own
        // words, never "no impl". Only an unread candidate can be one (its row's trait is
        // unread), and the names say which calls were refused.
        if b.is_unread(k)
            && let Ok(frame) = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer)
        {
            let erased = blaze_own::Erased::new(&frame);
            if let Some(row) = names.failed_call(k, erased.bump()) {
                b.fail(k, refused_derive_words(&row));
                continue;
            }
        }
        let made = heads.make(Want::Impl(k));
        if !matches!(made, Ok(Made::Later(_))) {
            b.count_lowered();
        }
        match made {
            Ok(Made::Made) => {}
            Ok(Made::Later(why)) => return Ok(Made::Later(why)),
            // A constant the header needs is evaluated by `make`, then the header is retried.
            Err(e @ HeadError::Item(_, Why::NeedConst(..))) => {
                if refused.as_ref().is_none_or(|(at, _)| k < *at) {
                    refused = Some((k, e));
                }
            }
            // A refused candidate is a named gap of the questions it is a candidate of
            // (`Gap::RefusedImpl`, `Gap::UnreadImpl`), never left out of their answers and
            // never the end of a body that asks none of them.
            Err(e) => b.fail(k, super::header_written::head_failed(names, k, &e)),
        }
    }
    match refused {
        Some((_, e)) => Err(MakeFail::Head(e)),
        None => Ok(Made::Made),
    }
}

/// The words of a refused derive's gap ([`lower_heads`]): which derive, on which item, in which
/// file, and why the names record says it was refused (rule 19). Never its key: two units of
/// one package refuse one derive under two keys and say one cause in equal words.
fn refused_derive_words(row: &blaze_scope::names_record::FailedRow<'_>) -> Box<str> {
    let on = if row.on.is_empty() { "an item with no name" } else { row.on };
    format!(
        "unknown: the derive of `{}` on `{on}` in {} was refused, so no impl it would have written is known: {}",
        row.path, row.file, row.why
    )
    .into_boxed_str()
}
