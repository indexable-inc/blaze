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

use super::buckets::{Buckets, Unfilled, inherent_home, trait_home};
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
    /// The body is malformed or a bound of the checker passed.
    Check { owner: ItemKey, error: tc::CheckError },
    /// The check finished with diagnostics: the program's errors in the body, each by its
    /// code and its range in the item's text. Never counted as checked.
    /// Each with what it is about, printed from the record's types: an unmet bound (E0277) as
    /// `Self: Trait<First>` from `Because::Bound`, else its `because`, `found` and `expected`.
    /// `filed`, one per diagnostic: where it is filed (front2 expansion_origin.rs): in expanded
    /// text, at the user's tokens or at the call; elsewhere [`Filed::Own`](super::expansion_origin::Filed::Own).
    Diags { owner: ItemKey, diags: Vec<(tc::DiagCode, tc::Range, Box<str>), Global>, filed: Vec<super::expansion_origin::Filed, Global> },
    /// Still `Need` after [`MOST_ROUNDS`].
    Rounds { owner: ItemKey, last: Vec<Question, Global> },
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
}

impl core::fmt::Display for CheckFail {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            CheckFail::Gap { owner, gaps } => {
                write!(f, "checking {owner:?}: {} gaps", gaps.len())?;
                for gap in gaps {
                    write!(f, "\n  {gap}")?;
                }
                Ok(())
            }
            CheckFail::NothingToFill { owner, questions } => write!(f, "checking {owner:?}: the checker needs {questions:?} and the world noted nothing to make"),
            CheckFail::Head { owner, error } => write!(f, "checking {owner:?}: {error}"),
            CheckFail::Buckets { owner, error } => write!(f, "checking {owner:?}: {error}"),
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
            CheckFail::Rounds { owner, last } => write!(f, "checking {owner:?}: still needs {last:?} after {MOST_ROUNDS} rounds"),
            CheckFail::ReadsDiffer { owner, diff } => write!(f, "checking {owner:?} again read other answers than its first check: two read-list digests under one body (rule 21): {diff}"),
            CheckFail::Resolve { owner, item, error, args } => write!(f, "in {owner:?}, the call of {item:?} at [{args}]: {error}"),
            CheckFail::Const { owner, key, shown, error } => write!(f, "checking {owner:?}: the constant {key:?} ({shown}) an array length names: {error:?}"),
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
) -> Result<tc::Checked<'static>, CheckFail> {
    check_rounds_as(r, body, scope, count, asked, super::stalled_goals::Stalls::Gap)
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
) -> Result<tc::Checked<'static>, CheckFail> {
    let owner = body.typeck.owner;
    // The store's allocator: the checked body outlives the round (`Checked<'static>`).
    let alloc = r.store.tys.alloc();
    let mut hints = Needs::new_in(alloc);
    let mut all_gaps: Vec<Gap, Global> = Vec::new_in(Global);
    // The opaques whose hidden types this check reads, grown only by its own gaps
    // (front2/opaque_auto.rs): its answers never depend on what was checked before it.
    let mut leaks = r.store.hidden.new_leaks();
    for round in 0..MOST_ROUNDS {
        r.store.hidden.set_leaking(&leaks);
        let tys = body.tys.view(&r.store.tys).map_err(|error| CheckFail::Check { owner, error })?;
        let view: BodyView<'_> = body.typeck.view(tys);
        let world = world_at(r, scope);
        let mut cx = InferCtx::new_in(alloc);
        let out = match &mut count {
            Some((tally, clock)) => tc::check_counted(&view, &world, &mut cx, &hints, tally, *clock, alloc),
            // Stalled goals decided after the async bodies' flows (front2 stalled_goals.rs).
            None if stalls == super::stalled_goals::Stalls::Decide => tc::check_deferring(&view, &world, &mut cx, &hints, alloc),
            None => tc::check(&view, &world, &mut cx, &hints, alloc),
        };
        // An auto trait of another fn's opaque: its defining body filed (checked only if no
        // clean check of it is filed yet), the opaque leaked to this check, the round again.
        let out = match out {
            Ok(out) => out,
            Err(error) => {
                drop(world);
                if super::opaque_auto::after_auto_gap(r, owner, &error, &mut leaks)? {
                    continue;
                }
                return Err(CheckFail::Check { owner, error });
            }
        };
        if let Some((tally, _)) = &mut count {
            cx.add_solver_counts(&mut tally.solve);
        }
        let mut fills = world.fills.into_inner();
        if r.order == BuildOrder::Reversed {
            fills.reverse();
        }
        // A failed candidate is a gap only of a question it is a candidate of (the world's
        // `impls_of`/`inherent`), never of every body checked after it failed.
        let mut gaps = world.gaps.into_inner();
        // A const term the body's conversion left a hole (`[0u8; N * 2]`, `f::<{ N + 1 }>()`)
        // was checked as a const variable, which takes any value: a gap of every round, so
        // the finished check is no answer and none of its diagnostics is the program's.
        gaps.extend(body.typeck.const_gaps.iter().map(|&gap| Gap::BodyConst { owner, gap }));
        let round_gaps = !gaps.is_empty();
        for gap in gaps {
            // `Gap` holds checker questions and library errors with no `Eq`: one gap is one
            // `Debug` text, so the list names each once.
            // Kept sorted by that text: the round a gap is met in depends on what earlier rounds
            // made, so the list's order must not.
            let shown = format!("{gap:?}");
            if let Err(at) = all_gaps.binary_search_by(|g| format!("{g:?}").cmp(&shown)) {
                all_gaps.insert(at, gap);
            }
        }
        let needs = match out {
            // A finished check with a diagnostic is not a checked body: each is a finding.
            Outcome::Done(done) if !round_gaps && !done.diags.is_empty() => {
                let mut diags = Vec::with_capacity_in(done.diags.len(), Global);
                let tys = done.types.view();
                diags.extend(done.diags.iter().map(|d| (d.code, d.at, diag_about(r, scope.spell, tys, d))));
                if let Some(asked) = asked.as_deref_mut() {
                    asked.finished = Some(done.reads.clone());
                }
                // Each filed where its tokens came from, before it leaves the task that made it
                // (drive.md, ruled 20:57), in the store's allocator as this loop's other three.
                let lib = r.keys.library_in(r.names, owner);
                let mut filed = Vec::with_capacity_in(diags.len(), Global);
                filed.extend(diags.iter().map(|(_, at, _)| super::expansion_origin::file_range(r.names, lib, *at, alloc)));
                return Err(CheckFail::Diags { owner, diags, filed });
            }
            Outcome::Done(done) if !round_gaps => return Ok(done),
            // A gap answered `Missing`, so a finished check past one is no answer.
            Outcome::Done(done) => {
                if let Some(asked) = asked.as_deref_mut() {
                    asked.finished = Some(done.reads.clone());
                }
                return Err(CheckFail::Gap { owner, gaps: all_gaps });
            }
            Outcome::Need(needs) => needs,
        };
        if let Some(asked) = asked.as_deref_mut() {
            asked.questions.clear();
            asked.questions.extend_from_slice(&needs.questions);
        }
        if fills.is_empty() && !all_gaps.is_empty() {
            return Err(CheckFail::Gap { owner, gaps: all_gaps });
        }
        if fills.is_empty() {
            let mut questions = Vec::new_in(Global);
            questions.extend_from_slice(&needs.questions);
            return Err(CheckFail::NothingToFill { owner, questions });
        }
        let dropped = make_round(r, fills, owner)?;
        if let Some((tally, _)) = &mut count {
            tally.dropped += dropped;
            if round > 0 {
                for &q in needs.missing.iter() {
                    tally.late_holes[tc::question_kind(q)] += 1;
                }
            }
        }
        hints = needs;
    }
    let mut last = Vec::new_in(Global);
    last.extend_from_slice(&hints.questions);
    Err(CheckFail::Rounds { owner, last })
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
/// and its name; else that the store gives no path.
pub(crate) fn key_shown<'n, S: NamesSource<'n>>(r: &Records<'_, 'n, S>, k: ItemKey) -> Box<str, Global> {
    key_shown_in(r.names, &*r.keys, &*r.store, k)
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
        Some(Origin::Member { container, name, .. }) => match keys.library_in(names, *container).and_then(path_of) {
            Some(c) => format!("`{c}::{name}`"),
            None => format!("`{name}` of {}", super::item_shown::ItemShown::of(names, keys, *container)),
        },
        _ => match keys.library_in(names, k).and_then(path_of) {
            Some(p) => format!("`{p}`"),
            None => super::item_shown::ItemShown::of(names, keys, k).to_string(),
        },
    };
    shown.into_boxed_str()
}

/// `t` of `tys` as Rust spells it (`Result<Out, Missing>`), by a work list; a node with no
/// spelling here is its `Debug`.
fn ty_shown<'n, S: NamesSource<'n>>(r: &Records<'_, 'n, S>,tys: tc::PTys<'_>, t: tc::PTy) -> String {
    enum Step {
        Ty(tc::PTy),
        Lit(&'static str),
    }
    let mut out = String::new();
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
            Step::Ty(t) => t,
        };
        let (open, close, list) = match tys.node(t) {
            Ok(tc::PNode::Adt(k, l)) => {
                out.push_str(&item_shown(r, k));
                if l.len == 0 {
                    continue;
                }
                ("<", ">", l)
            }
            Ok(tc::PNode::Tuple(l)) => ("(", ")", l),
            Ok(tc::PNode::Ref(m, i)) => {
                out.push_str(if m == tc::Mutbl::Mut { "&mut " } else { "&" });
                todo.push(Step::Ty(i));
                continue;
            }
            Ok(tc::PNode::Param(i)) => {
                out.push_str(&format!("Param({i})"));
                continue;
            }
            Ok(tc::PNode::Never) => {
                out.push('!');
                continue;
            }
            Ok(n @ (tc::PNode::Bool | tc::PNode::Char | tc::PNode::Str | tc::PNode::Int(_) | tc::PNode::Uint(_) | tc::PNode::Float(_))) => {
                // types.rs `PRIMS` is the one table of built-in names; it covers all 19.
                match super::types::PRIMS.iter().find(|(_, p)| *p == n) {
                    Some(&(name, _)) => out.push_str(name),
                    None => out.push_str(&format!("{n:?} (a built-in type missing from types.rs PRIMS)")),
                }
                continue;
            }
            Ok(tc::PNode::Slice(e)) => {
                out.push('[');
                todo.push(Step::Lit("]"));
                todo.push(Step::Ty(e));
                continue;
            }
            Ok(tc::PNode::Ptr(m, i)) => {
                out.push_str(if m == tc::Mutbl::Mut { "*mut " } else { "*const " });
                todo.push(Step::Ty(i));
                continue;
            }
            Ok(n) => {
                out.push_str(&format!("{n:?}"));
                continue;
            }
            Err(e) => {
                out.push_str(&format!("<{e:?}>"));
                continue;
            }
        };
        let kids = tys.list(list).unwrap_or(&[]);
        out.push_str(open);
        todo.push(Step::Lit(close));
        for (i, &k) in kids.iter().enumerate().rev() {
            todo.push(Step::Ty(k));
            if i > 0 {
                todo.push(Step::Lit(", "));
            }
        }
    }
    out
}

/// What a diagnostic is about, printed: an unmet goal entire, the leaf where-clause first when
/// the checker names one (`Error: From<Other>` because `Result<u8, Error>: FromResidual<..>`).
pub(super) fn diag_about<'n, S: NamesSource<'n>>(r: &Records<'_, 'n, S>, spell: &Spelling<'_>, tys: tc::PTys<'_>, d: &tc::Diag) -> Box<str> {
    let ty = |t: Option<tc::PTy>| t.map(|t| ty_shown(r, tys, t));
    let s = match d.because {
        Some(tc::Because::Unmet { goal, leaf }) => {
            let shown = |g: tc::UnmetGoal| {
                let (tc::UnmetGoal::Trait { trait_, args } | tc::UnmetGoal::Projection { trait_, args, .. }) = g;
                let all: Vec<String> = tys.list(args).map(|l| l.iter().map(|&t| ty_shown(r, tys, t)).collect()).unwrap_or_default();
                let (self_, rest) = all.split_first().map_or(("?", &[][..]), |(s, rest)| (s.as_str(), rest));
                let tr = if rest.is_empty() { item_shown(r, trait_).to_string() } else { format!("{}<{}>", item_shown(r, trait_), rest.join(", ")) };
                match g {
                    tc::UnmetGoal::Trait { .. } => format!("`{self_}: {tr}`"),
                    tc::UnmetGoal::Projection { trait_, name, term, .. } => {
                        // Rule 19: the associated type by its name, from the trait's members
                        // as filed; never its hash alone.
                        let assoc = match r.store.trait_defs.get(&trait_).and_then(|t| t.assoc.iter().find(|a| a.name == name)) {
                            Some(a) => item_shown(r, a.item),
                            None => format!("{name:?} (an associated type of a trait whose members are not filed, so its name is not known here)"),
                        };
                        format!("`<{self_} as {tr}>::{assoc} == {}`", ty_shown(r, tys, term))
                    }
                }
            };
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
        Some(tc::Because::Path(miss)) => super::path_miss_shown::path_miss_shown(r, spell, d.name, miss),
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
                tc::Because::Unmet { .. } | tc::Because::NoLangItem(_) | tc::Because::Path(_) => format!("{other:?}"),
            };
            node_about(Some(why), ty(d.found), ty(d.expected), d.name.map(|n| name_about(r, spell, tys, d.found, n)))
        }
        None => node_about(None, ty(d.found), ty(d.expected), d.name.map(|n| name_about(r, spell, tys, d.found, n))),
    };
    s.into_boxed_str()
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
    let text = r.names.text_of(lib)?;
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
    let mut put = |lit: Lit<'_>, out: &mut String| match write_value(lit, 0, &fill, out) {
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
    let alloc = r.store.tys.alloc();
    for _ in 0..MOST_ROUNDS {
        // A callee resolved at ground types after its body has checked is post-analysis: it
        // sees every opaque's hidden type, not only the body's leak list.
        let world = BodyWorld { reveal: super::opaque_auto::Reveal::All, ..world_at(r, scope) };
        let out = tc::resolve_instance(&world, item, args, ground, alloc);
        let mut fills = world.fills.into_inner();
        if r.order == BuildOrder::Reversed {
            fills.reverse();
        }
        let gaps = world.gaps.into_inner();
        match out {
            Ok(_) if !gaps.is_empty() => return Err(CheckFail::Gap { owner, gaps }),
            Ok(res) => return Ok(Target::Body(res.item)),
            Err(tc::ResolveError::Missing(_)) if !fills.is_empty() => make_all(r, fills, owner)?,
            Err(_) if !gaps.is_empty() => return Err(CheckFail::Gap { owner, gaps }),
            Err(error) => return Err(CheckFail::Resolve { owner, item, error, args: shown_args(ground.view(), args) }),
        }
    }
    Err(CheckFail::Rounds { owner, last: Vec::new_in(Global) })
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
            | tc::PNode::ExAuto(_) => None,
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
            tc::PNode::Tuple(l) | tc::PNode::Adt(_, l) | tc::PNode::FnDef(_, l) | tc::PNode::FnPtr(l, _) | tc::PNode::Dynamic(l) | tc::PNode::ExTrait(_, l) => Some(l),
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
enum MakeFail {
    Head(HeadError),
    Buckets(Unfilled),
    Const(LibKey, ConstFail),
}

impl MakeFail {
    fn at<'n, S: NamesSource<'n>>(self, r: &Records<'_, 'n, S>, owner: ItemKey) -> CheckFail {
        match self {
            MakeFail::Head(error) => CheckFail::Head { owner, error },
            MakeFail::Buckets(error) => CheckFail::Buckets { owner, error },
            MakeFail::Const(key, error) => CheckFail::Const { owner, key, shown: key_shown(r, checker_of(key)), error: Box::new_in(error, Global) },
        }
    }
}

/// The most constants one fill may need evaluated before it is made (rule 18).
const MOST_CONSTS: u32 = 64;

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
        return Err(ConstFail::Nested { item: checker_of(c), shown: key_shown(r, checker_of(c)) });
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
        r.store.maps.const_bits.insert(o, bits);
    }
    for (o, why) in failed {
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
    r.store.maps.const_bits.insert(c, bits);
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
        make(r, Fill::Impls(trait_, shape)).ok()?;
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
        Fill::Impls(trait_, shape) => {
            // `trait_home` reads the trait's head when one is made (whether each parameter
            // defaults to `Self`) and the text's count when none is. A head made only after a
            // question's first answer gave a later answer of the same question another home,
            // other candidates and another gap, so two checks of one body read two answers
            // under one key (rule 21; census 7: k256's `<impl ProjectivePoint>::GENERATOR`).
            // So the head is made first, and the home is the head's whenever it can be made. A
            // head its text refuses (`TransmuteFrom`'s const parameter) is refused at every
            // ask, so its question's home is the text's at every ask; one that waits on a
            // constant goes up as any head's wait does (`make` evaluates it and asks again).
            if let Some(lib) = r.keys.library_in(r.names, trait_) {
                match r.store.ensure(r.names, r.keys, Want::Head(lib)) {
                    Ok(()) => {}
                    Err(wait @ HeadError::Item(_, Why::NeedConst(..))) => return Err(MakeFail::Head(wait)),
                    Err(_refused_by_its_text) => {}
                }
            }
            // Only impls written where the orphan rules allow (`trait_home`) are candidates.
            let home = trait_home(r.names, r.keys, r.store, trait_, shape);
            let b = buckets_for(r.buckets, r.names, r.index, home);
            // Over a bucket index: this question's buckets of the crates `home` admits.
            b.admit_asked(r.names, r.keys, home, super::buckets::Ask::Trait(trait_, shape));
            let mut candidates = b.trait_candidates(trait_, shape, home);
            // A negative impl is a candidate of its trait's questions (`Buckets::answer`): it
            // decides an auto trait's structural rule, and the solver matches it to no positive
            // goal; its refused header is the question's gap, never a wrong `yes`.
            candidates.extend_from_slice(b.negative_of(trait_));
            candidates.sort_unstable();
            candidates.dedup();
            lower_heads(r.names, r.keys, r.store, b, candidates, r.order)?;
            b.answer(r.store, trait_, shape, home).map_err(MakeFail::Buckets)
        }
        Fill::Inherent(shape, name) => {
            // Only impls written in the type's own crate (`inherent_home`) are candidates.
            let home = inherent_home(r.names, r.keys, shape);
            let names = r.names;
            let b = buckets_for(r.buckets, names, r.index, home);
            b.admit_asked(names, r.keys, home, super::buckets::Ask::Inherent(shape));
            // The name filters first, from the impls' member rows (no header made): an impl
            // that declares no member of the name is never the answer, so its header is not
            // lowered for it. rustc finds an inherent item by name before it unifies self
            // types; without this filter `<U1792>::LIMBS` in the header of crypto-bigint's
            // `impl Uint<{ <U1792>::LIMBS / 2 }> { fn concat .. }` made that very header
            // (census 7: "an array length names: Nested", 169 fns).
            let mut candidates = b.inherent_candidates(shape, home);
            candidates.retain(|&k| super::member_names::may_declare(names, k, name));
            lower_heads(names, r.keys, r.store, b, candidates.clone(), r.order)?;
            b.answer_inherent(r.store, shape, name, &candidates).map_err(MakeFail::Buckets)
        }
    }
}

/// The impl index a question at `home` reads. Over the solved names every impl is filed when it
/// is first made (`Buckets::new`); over records none is (`Buckets::lazy`), and the crates
/// `home` admits are filed here, once each, before the question (`Buckets::admit`: nothing over
/// the solved names).
fn buckets_for<'b, 'n, S: NamesSource<'n>>(buckets: &'b mut Option<Buckets>, names: S, index: &Index<'n>, home: super::buckets::Homes) -> &'b mut Buckets {
    let b = buckets.get_or_insert_with(|| match names.solved() {
        Some(solved) => Buckets::new(solved, index),
        None => Buckets::lazy(names),
    });
    b.admit(names, home);
    b
}

/// Lowers the headers of `candidates` the store does not hold yet, counting them.
fn lower_heads<'n, S: NamesSource<'n>>(names: S, keys: &mut Keys, store: &mut Store<'n>, b: &mut Buckets, mut candidates: Vec<LibKey, Global>, order: BuildOrder) -> Result<(), MakeFail> {
    if order == BuildOrder::Reversed {
        candidates.reverse();
    }
    let mut refused = None;
    for k in candidates {
        if !store.impls.contains_key(&checker_of(k)) && !b.has_failed(k) {
            b.lowered += 1;
            match store.ensure(names, keys, Want::Impl(k)) {
                Ok(()) => {}
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
    }
    match refused {
        Some((_, e)) => Err(MakeFail::Head(e)),
        None => Ok(()),
    }
}
