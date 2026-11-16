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
use blaze_own::Alloc;
use blaze_scope::resolve::Names;
use blaze_typeck as tc;
use blaze_typeck::{BodyView, InferCtx, ItemKey, Name, Needs, Outcome, Question};

use super::buckets::{Buckets, Unfilled, inherent_home, trait_home};
use super::heads::{HeadError, Store, Want};
use super::index::Index;
use super::keys::{Keys, Origin, checker_of};
use super::spell::Spelling;
use super::consts::{ConstFail, Consts, int_of};
use super::types::{SelfTy, Why};
use blaze_lower::scoped::Prepares;
use super::world_items::{BodyWorld, Fill, Gap};

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
    Diags { owner: ItemKey, diags: Vec<(tc::DiagCode, tc::Range, Box<str>), Global> },
    /// Still `Need` after [`MOST_ROUNDS`].
    Rounds { owner: ItemKey, last: Vec<Question, Global> },
    /// The constant `key` an array length in a head names has no value: the evaluator's
    /// refusal, or [`ConstFail::Nested`] for one met while another constant is evaluated.
    Const { owner: ItemKey, key: LibKey, error: Box<ConstFail, Global> },
    /// Instance resolution of the call of `item` in `owner` failed (no impl, overlap, not
    /// ground, or a question the world could not make a record for).
    Resolve { owner: ItemKey, item: ItemKey, error: tc::ResolveError, args: Box<str> },
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
            CheckFail::Diags { owner, diags } => {
                write!(f, "checking {owner:?}: {} diagnostics", diags.len())?;
                for (code, at, about) in diags {
                    write!(f, "\n  {} at {}..{}: {about}", code.code(), at.start, at.end)?;
                }
                Ok(())
            }
            CheckFail::Rounds { owner, last } => write!(f, "checking {owner:?}: still needs {last:?} after {MOST_ROUNDS} rounds"),
            CheckFail::Resolve { owner, item, error, args } => write!(f, "in {owner:?}, the call of {item:?} at [{args}]: {error}"),
            CheckFail::Const { owner, key, error } => write!(f, "checking {owner:?}: the constant {key:?} an array length names: {error:?}"),
        }
    }
}

/// The program's records the loop reads and fills.
pub struct Records<'r, 'n> {
    pub names: &'n Names<'n>,
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
pub fn check_body(r: &mut Records<'_, '_>, body: &Body<'_>, scope: &BodyScope<'_>) -> Result<tc::Checked<'static>, CheckFail> {
    check_body_counted(r, body, scope, None)
}

/// `check_body` that adds each round's questions and costs into a tally (blaze-typeck asks.rs
/// `Tally`), reading the caller's clock: a measurement, never an input to the outcome.
pub fn check_body_counted(
    r: &mut Records<'_, '_>,
    body: &Body<'_>,
    scope: &BodyScope<'_>,
    mut count: Option<(&mut tc::Tally, &dyn tc::Clock)>,
) -> Result<tc::Checked<'static>, CheckFail> {
    let owner = body.typeck.owner;
    let mut hints = Needs::new_in(Alloc::global());
    let mut all_gaps: Vec<Gap, Global> = Vec::new_in(Global);
    for round in 0..MOST_ROUNDS {
        let view: BodyView<'_> = body.typeck.view(body.tys.view());
        let world = world_at(r, scope);
        let mut cx = InferCtx::new_in(Alloc::global());
        let out = match &mut count {
            Some((tally, clock)) => tc::check_counted(&view, &world, &mut cx, &hints, tally, *clock, Alloc::global()),
            None => tc::check(&view, &world, &mut cx, &hints, Alloc::global()),
        }
        .map_err(|error| CheckFail::Check { owner, error })?;
        if let Some((tally, _)) = &mut count {
            cx.add_solver_counts(&mut tally.solve);
        }
        let mut fills = world.fills.into_inner();
        if r.order == BuildOrder::Reversed {
            fills.reverse();
        }
        // A failed candidate is a gap only of a question it is a candidate of (the world's
        // `impls_of`/`inherent`), never of every body checked after it failed.
        let gaps = world.gaps.into_inner();
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
                diags.extend(done.diags.iter().map(|d| (d.code, d.at, diag_about(r, tys, d))));
                return Err(CheckFail::Diags { owner, diags });
            }
            Outcome::Done(done) if !round_gaps => return Ok(done),
            // A gap answered `Missing`, so a finished check past one is no answer.
            Outcome::Done(_) => return Err(CheckFail::Gap { owner, gaps: all_gaps }),
            Outcome::Need(needs) => needs,
        };
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
fn make_round(r: &mut Records<'_, '_>, fills: Vec<Fill, Global>, owner: ItemKey) -> Result<u64, CheckFail> {
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
        Some((_, e)) if made == 0 => Err(e.at(owner)),
        _ => Ok(dropped),
    }
}

/// An item's name for a finding: its written name, else its key.
pub(crate) fn item_shown(r: &Records<'_, '_>, k: ItemKey) -> String {
    match r.keys.library(k).and_then(|lib| r.names.item_text(lib)).and_then(|t| t.name) {
        Some(n) => n.to_string(),
        None => format!("{k:?}"),
    }
}

/// `t` of `tys` as Rust spells it (`Result<Out, Missing>`), by a work list; a node with no
/// spelling here is its `Debug`.
fn ty_shown(r: &Records<'_, '_>, tys: tc::PTys<'_>, t: tc::PTy) -> String {
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
fn diag_about(r: &Records<'_, '_>, tys: tc::PTys<'_>, d: &tc::Diag) -> Box<str> {
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
                    tc::UnmetGoal::Projection { name, term, .. } => format!("`<{self_} as {tr}>::{name:?} == {}`", ty_shown(r, tys, term)),
                }
            };
            match leaf {
                Some(l) => format!("{} because {}", shown(l), shown(goal)),
                None => shown(goal),
            }
        }
        Some(tc::Because::NoLangItem(l)) => format!("no lang item {l:?}"),
        // Every goal's finding (E0277, E0283) carries `Unmet`; what is left is about a node, not a
        // goal: its types and the name it looked up.
        Some(other) => format!("because {other:?}, found {:?}, expected {:?}", ty(d.found), ty(d.expected)),
        None => format!("found {:?}, expected {:?}, name {:?}", ty(d.found), ty(d.expected), d.name),
    };
    s.into_boxed_str()
}

/// The world a body written at `scope` is checked in, over `r`'s records as they are now.
pub(super) fn world_at<'w, 'n>(r: &'w Records<'_, 'n>, scope: &'w BodyScope<'w>) -> BodyWorld<'w, 'n> {
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
        fills: core::cell::RefCell::new(Vec::new_in(Global)),
        gaps: core::cell::RefCell::new(Vec::new_in(Global)),
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
pub fn callee_target(r: &mut Records<'_, '_>, scope: &BodyScope<'_>, owner: ItemKey, item: ItemKey, args: tc::List, ground: &mut tc::PortableTys<'_>) -> Result<Target, CheckFail> {
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
    for _ in 0..MOST_ROUNDS {
        let world = world_at(r, scope);
        let out = tc::resolve_instance(&world, item, args, ground, Alloc::global());
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
            tc::PNode::Closure { args: l, .. } => Some(l),
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
    fn at(self, owner: ItemKey) -> CheckFail {
        match self {
            MakeFail::Head(error) => CheckFail::Head { owner, error },
            MakeFail::Buckets(error) => CheckFail::Buckets { owner, error },
            MakeFail::Const(key, error) => CheckFail::Const { owner, key, error: Box::new_in(error, Global) },
        }
    }
}

/// The most constants one fill may need evaluated before it is made (rule 18).
const MOST_CONSTS: u32 = 64;

/// Makes `fill`; a head whose array length names a constant ([`Why::NeedConst`]) has that
/// constant evaluated (`Records::consts`) and the fill made again. While a constant is being
/// evaluated the evaluator is taken out of `r`, so one needed inside it is
/// [`ConstFail::Nested`] naming it: never a nested evaluation.
fn make(r: &mut Records<'_, '_>, fill: Fill) -> Result<(), MakeFail> {
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
        eval_len(r, c).map_err(|e| MakeFail::Const(c, e))?;
    }
}

/// Evaluates the `usize` constant `c` an array length names and files its value in
/// `Maps::const_lens`. The evaluator is taken out of `r` meanwhile, so a length met inside
/// that evaluation is [`ConstFail::Nested`]: never a nested evaluation.
pub fn eval_len(r: &mut Records<'_, '_>, c: LibKey) -> Result<(), ConstFail> {
    let Some((consts, prepared)) = r.consts.take() else {
        return Err(ConstFail::Nested { item: checker_of(c) });
    };
    let value = consts.value(r, prepared, checker_of(c)).map(int_of);
    r.consts = Some((consts, prepared));
    let v = value?.and_then(|v| u64::try_from(v).ok()).ok_or(ConstFail::NotUsize { item: checker_of(c) })?;
    r.store.maps.const_lens.insert(c, v);
    Ok(())
}

/// Makes every fill of a round, in the given order; a refusal named is that of the least fill
/// (by its `Debug` text: `Shape` has no order), so the error does not depend on the order.
pub(super) fn make_all(r: &mut Records<'_, '_>, fills: Vec<Fill, Global>, owner: ItemKey) -> Result<(), CheckFail> {
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
        Some((_, e)) => Err(e.at(owner)),
        None => Ok(()),
    }
}

/// Makes the answer to `ImplsOf(trait_, shape)` from the one impl index (the same buckets the
/// checker's questions fill) and returns the impls it holds; `None` when a candidate's header
/// refused: the question then stays a gap, so the back half's asking it refuses by name.
pub fn answer_impls(r: &mut Records<'_, '_>, trait_: ItemKey, shape: tc::Shape) -> Option<Vec<ItemKey, Global>> {
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

fn make_once(r: &mut Records<'_, '_>, fill: Fill) -> Result<(), MakeFail> {
    match fill {
        Fill::Store(want) => r.store.ensure(r.names, r.keys, want).map_err(MakeFail::Head),
        Fill::Impls(trait_, shape) => {
            // Only impls written where the orphan rules allow (`trait_home`) are candidates.
            let home = trait_home(r.names, r.keys, r.store, trait_, shape);
            let b = r.buckets.get_or_insert_with(|| Buckets::new(r.names, r.index));
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
        Fill::Inherent(shape) => {
            // Only impls written in the type's own crate (`inherent_home`) are candidates.
            let home = inherent_home(r.names, r.keys, shape);
            let b = r.buckets.get_or_insert_with(|| Buckets::new(r.names, r.index));
            let candidates = b.inherent_candidates(shape, home);
            lower_heads(r.names, r.keys, r.store, b, candidates, r.order)?;
            b.answer_inherent(r.store, shape, home).map_err(MakeFail::Buckets)
        }
    }
}

/// Lowers the headers of `candidates` the store does not hold yet, counting them.
fn lower_heads<'n>(names: &'n Names<'n>, keys: &mut Keys, store: &mut Store<'n>, b: &mut Buckets, mut candidates: Vec<LibKey, Global>, order: BuildOrder) -> Result<(), MakeFail> {
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
                Err(e) => b.fail(k, format!("{e}").into_boxed_str()),
            }
        }
    }
    match refused {
        Some((_, e)) => Err(MakeFail::Head(e)),
        None => Ok(()),
    }
}
