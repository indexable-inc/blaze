//! The real type context: [`InferCtx`] implements [`TypeCtx`] over blaze-ty's arena and
//! inference tables, with blaze-solve's solver behind it (typeck.md, "`TypeCtx`").
//!
//! - `terms`: blaze-solve's `Tys` over blaze-ty (one term store; see its module).
//! - `facts`: blaze-solve's `Facts` as a cache filled through the body's `Reads`, the one read
//!   seam: every fact a goal reads enters the body's read list.
//!
//! Handles: typeck's `Ty` and `TyList` are blaze-ty node bits. Regions are erased (typeck sees
//! types as rustc's typeck does). An associated-type alias is blaze-ty's `Alias(Projection,
//! assoc_def(trait, name), args)`; a closure is `Closure(def, [kind, sig, ()])` with its
//! expression kept in a side table, so `view` gives typeck's form back.
//!
//! Obligations live in this context's own columns (goal, state, an undo log of state changes),
//! so a snapshot covers them and `rollback_to` undoes registrations and proofs alike. Each
//! `select` hands the pending ones to a fresh `Fulfill` (wake lists within the call); the
//! solver's task cache carries proofs across calls.
//!
//! A solver question the cache lacks is pulled (`Pull`, blaze-solve `Source`): the cache reads
//! it through `Reads` at the step that asked, and the step goes on, `ConstHolds` too (its
//! arguments exported through an `Exporter`, the parts of the context the solver does not
//! hold). No evaluation is asked again from its root for a fact: [`InferCtx::need_rounds`] is 0
//! but under the test strategy that fills nothing (`stop_for_facts`). A question the world
//! lacks stops the body with typeck's `Need`, as every read does.

mod facts;
mod terms;

pub use facts::{FactCache, assoc_def};
pub use terms::{Terms, def_of, item_key, var_index, var_of_index};
pub use crate::ty::TyFlags;

use blaze_hash::HashMap;
use blaze_own::{Alloc, Vec};
use blaze_solve::eval::Answer;
use blaze_solve::fulfill::{Fulfill, State, Wake};
use blaze_solve::term::{Def as SDef, List as SList, Polarity, Pred, Ty as STy, TypingMode};
use blaze_solve::{Clause, EnvId, Goal as SGoal, Solver, Tys as _};
use blaze_ty::{AliasKind, ConstKind, FnHeader, Node, Region, TyKind};

use crate::error::{CheckError, Stop};
use crate::ids::{ExprId, ItemKey, Name, RecordSchema};
use crate::ty::{FulfillError, FulfillErrorKind, Goal, Normalized, Obligation, Ty, TyList, TyView, TypeCtx, Var, VarKind};
use crate::world::{Reads, World};

/// The most fact-fill rounds one solver call may take (rule 18): each round fills one question
/// no earlier round filled, so a world never comes near it. Only the test strategy that fills
/// nothing mid-step (`stop_for_facts`) takes a round; the pull answers every question in place.
const MAX_FILLS: u32 = 1 << 16;

/// The real type context of one body's check.
pub struct InferCtx<'a> {
    alloc: Alloc<'a>,
    pub terms: Terms<'a>,
    pub facts: FactCache<'a>,
    solver: Solver<'a>,
    env_clauses: Vec<Clause, Alloc<'a>>,
    env: Option<EnvId>,
    /// Obligations, in columns.
    obs: Vec<Obligation, Alloc<'a>>,
    preds: Vec<Pred, Alloc<'a>>,
    state: Vec<State, Alloc<'a>>,
    /// `(obligation, previous state)` for every state change, for rollback.
    undo: Vec<(u32, State), Alloc<'a>>,
    errors: Vec<FulfillError, Alloc<'a>>,
    /// The next error `next_error` hands out.
    error_at: usize,
    /// What `view` reads besides the terms and the facts: the body terms' own rows.
    rows: BodyRows<'a>,
    /// Solver reads (the solver's own log; the body's read list is `Reads`): emptied after each
    /// call, as nothing here reads it (it grew for the whole body before).
    solver_reads: Vec<blaze_solve::Read, Alloc<'a>>,
    /// Solver calls repeated after a fact was filled: the Need rounds inside the context.
    pub need_rounds: u64,
    /// How many `Specializes` questions this context is answering for the contexts above it
    /// (each is answered in a context of its own, one deeper): bounded by `MAX_SPEC_DEPTH`.
    spec_depth: u8,
    /// The typing mode every solver call of this context runs in, fixed by its constructor
    /// ([`InferCtx::analysis_in`], [`InferCtx::post_analysis_in`]): what an opaque type is to
    /// normalization (blaze-solve alias.rs `normalize_opaque_type`).
    mode: TypingMode,
    /// A planted bug of the body suite; `InferPlant::None` in every other test.
    #[cfg(test)]
    pub(crate) plant: InferPlant,
    /// The other strategy (rule 78): the source fills nothing, so every fact the cache lacks
    /// stops the solve and is filled by the caller's loop, the root asked again
    /// (tests.rs `pull_and_stop_check_alike`).
    #[cfg(test)]
    pub(crate) stop_for_facts: bool,
}

/// The deepest chain of `Specializes` questions asked while one is answered (an impl's
/// where-clause whose own selection weighs two impls that specialize). Past it the check stops
/// with `Limit::Specialization` rather than recursing.
const MAX_SPEC_DEPTH: u8 = 8;

/// The context's planted bugs (`suite`'s controls).
#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum InferPlant {
    None,
    /// Impl buckets keep only their first impl (`FactCache::first_impl_only`).
    FirstImpl,
    /// A trait obligation still ambiguous at `select_all` is settled by the first impl of its
    /// bucket whose header unifies.
    AmbiguityByOrder,
    /// An impl's `where const` clauses are passed over (`FactCache::drop_const_clauses`).
    DropConstClauses,
    /// Two arrays unify when their elements do, whatever their lengths (`unify`).
    AnyLength,
    /// A pattern type unifies as its base when the other side is not a pattern type (`unify`).
    PatIsBase,
}

/// A closure this body made, by its def: its expression, its signature as typeck made it
/// (`fn(inputs.., output)`), its kind variable and its capture-tuple variable.
#[derive(Clone, Copy, Debug)]
struct ClosureRow {
    expr: ExprId,
    sig: Ty,
    kind: blaze_ty::Const,
    upvars: blaze_ty::Ty,
}

/// A closure of another body as typeck sees it (`TyView::ClosureOf`), by the solver type `mk`
/// made for it: `view` gives it back.
#[derive(Clone, Copy, Debug)]
struct ForeignClosure {
    /// The solver's term `mk` made for it.
    made: blaze_ty::Ty,
    owner: ItemKey,
    ordinal: u32,
    args: TyList,
    sig: Ty,
    kind: crate::ty::ClosureKind,
}

/// An async block's future of another body as typeck sees it (`TyView::CoroutineOf`), by the
/// solver type `mk` made for it: `view` gives it back. Its solver list is the output,
/// the interior, then every argument of the owner, including parameters absent from both.
#[derive(Clone, Copy, Debug)]
struct ForeignCoroutine {
    /// The solver's term `mk` made for it.
    made: blaze_ty::Ty,
    owner: ItemKey,
    ordinal: u32,
    args: TyList,
    output: Ty,
    interior: Ty,
}

/// The body terms' own rows: what typeck's `view` of a solver term reads besides the term
/// arena (`Terms`) and the facts (`FactCache`: alias names, hidden bodies). One home, apart from
/// the solver, so a fact the solver asks for mid-step can be read through [`Exporter`] while
/// the solver holds the rest of the context borrowed (`Pull::fill`, a `where const` condition).
pub(crate) struct BodyRows<'a> {
    closures: HashMap<u128, ClosureRow, Alloc<'a>>,
    /// Each closure of another body this context made, in the order made (a body names few).
    foreign: Vec<ForeignClosure, Alloc<'a>>,
    /// Each async block's future this body made, by its def: its expression.
    futures: HashMap<u128, ExprId, Alloc<'a>>,
    /// Each future of another body this context made (`TyView::CoroutineOf`).
    foreign_coroutines: Vec<ForeignCoroutine, Alloc<'a>>,
    /// The first handle a read could not read (`Malformed::Unreadable`): the `TypeCtx` reads
    /// return no error, so they give the error type and leave this, and the check of the body
    /// fails with it when it ends (lib.rs `run`, `TypeCtx::take_malformed`; rule 20).
    malformed: core::cell::Cell<Option<crate::error::Malformed>>,
}

impl<'a> BodyRows<'a> {
    fn new_in(alloc: Alloc<'a>) -> BodyRows<'a> {
        BodyRows {
            closures: HashMap::new_in(alloc),
            foreign: Vec::new_in(alloc),
            futures: HashMap::new_in(alloc),
            foreign_coroutines: Vec::new_in(alloc),
            malformed: core::cell::Cell::new(None),
        }
    }
}

/// A shared view of a body's terms: the reads `TypeCtx::view` makes, written once, for the
/// context ([`InferCtx`]) and for [`Exporter`] alike (rule 30).
#[derive(Clone, Copy)]
pub(crate) struct Seen<'x, 'a> {
    terms: &'x Terms<'a>,
    facts: &'x FactCache<'a>,
    rows: &'x BodyRows<'a>,
}

/// The reads and the one write (`adopt`) a ground export of a term makes (instance.rs
/// `export`), over the parts of the context it needs and nothing else: the terms, the facts
/// read only, the body rows. Built by `InferCtx::exporter`, or in `Pull::fill` from the terms
/// the solver hands it and the pull's own borrows, so a `where const` condition is answered
/// where the solver meets it (no evaluation asked again from its root, D161 loop 3).
pub(crate) struct Exporter<'x, 'a> {
    terms: &'x mut Terms<'a>,
    facts: &'x FactCache<'a>,
    rows: &'x mut BodyRows<'a>,
    alloc: Alloc<'a>,
}

/// A snapshot of the context: the arena's tables and the obligation columns.
#[derive(Clone, Copy, Debug)]
pub struct InferSnapshot {
    cx: blaze_ty::Snapshot,
    obs: usize,
    undo: usize,
    errors: usize,
    error_at: usize,
    env_clauses: usize,
    env: Option<EnvId>,
}

fn solve_err(e: blaze_solve::Stop) -> Result<(), CheckError> {
    match e {
        blaze_solve::Stop::Error(e) => Err(CheckError::Solve(e)),
        blaze_solve::Stop::Need(_) => Ok(()),
    }
}

/// The solver's fact source over this context's cache (blaze-solve `Source`, the pull): a
/// question the cache lacks is read through `Reads` (`FactCache::fill`, the same read the stop
/// made, so the body's read list is the same set) the moment a solver step meets it, and the
/// step goes on; no evaluation is asked again from its root for a fact.
///
/// Every question is pulled, `ConstHolds` too (D161 loop 3): its arguments are terms of this
/// context, exported to a ground table through an [`Exporter`] over the terms the solver hands
/// `fill` and this pull's borrows of the facts and the body rows ([`fill_const_holds`]). So the
/// caller's fill loops (`InferCtx::env`, `evaluate`, `select`) meet `Stop::Need` only under
/// the test strategy that fills nothing (`stop_for_facts`); `Solve::refills` is 0 otherwise.
struct Pull<'x, 'a, 'w, 'r, W: World> {
    facts: &'x mut FactCache<'a>,
    rows: &'x mut BodyRows<'a>,
    reads: &'x mut Reads<'w, 'r, W>,
    alloc: Alloc<'a>,
    spec_depth: u8,
    /// The need rounds of the `Specializes` contexts this source opened.
    need_rounds: u64,
    /// The time spent in fills, taken out of the solver's own time.
    fill_ns: u64,
    /// Why a fill failed (typeck's `Need` of the world, a malformed record): the solver then
    /// stops with `Need(q)`, and the caller returns this.
    err: Option<Stop>,
    /// `InferCtx::stop_for_facts`: fill nothing.
    #[cfg(test)]
    stop: bool,
}

impl<'x, 'a, 'w, 'r, W: World> Pull<'x, 'a, 'w, 'r, W> {
    fn new(facts: &'x mut FactCache<'a>, rows: &'x mut BodyRows<'a>, reads: &'x mut Reads<'w, 'r, W>, alloc: Alloc<'a>, spec_depth: u8) -> Self {
        Pull {
            facts,
            rows,
            reads,
            alloc,
            spec_depth,
            need_rounds: 0,
            fill_ns: 0,
            err: None,
            #[cfg(test)]
            stop: false,
        }
    }
}

impl<'a, W: World> blaze_solve::Source<Terms<'a>> for Pull<'_, 'a, '_, '_, W> {
    type Facts = FactCache<'a>;
    const PULLS: bool = true;

    fn facts(&self) -> &FactCache<'a> {
        &*self.facts
    }

    fn goals(&self) -> Option<&dyn blaze_solve::share::GoalShare> {
        self.reads.goal_share()
    }

    fn fill(&mut self, tys: &mut Terms<'a>, q: blaze_solve::facts::Question) -> bool {
        if self.err.is_some() {
            return false;
        }
        #[cfg(test)]
        if self.stop {
            return false;
        }
        let at = self.reads.stamp();
        let r = match q {
            blaze_solve::facts::Question::ConstHolds(cond, list) => {
                fill_const_holds(tys, self.facts, self.rows, self.reads, self.alloc, cond, list)
            }
            blaze_solve::facts::Question::Specializes(a, b) => {
                fill_specializes(self.facts, self.reads, self.alloc, self.spec_depth, &mut self.need_rounds, a, b)
            }
            _ => self.facts.fill(q, self.reads),
        };
        self.fill_ns += self.reads.stamp().saturating_sub(at);
        match r {
            Ok(()) => true,
            Err(e) => {
                self.err = Some(e);
                false
            }
        }
    }
}

/// Answers the solver's `Specializes(a, b)` (specialize.rs `specializes`, RFC 1210) in a
/// context of its own: impl `a`'s parameters are rigid `Param(i)` there and its
/// where-clauses the only assumptions, which in the asking context would name the body's own
/// parameters and stand beside the body's where-clauses. The answer is a fact of the two
/// impls, kept in `facts` for the rest of the asking context.
fn fill_specializes<W: World>(
    facts: &mut FactCache<'_>,
    reads: &mut Reads<'_, '_, W>,
    alloc: Alloc<'_>,
    spec_depth: u8,
    need_rounds: &mut u64,
    a: SDef,
    b: SDef,
) -> Result<(), Stop> {
    if facts.has(blaze_solve::facts::Question::Specializes(a, b)) {
        return Ok(());
    }
    if spec_depth >= MAX_SPEC_DEPTH {
        return Err(CheckError::Limit(crate::error::Limit::Specialization).into());
    }
    let (ka, kb) = (ItemKey(a.0), ItemKey(b.0));
    let Some(ha) = reads.impl_header(ka)? else {
        return Err(CheckError::Malformed(crate::error::Malformed::NoRecord(crate::world::Question::ImplHeader(ka))).into());
    };
    let Some(trait_) = ha.trait_ else {
        return Err(CheckError::Malformed(crate::error::Malformed::NoRecord(crate::world::Question::ImplHeader(ka))).into());
    };
    if let Some(hb) = reads.impl_header(kb)?
        && hb.trait_ == Some(trait_)
        && !may_specialize_headers(&ha, &hb)
    {
        facts.keep_specializes(a, b, false);
        return Ok(());
    }
    // A fact of two impls, asked of their headers alone (no body's opaque in it).
    let mut inner = InferCtx::analysis_in(alloc);
    inner.spec_depth = spec_depth + 1;
    let got = crate::specialize::specializes(&mut inner, reads, ka, kb, trait_);
    *need_rounds += inner.need_rounds;
    let v = match got {
        Ok(v) => v,
        Err(crate::instance::ResolveError::Missing(q)) => return Err(Stop::Need(q)),
        Err(crate::instance::ResolveError::Check(e)) => return Err(e.into()),
        // Two impls of different traits: the solver asks only of two impl candidates of
        // one goal, so this is a malformed question.
        Err(crate::instance::ResolveError::NoImpl { item, .. }) => {
            return Err(CheckError::Malformed(crate::error::Malformed::NoRecord(crate::world::Question::ImplHeader(item))).into());
        }
        Err(crate::instance::ResolveError::Overlap { first, .. } | crate::instance::ResolveError::NotGround { item: first }) => {
            return Err(CheckError::Malformed(crate::error::Malformed::NoRecord(crate::world::Question::ImplHeader(first))).into());
        }
    };
    facts.keep_specializes(a, b, v);
    Ok(())
}

/// Fast sound pre-filter for `specializes(a, b)`: returns `false` only when rigid top-level
/// constructors in `hb` (general) and `ha` (specific, with `Param` rigid) definitely cannot unify,
/// skipping full `InferCtx` allocation for disjoint impl pairs (e.g. `impl Add<u8> for u8` vs
/// `impl Add<i32> for i32`). Returns `true` conservatively on any alias, opaque, error, or param
/// in `hb`.
fn may_specialize_headers(ha: &crate::world::ImplHeader<'_>, hb: &crate::world::ImplHeader<'_>) -> bool {
    if !may_specialize_ty(ha.types, ha.self_ty, hb.types, hb.self_ty) {
        return false;
    }
    let Ok(a_rest) = ha.types.list(ha.trait_args.list()) else { return true };
    let Ok(b_rest) = hb.types.list(hb.trait_args.list()) else { return true };
    if a_rest.len() != b_rest.len() {
        return false;
    }
    for (&at, &bt) in a_rest.iter().zip(b_rest.iter()) {
        if !may_specialize_ty(ha.types, at, hb.types, bt) {
            return false;
        }
    }
    true
}

fn may_specialize_ty(
    a_tys: crate::portable::PTys<'_>,
    at: crate::portable::PTy,
    b_tys: crate::portable::PTys<'_>,
    bt: crate::portable::PTy,
) -> bool {
    use crate::portable::PNode as N;
    let (Ok(na), Ok(nb)) = (a_tys.node(at), b_tys.node(bt)) else { return true };
    match nb {
        // `b`'s parameters become fresh inference vars, and aliases/opaques/errors may normalize.
        N::Param(_) | N::ConstParam(_) | N::Alias { .. } | N::Opaque(..) | N::Infer | N::Error => return true,
        _ => {}
    }
    match na {
        // `a`'s aliases/opaques/errors may normalize to `b`'s head.
        N::Alias { .. } | N::Opaque(..) | N::Infer | N::Error => return true,
        // `a`'s rigid `Param`/`ConstParam` cannot unify with a non-param rigid constructor in `b`.
        N::Param(_) | N::ConstParam(_) => return false,
        _ => {}
    }
    match (na, nb) {
        (N::Bool, N::Bool) | (N::Char, N::Char) | (N::Str, N::Str) | (N::Never, N::Never) => true,
        (N::Int(x), N::Int(y)) => x == y,
        (N::Uint(x), N::Uint(y)) => x == y,
        (N::Float(x), N::Float(y)) => x == y,
        (N::Adt(x, _), N::Adt(y, _)) => x == y,
        (N::FnDef(x, _), N::FnDef(y, _)) => x == y,
        (N::Foreign(x), N::Foreign(y)) => x == y,
        (N::Ref(x, _), N::Ref(y, _)) => x == y,
        (N::Ptr(x, _), N::Ptr(y, _)) => x == y,
        (N::Tuple(x), N::Tuple(y)) => x.len == y.len,
        (N::Record(sx, lx), N::Record(sy, ly)) => sx == sy && lx.len == ly.len,
        (N::Union(x), N::Union(y)) => x.len == y.len,
        (N::Array(..), N::Array(..))
        | (N::Slice(_), N::Slice(_))
        | (N::Pat(..), N::Pat(..))
        | (N::FnPtr(..), N::FnPtr(..))
        | (N::Closure { .. }, N::Closure { .. })
        | (N::Future(..), N::Future(..))
        | (N::Coroutine { .. }, N::Coroutine { .. })
        | (N::ConstValue { .. }, N::ConstValue { .. })
        | (N::Dynamic(_), N::Dynamic(_))
        | (N::ExTrait(..), N::ExTrait(..))
        | (N::ExProj { .. }, N::ExProj { .. })
        | (N::ExAuto(_), N::ExAuto(_)) => true,
        _ => false,
    }
}

/// Answers the solver's `ConstHolds(cond, list)` (a `where const` condition, blaze's
/// extension): the arguments, terms of this context, exported as a ground table through an
/// [`Exporter`] over `terms`, `facts` and `rows`, which names the condition's record; the answer
/// kept in `facts`. The one body of the answer, for the pull (`Pull::fill`, mid-step) and the
/// test strategy's stop (`InferCtx::fill`).
fn fill_const_holds<'a, W: World>(
    terms: &mut Terms<'a>,
    facts: &mut FactCache<'a>,
    rows: &mut BodyRows<'a>,
    reads: &mut Reads<'_, '_, W>,
    alloc: Alloc<'a>,
    cond: SDef,
    list: SList,
) -> Result<(), Stop> {
    let mut args: Vec<STy, Alloc<'a>> = Vec::with_capacity_in(list.len as usize, alloc);
    for i in 0..list.len {
        args.push(terms.list_get(list, i));
    }
    if facts.const_holds_known(cond, &args) {
        return Ok(());
    }
    let mut ground = crate::portable::PortableTys::new_in(alloc);
    let mut out: Vec<crate::portable::PTy, Alloc<'a>> = Vec::with_capacity_in(args.len(), alloc);
    {
        let mut ex = Exporter { terms, facts: &*facts, rows, alloc };
        for &s in args.iter() {
            match crate::instance::export(&mut ex, Ty(s.0), &mut ground, 0)? {
                Some(p) => out.push(p),
                None => return Err(CheckError::Malformed(crate::error::Malformed::NotGround { cond: ItemKey(cond.0) }).into()),
            }
        }
    }
    let got = reads.const_holds(ItemKey(cond.0), &out, ground.view())?;
    facts.keep_const_holds(cond, &args, got)?;
    Ok(())
}

/// Owner-qualified closure and coroutine nodes in dependency order, including nodes
/// reached through another body's checked signature, output, or stored state. A caller
/// converts each returned node before converting its parent and retains its read digest.
pub(super) fn hidden_body_nodes<'w, 'a, W: World>(
    question: crate::world::Question, hidden: crate::world::Hidden<'w>, reads: &mut Reads<'w, '_, W>, alloc: Alloc<'a>,
) -> Result<Vec<(crate::world::Question, crate::world::Hidden<'w>), Alloc<'a>>, Stop> {
    use crate::error::{Limit, Malformed};
    use crate::portable::{PNode, PTy};
    use crate::world::{Hidden, Question};

    const MAX_NODES: usize = 65_536;
    let mut ordered = Vec::new_in(alloc);
    let mut work = Vec::new_in(alloc);
    let mut finished = HashMap::new_in(alloc);
    let mut children = Vec::new_in(alloc);
    work.push((question, hidden, false));
    while let Some((q, h, leave)) = work.pop() {
        let key = (q, h.ty);
        let node = h.types.node(h.ty)?;
        if leave {
            finished.insert(key, true);
            if matches!(node, PNode::Closure { .. } | PNode::Coroutine { .. }) {
                ordered.push((q, h));
            }
            continue;
        }
        match finished.get(&key) {
            Some(true) => continue,
            Some(false) => return Err(CheckError::Malformed(Malformed::BodyRecordCycle { record: q, node: h.ty.0 }).into()),
            None => {}
        }
        if finished.len() >= MAX_NODES || work.len() >= MAX_NODES {
            return Err(CheckError::Limit(Limit::HiddenBodyNodes { record: q, node: h.ty, limit: MAX_NODES }).into());
        }
        finished.insert(key, false);
        work.push((q, h, true));
        children.clear();
        crate::portable::node_children(h.types, node, &mut children)?;
        for &child in children.iter() {
            if child >= h.ty.0 {
                return Err(CheckError::Malformed(Malformed::TypeOrder { node: h.ty.0, child }).into());
            }
            work.push((q, Hidden { ty: PTy(child), types: h.types }, false));
        }
        match node {
            PNode::Coroutine { owner, ordinal, .. } => {
                let key = Question::Coroutine(owner, ordinal);
                let facts = reads.coroutine(owner, ordinal)?.ok_or(CheckError::Malformed(Malformed::NoRecord(key)))?;
                for ty in [facts.output, facts.interior] {
                    work.push((key, Hidden { ty, types: facts.types }, false));
                }
            }
            PNode::Closure { owner, ordinal, .. } => {
                let key = Question::Closure(owner, ordinal);
                let (facts, env) = reads.closure(owner, ordinal)?.ok_or(CheckError::Malformed(Malformed::NoRecord(key)))?;
                work.push((key, Hidden { ty: facts.sig, types: facts.types }, false));
                if let Some(ty) = env {
                    work.push((key, Hidden { ty, types: facts.types }, false));
                }
            }
            PNode::Future(block, ..) => {
                let why = match q {
                    Question::Coroutine(owner, ordinal) => Malformed::NestedFutureIdentity { owner, ordinal, block, reader: None },
                    _ => Malformed::FutureOutsideOwner { block, reader: None },
                };
                return Err(CheckError::Malformed(why).into());
            }
            _ => {}
        }
    }
    Ok(ordered)
}

impl<W: World> crate::instance::Asked<'_, '_, '_, W> {
    /// A nominal coroutine carries the defining item's arguments. Its output and
    /// stored types come from the same read used to lower that type in a body.
    pub(crate) fn import_coroutine(
        &mut self, cx: &mut InferCtx<'_>, owner: ItemKey, ordinal: u32, args: TyList, depth: u32,
    ) -> Result<Ty, Stop> {
        let q = crate::world::Question::Coroutine(owner, ordinal);
        let facts = self.reads.coroutine(owner, ordinal)?
            .ok_or(CheckError::Malformed(crate::error::Malformed::NoRecord(q)))?;
        let mut subst = Vec::with_capacity_in(cx.list_len(args) as usize, cx.alloc());
        for i in 0..cx.list_len(args) {
            subst.push(cx.list_get(args, i));
        }
        let mut import_field = |p| {
            crate::instance::import(cx, self, facts.types, p, &subst, depth).map_err(|e| match e {
                Stop::Error(CheckError::Malformed(crate::error::Malformed::FutureOutsideOwner { block, reader })) => {
                    CheckError::Malformed(crate::error::Malformed::NestedFutureIdentity { owner, ordinal, block, reader }).into()
                }
                other => other,
            })
        };
        let output = import_field(facts.output)?;
        let interior = import_field(facts.interior)?;
        Ok(cx.imported_coroutine(owner, ordinal, args, output, interior)?)
    }
}

impl<'a> InferCtx<'a> {
    /// The allocator the check's caller handed in (D102): what this check's scratch lives in.
    pub(crate) fn alloc(&self) -> Alloc<'a> {
        self.alloc
    }

    /// A block imported at the root of writeback uses the owner's identity arguments.
    /// Inside a signature the enclosing import supplies its substitution.
    pub(crate) fn instance_owner_args(
        &mut self, owner: ItemKey, count: u32, consts: &[(u32, crate::portable::PTy)], subst: &[Ty],
    ) -> Result<TyList, CheckError> {
        if subst.len() == count as usize {
            return self.mk_list(subst);
        }
        if !subst.is_empty() {
            return Err(CheckError::Malformed(crate::error::Malformed::CoroutineSubstitution {
                owner, expected: count, given: subst.len(),
            }));
        }
        let mut args = Vec::with_capacity_in(count as usize, self.alloc);
        for i in 0..count {
            let is_const = consts.binary_search_by_key(&i, |&(index, _)| index).is_ok();
            args.push(self.mk(if is_const { TyView::ConstParam(i) } else { TyView::Param(i) })?);
        }
        self.mk_list(&args)
    }

    /// Imports a future from the body currently being written back. Use its nominal
    /// identity here so exporting the selected impl's arguments never exports a local
    /// expression id. The output and interior are types of this same body.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn import_instance_future<S: crate::instance::ClosureSource>(
        &mut self, source: &mut S,
        body: &crate::body::BodyView<'_>, block: ExprId, owner_args: TyList,
        types: crate::portable::PTys<'_>, output: crate::portable::PTy, interior: crate::portable::PTy,
        subst: &[Ty], depth: u32,
    ) -> Result<Ty, S::Error> {
        let Some(ordinal) = crate::coroutine_id::coroutine_ordinal(body, block) else {
            let at = if (block.0 as usize) < body.exprs.len() { block } else { body.root };
            return Err(CheckError::Malformed(crate::error::Malformed::FutureIdentity {
                owner: body.owner, block, at: body.expr_range(at),
            }).into());
        };
        let output = crate::instance::import(self, source, types, output, subst, depth)?;
        let interior = crate::instance::import(self, source, types, interior, subst, depth)?;
        Ok(self.imported_coroutine(body.owner, ordinal, owner_args, output, interior)?)
    }

    /// The solver term for a portable coroutine instance. Its bounds use the first
    /// two elements. Unification includes every owner argument, even when absent
    /// from the stored state and output. (`pub(crate)`: stalled.rs imports another fn's
    /// coroutine record through it.)
    pub(crate) fn imported_coroutine(&mut self, owner: ItemKey, ordinal: u32, args: TyList, output: Ty, interior: Ty) -> Result<Ty, CheckError> {
        let def = self.terms.bdef(SDef(crate::coroutine_id::coroutine_def(owner, ordinal)));
        let list = self.blist(args);
        let n = self.terms.cx.tys.list_len(list);
        let mut all = Vec::with_capacity_in(n as usize + 2, self.alloc);
        all.push(self.bty(output).arg());
        all.push(self.bty(interior).arg());
        for i in 0..n {
            let arg = self.terms.cx.tys.list_get(list, i).ok_or(CheckError::Malformed(
                crate::error::Malformed::Unreadable { node: args.0, what: crate::error::Unreadable::PastEnd { index: i } },
            ))?;
            all.push(arg);
        }
        let all = self.terms.cx.tys.mk_list(&all);
        let made = self.terms.cx.tys.coroutine(def, all);
        self.rows.foreign_coroutines.push(ForeignCoroutine { made, owner, ordinal, args, output, interior });
        Ok(Ty(made.node().bits()))
    }

    /// A context that checks a body, or answers a question asked while bodies are checked
    /// (`TypingMode::Analysis`): an opaque type outside its defining body is rigid, only its
    /// item bounds usable (the Reference, "Impl trait", "Abstract return types"); an auto
    /// trait still sees its hidden type (blaze-solve structural.rs).
    pub fn analysis_in(alloc: Alloc<'a>) -> InferCtx<'a> {
        InferCtx::in_mode(alloc, TypingMode::Analysis)
    }

    /// A context that answers a question AFTER type checking, at an instance's ground
    /// arguments (instance.rs `resolve_instance`, `select_impl`, `normalize_projection`,
    /// `holds`: instance resolution, layout, drop glue, a constant's evaluation):
    /// `TypingMode::PostAnalysis`, every opaque type with a hidden type normalizes to it, one
    /// layer per normalization (blaze-solve alias.rs `normalize_opaque_type`). A where-clause
    /// `<F as FnOnce<()>>::Output == U` with `F` a fn returning `impl Trait` and `U` that
    /// opaque's hidden type holds only so (suite/opaque_hidden_closure.rs).
    pub fn post_analysis_in(alloc: Alloc<'a>) -> InferCtx<'a> {
        InferCtx::in_mode(alloc, TypingMode::PostAnalysis)
    }

    /// [`InferCtx::analysis_in`] under its old name, for the callers outside this crate's
    /// writer's files until the one rewrite of every site (`InferCtx::new_in($A)` to
    /// `InferCtx::analysis_in($A)`) removes it: a new caller names its mode.
    pub fn new_in(alloc: Alloc<'a>) -> InferCtx<'a> {
        InferCtx::in_mode(alloc, TypingMode::Analysis)
    }

    fn in_mode(alloc: Alloc<'a>, mode: TypingMode) -> InferCtx<'a> {
        InferCtx {
            mode,
            alloc,
            terms: Terms::new_in(alloc),
            facts: FactCache::new_in(alloc),
            solver: Solver::new_in(alloc),
            env_clauses: Vec::new_in(alloc),
            env: None,
            obs: Vec::new_in(alloc),
            preds: Vec::new_in(alloc),
            state: Vec::new_in(alloc),
            undo: Vec::new_in(alloc),
            errors: Vec::new_in(alloc),
            error_at: 0,
            rows: BodyRows::new_in(alloc),
            solver_reads: Vec::new_in(alloc),
            need_rounds: 0,
            spec_depth: 0,
            #[cfg(test)]
            plant: InferPlant::None,
            #[cfg(test)]
            stop_for_facts: false,
        }
    }

    /// Notes that handle `node` could not be read (`what`), keeping the first such note, and
    /// gives `stand_in` (the error type, an empty list) for the read to go on with: the body's
    /// check fails with the note when it ends (`TypeCtx::take_malformed`).
    fn unreadable<T>(&self, node: u32, what: crate::error::Unreadable, stand_in: T) -> T {
        self.seen().unreadable(node, what, stand_in)
    }

    /// The shared view of this context's terms ([`Seen`]).
    fn seen(&self) -> Seen<'_, 'a> {
        Seen { terms: &self.terms, facts: &self.facts, rows: &self.rows }
    }

    /// The parts of this context a ground export reads and adopts into ([`Exporter`]).
    pub(crate) fn exporter(&mut self) -> Exporter<'_, 'a> {
        Exporter { terms: &mut self.terms, facts: &self.facts, rows: &mut self.rows, alloc: self.alloc }
    }

    /// The `AmbiguityByOrder` plant: each pending trait obligation whose self type has a shape
    /// takes the first impl of its bucket whose header unifies.
    #[cfg(test)]
    fn settle_by_order<W: World>(&mut self, reads: &mut Reads<'_, '_, W>) -> Result<(), Stop> {
        for i in 0..self.obs.len() {
            if self.state[i] != State::Pending {
                continue;
            }
            let Goal::Trait { trait_, args } = self.obs[i].goal else { continue };
            let self_ty = self.list_get(args, 0);
            let self_ty = self.shallow_resolve(self_ty);
            let Some(shape) = crate::instance::shape_of(self.view(self_ty), self) else { continue };
            for &imp in reads.impls_of(trait_, shape)? {
                let Some(h) = reads.impl_header(imp)? else { continue };
                if h.trait_ != Some(trait_) || h.polarity != crate::world::ImplPolarity::Positive {
                                    continue;
                                }
                let s = self.snapshot();
                if crate::instance::unify_header(self, &h, args)?.is_some() {
                    self.commit(s);
                    break;
                }
                self.rollback_to(s);
            }
        }
        Ok(())
    }

    /// Obligations registered and not yet rolled back (the tests' undo check).
    pub fn pending_len(&self) -> usize {
        self.obs.len()
    }

    /// Adds what this context's solver did into `s` (a measurement: asks.rs `Solve`).
    pub fn add_solver_counts(&self, s: &mut crate::asks::Solve) {
        s.refills += self.need_rounds;
        s.stats.add(&self.solver.stats());
    }

    /// Keeps a digest of each ground root goal the solver searches from now on (blaze-solve
    /// `Solver::keep_root_keys`: a measurement of goals shared across bodies).
    pub fn keep_root_keys(&mut self) {
        self.solver.keep_root_keys();
    }

    /// The shared goals this context's solver took (blaze-solve share.rs `GoalHit`): the one
    /// read each hit names, for the body's result.
    pub fn goal_hits(&self) -> &[blaze_solve::share::GoalHit] {
        self.solver.goal_hits()
    }

    /// The digests kept since [`InferCtx::keep_root_keys`].
    pub fn root_keys(&self) -> &[u64] {
        self.solver.root_keys()
    }

    /// Traces the trait goals `ask` names from now on (blaze-solve witness.rs, "the trait-goal
    /// trace"): a measurement for tests, off by default.
    pub fn trace_trait_goals(&mut self, ask: blaze_solve::witness::TraceAsk) {
        self.solver.trace_on(ask);
    }

    /// The trace kept since [`InferCtx::trace_trait_goals`], ending it.
    pub fn take_trace(&mut self) -> Option<blaze_solve::witness::Trace<'a>> {
        self.solver.take_trace()
    }

    pub(crate) fn imported_closure(
        &mut self, owner: ItemKey, ordinal: u32, args: TyList, sig: Ty, kind: crate::ty::ClosureKind, upvars: Option<Ty>,
    ) -> Result<Ty, CheckError> {
        use blaze_solve::term::{CLOSURE_FN, CLOSURE_FN_MUT, CLOSURE_FN_ONCE};
        let def = self.terms.bdef(SDef(crate::coroutine_id::closure_of_def(owner, ordinal)));
        let k = match kind {
            crate::ty::ClosureKind::Fn => CLOSURE_FN,
            crate::ty::ClosureKind::FnMut => CLOSURE_FN_MUT,
            crate::ty::ClosureKind::FnOnce => CLOSURE_FN_ONCE,
        };
        let k = self.terms.cx.tys.const_value(blaze_ty::Ty::uint(blaze_ty::UintTy::Usize), u128::from(k));
        let solver_sig = self.rust_call_sig(sig)?;
        let env = match upvars {
            Some(t) => self.bty(t),
            None => self.terms.cx.tys.foreign(def),
        };
        let bl = self.blist(args);
        let n = self.terms.cx.tys.list_len(bl);
        let mut all = Vec::with_capacity_in(n as usize + 3, self.alloc);
        for i in 0..n {
            let a = self.terms.cx.tys.list_get(bl, i).ok_or(CheckError::Malformed(crate::error::Malformed::Unreadable {
                node: args.0, what: crate::error::Unreadable::PastEnd { index: i },
            }))?;
            all.push(a);
        }
        all.extend([k.arg(), solver_sig.arg(), env.arg()]);
        let l = self.terms.cx.tys.mk_list(&all);
        let made = self.terms.cx.tys.closure(def, l);
        self.rows.foreign.push(ForeignClosure { made, owner, ordinal, args, sig, kind });
        Ok(Ty(made.node().bits()))
    }

    fn node(&self, bits: u32) -> Node {
        self.seen().node(bits)
    }

    fn bty(&self, t: Ty) -> blaze_ty::Ty {
        self.seen().bty(t)
    }

    /// Files `t` when it is a closure or async block term the SOLVER made ([`Exporter::adopt`]).
    pub(crate) fn adopt(&mut self, t: Ty) -> Result<(), CheckError> {
        self.exporter().adopt(t)
    }
}

impl<'x, 'a> Seen<'x, 'a> {
    /// Notes that handle `node` could not be read (`what`), keeping the first such note, and
    /// gives `stand_in` (`InferCtx::unreadable`).
    fn unreadable<T>(self, node: u32, what: crate::error::Unreadable, stand_in: T) -> T {
        let first = self.rows.malformed.take();
        self.rows.malformed.set(Some(first.unwrap_or(crate::error::Malformed::Unreadable { node, what })));
        stand_in
    }

    fn node(self, bits: u32) -> Node {
        match self.terms.cx.tys.node_at(bits) {
            Some(n) => n,
            None => self.unreadable(bits, crate::error::Unreadable::NotInArena, blaze_ty::Ty::ERROR.node()),
        }
    }

    fn bty(self, t: Ty) -> blaze_ty::Ty {
        match self.node(t.0).as_ty() {
            Some(t) => t,
            None => self.unreadable(t.0, crate::error::Unreadable::NotAType, blaze_ty::Ty::ERROR),
        }
    }

    fn blist(self, l: TyList) -> blaze_ty::List {
        match self.node(l.0).as_list() {
            Some(l) => l,
            None => self.unreadable(l.0, crate::error::Unreadable::NotAList, blaze_ty::List::EMPTY),
        }
    }

    fn list_len(self, l: TyList) -> u32 {
        self.terms.cx.tys.list_len(self.blist(l))
    }

    fn list_get(self, l: TyList, i: u32) -> Ty {
        match self.terms.cx.tys.list_get(self.blist(l), i) {
            Some(a) => Ty(a.bits()),
            None => self.unreadable(l.0, crate::error::Unreadable::PastEnd { index: i }, Ty(blaze_ty::Ty::ERROR.node().bits())),
        }
    }

    /// The closure of another body this context made whose owner's arguments, with the
    /// variables bound since resolved, are `l`'s: the solver rebuilds `{closure#0 of gen<?T>}`
    /// as `{closure#0 of gen<u8>}` once `?T = u8`, a new term of the same closure. Each
    /// argument is compared after a shallow probe: a variable nested inside an argument
    /// (`gen::<Vec<?T>>`) is not seen through, and its closure stays unviewable (a panic).
    fn foreign_resolved(self, d: blaze_ty::Def, l: blaze_ty::List) -> Option<&'x ForeignClosure> {
        let tys = &self.terms.cx.tys;
        let n = tys.list_len(l);
        let same = |a: blaze_ty::Arg, b: blaze_ty::Arg| match (a.ty(), b.ty()) {
            (Some(x), Some(y)) => self.terms.cx.probe(x) == self.terms.cx.probe(y),
            _ => a == b,
        };
        self.rows.foreign.iter().find(|f| {
            let TyKind::Closure(d2, l2) = tys.ty_kind(f.made) else { return false };
            d2 == d
                && tys.list_len(l2) == n
                && (0..n.saturating_sub(3)).all(|i| match (tys.list_get(l, i), tys.list_get(l2, i)) {
                    (Some(a), Some(b)) => same(a, b),
                    _ => false,
                })
        })
    }

    /// One level of `t`. A handle it cannot view is noted (`unreadable`) and viewed as the error
    /// type: the body's check then fails with it by name (rule 20), never the process.
    fn view(self, t: Ty) -> TyView {
        use crate::error::Unreadable as U;
        let tys = &self.terms.cx.tys;
        let ty = |x: blaze_ty::Ty| Ty(x.node().bits());
        let list = |l: blaze_ty::List| TyList(l.node().bits());
        let key = |d: blaze_ty::Def| ItemKey(def_of(tys.item_key(d)));
        let bad = |what: U| self.unreadable(t.0, what, TyView::Error);
        if let Some(c) = self.node(t.0).as_const() {
            return match tys.const_kind(c) {
                ConstKind::Value(vt, bits) => TyView::ConstValue { ty: ty(vt), bits },
                ConstKind::Param(i) => TyView::ConstParam(i),
                ConstKind::Var(v) => TyView::ConstInfer(Var { kind: VarKind::Const, index: v }),
                ConstKind::Error => TyView::Error,
                _ => bad(U::NoView),
            };
        }
        match tys.ty_kind(self.bty(t)) {
            TyKind::Bool => TyView::Bool,
            TyKind::Char => TyView::Char,
            TyKind::Int(i) => TyView::Int(typeck_int(i)),
            TyKind::Uint(u) => TyView::Uint(typeck_uint(u)),
            TyKind::Float(f) => TyView::Float(typeck_float(f)),
            TyKind::Str => TyView::Str,
            TyKind::Never => TyView::Never,
            TyKind::Tuple(l) => TyView::Tuple(list(l)),
            TyKind::Record(s, l) => TyView::Record(RecordSchema(s), list(l)),
            TyKind::Union(l) => TyView::Union(list(l)),
            TyKind::Ref(_, p, m) => TyView::Ref(mutbl(m), ty(p)),
            TyKind::RawPtr(p, m) => TyView::Ptr(mutbl(m), ty(p)),
            TyKind::Array(e, n) => TyView::Array(ty(e), Ty(n.node().bits())),
            TyKind::Slice(e) => TyView::Slice(ty(e)),
            TyKind::Pat(b, l) => TyView::Pat(ty(b), list(l)),
            TyKind::Adt(d, l) => TyView::Adt(key(d), list(l)),
            TyKind::FnDef(d, l) => TyView::FnDef(key(d), list(l)),
            TyKind::FnPtr(l, h) => TyView::FnPtr(list(l), terms::s_header(h)),
            TyKind::Closure(d, l) => {
                let k = def_of(tys.item_key(d));
                // Typeck's view keeps the signature untupled, as the closure was made; the
                // solver's tupled form (`mk`) shares its variables.
                if tys.list_len(l) < 3 {
                    return bad(U::ClosureArgs);
                }
                if let Some(r) = self.rows.closures.get(&k) {
                    return TyView::Closure(r.expr, r.sig);
                }
                let made = self.bty(t);
                let exact = self.rows.foreign.iter().find(|f| f.made == made);
                let Some(f) = exact.or_else(|| self.foreign_resolved(d, l)) else {
                    return bad(U::ClosureUnmade);
                };
                let Some(env) = tys.list_get(l, tys.list_len(l) - 1).and_then(blaze_ty::Arg::ty) else {
                    return bad(U::ClosureArgs);
                };
                let upvars = (tys.ty_kind(env) != TyKind::Foreign(d)).then(|| ty(env));
                TyView::ClosureOf { owner: f.owner, ordinal: f.ordinal, args: f.args, sig: f.sig, kind: f.kind, upvars }
            }
            TyKind::Coroutine(d, l) => {
                let k = def_of(tys.item_key(d));
                let made = self.bty(t);
                if let Some(f) = self.rows.foreign_coroutines.iter().find(|f| f.made == made) {
                    return TyView::CoroutineOf { owner: f.owner, ordinal: f.ordinal, args: f.args, output: f.output, interior: f.interior };
                }
                let Some(&e) = self.rows.futures.get(&k) else { return bad(U::FutureUnmade) };
                let at = |i: u32| tys.list_get(l, i).and_then(blaze_ty::Arg::ty).map(ty);
                match (tys.list_len(l), at(0), at(1)) {
                    (2, Some(o), Some(i)) => TyView::Future(e, o, i),
                    _ => bad(U::FutureArgs),
                }
            }
            TyKind::Param(i) => TyView::Param(i),
            TyKind::Alias { kind: AliasKind::Opaque, def, args, .. } => TyView::Opaque(key(def), list(args)),
            TyKind::Alias { def, args, .. } => {
                let d = def_of(tys.item_key(def));
                match self.facts.names.get(&d) {
                    Some(&(trait_, name)) => TyView::Alias { trait_, name, args: list(args) },
                    None => bad(U::AliasUnnamed),
                }
            }
            TyKind::TyVar(v) => TyView::Infer(Var { kind: VarKind::Ty, index: v }),
            TyKind::IntVar(v) => TyView::Infer(Var { kind: VarKind::Int, index: v }),
            TyKind::FloatVar(v) => TyView::Infer(Var { kind: VarKind::Float, index: v }),
            TyKind::Error => TyView::Error,
            TyKind::Dynamic(preds, _) => TyView::Dynamic(list(preds)),
            TyKind::Foreign(d) => TyView::Foreign(key(d)),
            TyKind::NotAType => {
                let Some(p) = tys.as_pred(self.bty(t)) else { return bad(U::NeitherTypeNorPred) };
                match tys.pred_kind(p) {
                    blaze_ty::PredKind::ExTrait(d, l) => TyView::ExTrait(key(d), list(l)),
                    blaze_ty::PredKind::ExAuto(d) => TyView::ExAuto(key(d)),
                    blaze_ty::PredKind::ExProjection(d, l) => {
                        let Some(&(trait_, name)) = self.facts.names.get(&def_of(tys.item_key(d))) else {
                            return bad(U::AliasUnnamed);
                        };
                        TyView::ExProj { trait_, name, args_term: list(l) }
                    }
                    _ => bad(U::NoView),
                }
            }
            _ => bad(U::NoView),
        }
    }
}

impl<'x, 'a> Exporter<'x, 'a> {
    fn seen(&self) -> Seen<'_, 'a> {
        Seen { terms: &*self.terms, facts: self.facts, rows: &*self.rows }
    }

    /// The allocator the check's caller handed in (D102).
    pub(crate) fn alloc(&self) -> Alloc<'a> {
        self.alloc
    }

    /// One level of `t` (`TypeCtx::view`, the same body: [`Seen::view`]).
    pub(crate) fn view(&self, t: Ty) -> TyView {
        self.seen().view(t)
    }

    pub(crate) fn list_len(&self, l: TyList) -> u32 {
        self.seen().list_len(l)
    }

    pub(crate) fn list_get(&self, l: TyList, i: u32) -> Ty {
        self.seen().list_get(l, i)
    }

    /// `t` with every bound variable replaced (`TypeCtx::resolve`, the same body).
    pub(crate) fn resolve(&mut self, t: Ty) -> Result<Ty, CheckError> {
        let n = self.seen().node(t.0);
        Ok(Ty(self.terms.cx.resolve(n).bits()))
    }

    /// Files `t` when it is a closure or async block term the SOLVER made: an opaque's hidden
    /// type holds it (facts.rs `body_terms`), and revealing the opaque (blaze-solve alias.rs
    /// `normalize_opaque_type`, a ground resolution's post-analysis mode) instantiates its term
    /// at the opaque's arguments without passing `mk`, so `view` had no row for it ("a closure
    /// this context did not make", census g083729). Its row is read off the term by its def:
    /// owner arguments, then `[kind, rust-call sig, upvars]` for a closure (`closure_term`),
    /// `[output, interior, owner arguments..]` for an async block (`coroutine_term`). Any other term is left as it
    /// is. A walk of a resolved type it did not build calls this before `view` (instance.rs
    /// `export`); a closure or async block term it can name by nothing is refused by its handle.
    pub(crate) fn adopt(&mut self, t: Ty) -> Result<(), CheckError> {
        use crate::error::Malformed;
        let unseen = || CheckError::Malformed(Malformed::UnseenBodyType { node: t.0 });
        // A const (an array's length, a const argument) is no body type.
        let Some(made) = self.seen().node(t.0).as_ty() else { return Ok(()) };
        match self.terms.cx.tys.ty_kind(made) {
            TyKind::Closure(d, l) => {
                let k = def_of(self.terms.cx.tys.item_key(d));
                if self.rows.closures.contains_key(&k) || self.rows.foreign.iter().any(|f| f.made == made) || self.seen().foreign_resolved(d, l).is_some() {
                    return Ok(());
                }
                let Some(&facts::HiddenBody { owner, ordinal, args: n }) = self.facts.hidden_bodies.get(&k) else {
                    return Err(unseen());
                };
                let tys = &mut self.terms.cx.tys;
                if tys.list_len(l) != n + 3 {
                    return Err(unseen());
                }
                let mut owner_args: Vec<blaze_ty::Arg, Alloc<'a>> = Vec::with_capacity_in(n as usize, self.alloc);
                for i in 0..n {
                    owner_args.push(tys.list_get(l, i).ok_or_else(unseen)?);
                }
                let args = TyList(tys.mk_list(&owner_args).node().bits());
                use blaze_solve::term::{CLOSURE_FN, CLOSURE_FN_MUT, CLOSURE_FN_ONCE};
                let kind = match tys.list_get(l, n).and_then(blaze_ty::Arg::konst).map(|c| tys.const_kind(c)) {
                    Some(ConstKind::Value(_, v)) if v == u128::from(CLOSURE_FN) => crate::ty::ClosureKind::Fn,
                    Some(ConstKind::Value(_, v)) if v == u128::from(CLOSURE_FN_MUT) => crate::ty::ClosureKind::FnMut,
                    Some(ConstKind::Value(_, v)) if v == u128::from(CLOSURE_FN_ONCE) => crate::ty::ClosureKind::FnOnce,
                    _ => return Err(unseen()),
                };
                // The signature as typeck makes it, `fn(inputs.., output)`, from the solver's
                // rust-call `fn((inputs..,)) -> output` (`rust_call_sig` inverted).
                let Some(TyKind::FnPtr(io, h)) = tys.list_get(l, n + 1).and_then(blaze_ty::Arg::ty).map(|s| tys.ty_kind(s)) else {
                    return Err(unseen());
                };
                let (Some(tupled), Some(out)) = (tys.list_get(io, 0).and_then(blaze_ty::Arg::ty), tys.list_get(io, 1).and_then(blaze_ty::Arg::ty)) else {
                    return Err(unseen());
                };
                let TyKind::Tuple(ins) = tys.ty_kind(tupled) else { return Err(unseen()) };
                let m = tys.list_len(ins);
                let mut flat: Vec<blaze_ty::Arg, Alloc<'a>> = Vec::with_capacity_in(m as usize + 1, self.alloc);
                for i in 0..m {
                    flat.push(tys.list_get(ins, i).ok_or_else(unseen)?);
                }
                flat.push(out.arg());
                let io = tys.mk_list(&flat);
                let sig = Ty(tys.fn_ptr(io, h).node().bits());
                self.rows.foreign.push(ForeignClosure { made, owner, ordinal, args, sig, kind });
            }
            TyKind::Coroutine(d, l) => {
                let k = def_of(self.terms.cx.tys.item_key(d));
                if self.rows.futures.contains_key(&k) || self.rows.foreign_coroutines.iter().any(|f| f.made == made) {
                    return Ok(());
                }
                // A newly resolved instance of a term we imported has the same
                // definition even when one of its arguments has since been bound.
                let seen = self.seen();
                let identity = self.facts.hidden_bodies.get(&k).map(|f| (f.owner, f.ordinal, f.args))
                    .or_else(|| seen.rows.foreign_coroutines.iter().find_map(|f| {
                        matches!(seen.terms.cx.tys.ty_kind(f.made), TyKind::Coroutine(original, _) if original == d)
                            .then(|| (f.owner, f.ordinal, seen.list_len(f.args)))
                    }));
                let (owner, ordinal, n) = identity.ok_or_else(unseen)?;
                let tys = &mut self.terms.cx.tys;
                if tys.list_len(l).checked_sub(2) != Some(n) {
                    let given = tys.list_len(l).checked_sub(2).ok_or_else(unseen)?;
                    return Err(CheckError::Malformed(Malformed::CoroutineSubstitution { owner, expected: n, given: given as usize }));
                }
                let (Some(o), Some(i)) = (tys.list_get(l, 0).and_then(blaze_ty::Arg::ty), tys.list_get(l, 1).and_then(blaze_ty::Arg::ty)) else {
                    return Err(unseen());
                };
                let mut owner_args = Vec::with_capacity_in(n as usize, self.alloc);
                for index in 2..tys.list_len(l) {
                    owner_args.push(tys.list_get(l, index).ok_or_else(unseen)?);
                }
                let args = TyList(tys.mk_list(&owner_args).node().bits());
                let (output, interior) = (Ty(o.node().bits()), Ty(i.node().bits()));
                self.rows.foreign_coroutines.push(ForeignCoroutine { made, owner, ordinal, args, output, interior });
            }
            _ => {}
        }
        Ok(())
    }
}

impl<'a> InferCtx<'a> {
    /// A closure's signature (a `FnPtr`, inputs then output) in the solver's "rust-call" form:
    /// its one input the tuple of the closure's inputs (blaze-solve builtin.rs `callable`).
    fn rust_call_sig(&mut self, sig: Ty) -> Result<blaze_ty::Ty, CheckError> {
        let TyView::FnPtr(io, _) = self.view(sig) else {
            return Err(CheckError::Malformed(crate::error::Malformed::Unreadable { node: sig.0, what: crate::error::Unreadable::ClosureSig }));
        };
        let n = self.list_len(io);
        let mut ins: Vec<blaze_ty::Ty, Alloc<'a>> = Vec::with_capacity_in(n.saturating_sub(1) as usize, self.alloc);
        for i in 0..n.saturating_sub(1) {
            let t = self.list_get(io, i);
            ins.push(self.bty(t));
        }
        let tupled = self.terms.cx.tys.tuple_of(&ins);
        let out = self.bty(self.list_get(io, n.saturating_sub(1)));
        let io = self.terms.cx.tys.mk_list(&[tupled.arg(), out.arg()]);
        Ok(self.terms.cx.tys.fn_ptr(io, FnHeader::RUST))
    }

    fn blist(&self, l: TyList) -> blaze_ty::List {
        self.seen().blist(l)
    }

    fn slist(&self, l: TyList) -> SList {
        self.terms.slist(self.blist(l))
    }

    /// The solver's predicate of a typeck goal.
    fn pred(&mut self, g: Goal) -> Pred {
        match g {
            Goal::Trait { trait_, args } => {
                Pred::Trait { trait_: SDef(trait_.0), args: self.slist(args), polarity: Polarity::Positive }
            }
            Goal::Projection { trait_, args, name, term } => {
                let alias = self.alias(trait_, name, args);
                Pred::Projection { alias: STy(alias.node().bits()), term: STy(self.bty(term).node().bits()) }
            }
            Goal::WellFormed(t) => Pred::WellFormed(STy(self.bty(t).node().bits())),
            Goal::ConstHolds { cond, args } => Pred::ConstHolds { cond: SDef(cond.0), args: self.slist(args) },
            Goal::ForFields { container, trait_, args } => Pred::ForFields {
                container: STy(self.bty(container).node().bits()),
                trait_: SDef(trait_.0),
                args_after_self: self.slist(args),
            },
        }
    }

    fn alias(&mut self, trait_: ItemKey, name: Name, args: TyList) -> blaze_ty::Ty {
        let d = assoc_def(trait_, name);
        self.facts.names.insert(d, (trait_, name));
        let def = self.terms.bdef(SDef(d));
        let args = self.blist(args);
        self.terms.cx.tys.alias(AliasKind::Projection, def, args)
    }

    /// Answers the solver's question `q` through `reads`. A `where const` condition's arguments
    /// are terms of this context: exported as a ground table, which names the condition's
    /// record; every other question is the fact cache's.
    fn fill<W: World>(&mut self, q: blaze_solve::facts::Question, reads: &mut Reads<'_, '_, W>) -> Result<(), Stop> {
        if let blaze_solve::facts::Question::Specializes(a, b) = q {
            return self.fill_specializes(a, b, reads);
        }
        let blaze_solve::facts::Question::ConstHolds(cond, list) = q else { return self.facts.fill(q, reads) };
        fill_const_holds(&mut self.terms, &mut self.facts, &mut self.rows, reads, self.alloc, cond, list)
    }

    /// Answers the solver's `Specializes(a, b)` (the free [`fill_specializes`]).
    fn fill_specializes<W: World>(&mut self, a: SDef, b: SDef, reads: &mut Reads<'_, '_, W>) -> Result<(), Stop> {
        fill_specializes(&mut self.facts, reads, self.alloc, self.spec_depth, &mut self.need_rounds, a, b)
    }

    /// Runs `solve` with this context's pull source, then: the solver's time without the
    /// fills counted, the solver's own reads dropped, the specializing contexts' rounds added,
    /// and a failed fill's reason returned before the solver's stop.
    fn pulled<W: World, R>(
        &mut self,
        reads: &mut Reads<'_, '_, W>,
        solve: impl FnOnce(&mut Solver<'a>, &mut Terms<'a>, &mut Pull<'_, 'a, '_, '_, W>, &mut Vec<blaze_solve::Read, Alloc<'a>>) -> Result<R, blaze_solve::Stop>,
    ) -> Result<Result<R, blaze_solve::Stop>, Stop> {
        let at = reads.stamp();
        let mut src = Pull::new(&mut self.facts, &mut self.rows, reads, self.alloc, self.spec_depth);
        #[cfg(test)]
        {
            src.stop = self.stop_for_facts;
        }
        let r = solve(&mut self.solver, &mut self.terms, &mut src, &mut self.solver_reads);
        let (err, rounds, fill_ns) = (src.err.take(), src.need_rounds, src.fill_ns);
        reads.count_solve(at.saturating_add(fill_ns));
        self.solver_reads.clear();
        self.need_rounds += rounds;
        match err {
            Some(e) => Err(e),
            None => Ok(r),
        }
    }

    /// The reason an unmet `self: trait_<..>` bound failed, when an impl of `trait_` has a
    /// `where const` condition that was false, failed, or is over a generic parameter here.
    /// The first such condition in the order the impls were read (deterministic, rule 25).
    fn because(&mut self, trait_: ItemKey, args: TyList) -> Option<crate::diag::Because> {
        let generic = self.has_param(args);
        let mut i = 0;
        while let Some((cond, t)) = self.facts.const_condition(i) {
            i += 1;
            if t == trait_.0
                && let Some(b) = self.facts.reason(cond, generic)
            {
                return Some(b);
            }
        }
        None
    }

    /// The reason a `where const` obligation of an item's own clauses failed.
    fn because_of(&mut self, cond: ItemKey, args: TyList) -> Option<crate::diag::Because> {
        let generic = self.has_param(args);
        self.facts.reason(SDef(cond.0), generic)
    }

    /// Whether a type of `args`, resolved, holds a generic parameter.
    fn has_param(&mut self, args: TyList) -> bool {
        let l = self.slist(args);
        let mut scratch = blaze_solve::fold::Scratch::new_in(self.alloc);
        for i in 0..l.len {
            let t = self.terms.list_get(l, i);
            if let Ok(t) = blaze_solve::fold::resolve(&mut self.terms, &mut scratch, t)
                && self.terms.flags(t).has(blaze_solve::term::Flags::HAS_PARAM)
            {
                return true;
            }
        }
        false
    }

    fn set_state(&mut self, i: usize, s: State) {
        if self.state[i] != s {
            self.undo.push((i as u32, self.state[i]));
            self.state[i] = s;
        }
    }

    /// The param env, interned (filling its facts first).
    fn env<W: World>(&mut self, reads: &mut Reads<'_, '_, W>) -> Result<EnvId, Stop> {
        if let Some(e) = self.env {
            return Ok(e);
        }
        for _ in 0..MAX_FILLS {
            let clauses = core::mem::replace(&mut self.env_clauses, Vec::new_in(self.alloc));
            let r = self.pulled(reads, |solver, terms, src, sr| solver.env_from(terms, src, &clauses, sr));
            self.env_clauses = clauses;
            match r? {
                Ok(e) => {
                    self.env = Some(e);
                    return Ok(e);
                }
                Err(blaze_solve::Stop::Need(q)) => {
                    self.fill(q, reads)?;
                    self.need_rounds += 1;
                }
                Err(e) => solve_err(e)?,
            }
        }
        Err(CheckError::Limit(crate::error::Limit::Solver).into())
    }

    /// Evaluates one goal now, filling facts until it answers.
    fn evaluate<W: World>(&mut self, pred: Pred, reads: &mut Reads<'_, '_, W>) -> Result<Answer, Stop> {
        let env = self.env(reads)?;
        let mode = self.mode;
        for _ in 0..MAX_FILLS {
            let g = SGoal { env, pred, bound: 0 };
            let r = self.pulled(reads, |solver, terms, src, sr| solver.evaluate_from(terms, src, mode, g, sr));
            match r? {
                Ok(a) => return Ok(a),
                Err(blaze_solve::Stop::Need(q)) => {
                    self.fill(q, reads)?;
                    self.need_rounds += 1;
                }
                Err(e) => solve_err(e)?,
            }
        }
        Err(CheckError::Limit(crate::error::Limit::Solver).into())
    }

    /// [`TypeCtx::why_chain`]: `Solver::why_no` on `goal`, filling facts until it answers, and
    /// each step's goal decoded (`Solver::why_goal`) in the same probe; every binding and
    /// variable the walk made is rolled back after its terms are read.
    fn why_walk<W: World>(
        &mut self,
        goal: Goal,
        steps: &mut Vec<crate::diag::PendingWhyStep, Alloc<'_>>,
        reads: &mut Reads<'_, '_, W>,
    ) -> Result<Option<crate::diag::WhyLeaf>, Stop> {
        let env = self.env(reads)?;
        let pred = self.pred(goal);
        let s = self.snapshot();
        let r = self.why_walk_in(env, pred, steps, reads);
        self.rollback_to(s);
        r
    }

    /// [`InferCtx::why_walk`] inside its probe: a step's goal naming a variable made since the
    /// probe began (an erased region or type the canonical input gave back fresh) is not kept,
    /// as it cannot outlive the rollback (the same guard as `list_has_infer_since`'s callers).
    fn why_walk_in<W: World>(
        &mut self,
        env: EnvId,
        pred: Pred,
        steps: &mut Vec<crate::diag::PendingWhyStep, Alloc<'_>>,
        reads: &mut Reads<'_, '_, W>,
    ) -> Result<Option<crate::diag::WhyLeaf>, Stop> {
        use blaze_solve::why_no::Leaf;
        use crate::diag::WhyLeaf;
        let mark = self.var_mark();
        let alloc = self.alloc;
        let mode = self.mode;
        for _ in 0..MAX_FILLS {
            let g = SGoal { env, pred, bound: 0 };
            let r = self.pulled(reads, |solver, terms, src, sr| {
                let why = solver.why_no(&mut *terms, src, mode, g, sr)?;
                let mut rows: Vec<(blaze_solve::why_no::Reached, Option<(SDef, SList)>), Alloc<'a>> = Vec::with_capacity_in(why.steps.len(), alloc);
                for st in why.steps.iter() {
                    let goal = match solver.why_goal(&mut *terms, st.input)? {
                        Some((_, SGoal { pred: Pred::Trait { trait_, args, polarity: Polarity::Positive }, .. })) => Some((trait_, args)),
                        _ => None,
                    };
                    rows.push((st.reached, goal));
                }
                Ok((why.leaf, rows))
            });
            match r? {
                Ok((leaf, rows)) => {
                    let leaf = match leaf {
                        Leaf::NotNo => return Ok(None),
                        Leaf::NoImpl => WhyLeaf::NoImpl,
                        Leaf::NegativeImpl { impl_ } => WhyLeaf::NegativeImpl { impl_: ItemKey(impl_.0) },
                        Leaf::Refused { first, count, .. } => WhyLeaf::Refused {
                            first_impl: match first.source {
                                blaze_solve::assembly::Source::Impl(d) => Some(ItemKey(d.0)),
                                _ => None,
                            },
                            count,
                        },
                        Leaf::BuiltinNo { .. } => WhyLeaf::BuiltinNo,
                        Leaf::Cycle => WhyLeaf::Cycle,
                        Leaf::NotKept => WhyLeaf::NotKept,
                    };
                    let whole = self.why_rows_into(&rows, Some(mark), steps)?;
                    return Ok(Some(if whole { leaf } else { WhyLeaf::NotKept }));
                }
                Err(blaze_solve::Stop::Need(q)) => {
                    self.fill(q, reads)?;
                    self.need_rounds += 1;
                }
                Err(e) => solve_err(e)?,
            }
        }
        Err(CheckError::Limit(crate::error::Limit::Solver).into())
    }

    /// The solver's chain `rows` as the record's steps, appended to `steps`. A step's goal is
    /// kept only where its arguments name no variable made since `since` (`None`: no variable
    /// at all, for an undecided goal's chain, whose open variables are the body's own and are
    /// never written to a record). `false` when a step names a closure or async block this
    /// context did not make (`WhyDefUnmade`), or whose kept goal holds a closure or async block
    /// term no row names (`why_adopt`): the chain stops before that step, and its leaf is
    /// `NotKept`; a chain that cannot be built never turns the finding into a refusal.
    fn why_rows_into(
        &mut self,
        rows: &[(blaze_solve::why_no::Reached, Option<(SDef, SList)>)],
        since: Option<crate::ty::VarMark>,
        steps: &mut Vec<crate::diag::PendingWhyStep, Alloc<'_>>,
    ) -> Result<bool, Stop> {
        for &(reached, goal) in rows {
            let reached = match self.why_reached(reached) {
                Ok(r) => r,
                Err(CheckError::Malformed(crate::error::Malformed::WhyDefUnmade { .. })) => return Ok(false),
                Err(e) => return Err(e.into()),
            };
            let goal = match goal {
                Some((trait_, args)) => {
                    let args = TyList(self.terms.blist(args).map_err(CheckError::Solve)?.node().bits());
                    let open = match since {
                        Some(mark) => self.list_has_infer_since(args, mark),
                        None => self.list_has_infer(args),
                    };
                    if open {
                        None
                    } else {
                        // The step's goal is written to the record through `view`: a closure or
                        // async block in it that the solver made revealing an opaque (another
                        // fn's future, reached by its hidden type) is filed first, as an
                        // instance export files it (instance.rs `export_as`). One no row can
                        // name ends the chain here; noting it unreadable would refuse the
                        // whole body (`Malformed::Unreadable { what: FutureUnmade }`).
                        if !self.why_adopt(args) {
                            return Ok(false);
                        }
                        Some((ItemKey(trait_.0), args))
                    }
                }
                None => None,
            };
            steps.push(crate::diag::PendingWhyStep { goal, reached });
        }
        Ok(true)
    }

    /// Files every closure and async block term under `args` ([`Exporter::adopt`]), so each
    /// reads through `view`; `false` when one is named by nothing (`UnseenBodyType`), or the
    /// walk passes its limit.
    fn why_adopt(&mut self, args: TyList) -> bool {
        let l = self.slist(args);
        let mut work = Vec::new_in(self.alloc);
        let mut made: Vec<STy, Alloc<'a>> = Vec::new_in(self.alloc);
        for i in 0..l.len {
            let t = self.terms.list_get(l, i);
            let walked = blaze_solve::fold::visit(&self.terms, &mut work, t, |t, k| {
                if matches!(k, blaze_solve::term::TyKind::Closure(..) | blaze_solve::term::TyKind::Coroutine(..)) {
                    made.push(t);
                }
                true
            });
            if walked.is_err() {
                return false;
            }
        }
        // Outermost first: an async block's interior holds the futures it awaits.
        made.iter().all(|&t| self.adopt(Ty(t.0)).is_ok())
    }

    /// [`TypeCtx::hidden_unknown`]: one ordinary evaluation of `goal` in a probe (its answer
    /// from the global cache when it was asked before), and, when undecided, the opaque its
    /// read list met with no hidden type (blaze-solve `Ask::first_hidden_unknown`).
    fn hidden_unknown_in<W: World>(
        &mut self,
        goal: Goal,
        skip: &dyn Fn(ItemKey) -> bool,
        reads: &mut Reads<'_, '_, W>,
    ) -> Result<Option<ItemKey>, Stop> {
        let env = self.env(reads)?;
        let pred = self.pred(goal);
        let s = self.snapshot();
        let mut r = Err(CheckError::Limit(crate::error::Limit::Solver).into());
        let mode = self.mode;
        for _ in 0..MAX_FILLS {
            let g = SGoal { env, pred, bound: 0 };
            let got = self.pulled(reads, |solver, terms, src, sr| {
                let answer = solver.evaluate_from(&mut *terms, &mut *src, mode, g, &mut *sr)?;
                if !matches!(answer, Answer::Maybe(_)) {
                    return Ok(None);
                }
                let facts = blaze_solve::Source::facts(&*src);
                Ok(blaze_solve::facts::Ask::new(facts).first_hidden_unknown(&sr[..], |d| skip(ItemKey(d.0))))
            });
            match got {
                Err(e) => {
                    r = Err(e);
                    break;
                }
                Ok(Ok(found)) => {
                    r = Ok(found.map(|d| ItemKey(d.0)));
                    break;
                }
                Ok(Err(blaze_solve::Stop::Need(q))) => {
                    if let Err(e) = self.fill(q, reads) {
                        r = Err(e);
                        break;
                    }
                    self.need_rounds += 1;
                }
                Ok(Err(e)) => {
                    r = solve_err(e).map(|()| None).map_err(Stop::from);
                    break;
                }
            }
        }
        self.rollback_to(s);
        r
    }

    /// [`TypeCtx::why_maybe_chain`]: `Solver::why_maybe` on `goal` in a probe, as
    /// [`InferCtx::why_walk`] is `why_no`'s.
    fn why_maybe_walk<W: World>(
        &mut self,
        goal: Goal,
        steps: &mut Vec<crate::diag::PendingWhyStep, Alloc<'_>>,
        reads: &mut Reads<'_, '_, W>,
    ) -> Result<Option<crate::diag::WhyLeaf>, Stop> {
        let env = self.env(reads)?;
        let pred = self.pred(goal);
        let s = self.snapshot();
        let r = self.why_maybe_in(env, pred, steps, reads);
        self.rollback_to(s);
        r
    }

    fn why_maybe_in<W: World>(
        &mut self,
        env: EnvId,
        pred: Pred,
        steps: &mut Vec<crate::diag::PendingWhyStep, Alloc<'_>>,
        reads: &mut Reads<'_, '_, W>,
    ) -> Result<Option<crate::diag::WhyLeaf>, Stop> {
        use blaze_solve::why_maybe::{Leaf, LimitHit, Unopened};
        use crate::diag::{WhyLeaf, WhyLimit};
        let alloc = self.alloc;
        let mode = self.mode;
        for _ in 0..MAX_FILLS {
            let g = SGoal { env, pred, bound: 0 };
            let r = self.pulled(reads, |solver, terms, src, sr| {
                let why = solver.why_maybe(&mut *terms, src, mode, g, sr)?;
                let mut rows: Vec<(blaze_solve::why_no::Reached, Option<(SDef, SList)>), Alloc<'a>> = Vec::with_capacity_in(why.steps.len(), alloc);
                for st in why.steps.iter() {
                    let goal = match solver.why_goal(&mut *terms, st.input)? {
                        Some((_, SGoal { pred: Pred::Trait { trait_, args, polarity: Polarity::Positive }, .. })) => Some((trait_, args)),
                        _ => None,
                    };
                    rows.push((st.reached, goal));
                }
                Ok((why.leaf, rows))
            });
            match r? {
                Ok((leaf, rows)) => {
                    let leaf = match leaf {
                        Leaf::NotMaybe => return Ok(None),
                        Leaf::Unresolved { term, .. } => Some(WhyLeaf::Unresolved { term }),
                        Leaf::Candidates { first, second, count } => Some(WhyLeaf::Ambiguous { first: why_cand(first), second: why_cand(second), count }),
                        Leaf::Limit(hit) => Some(WhyLeaf::Limit {
                            hit: match hit {
                                LimitHit::Depth { depth, limit } => WhyLimit::Depth { depth, limit },
                                LimitHit::Fixpoint { reruns } => WhyLimit::Fixpoint { reruns },
                                LimitHit::Rounds { rounds } => WhyLimit::Rounds { rounds },
                            },
                        }),
                        Leaf::Unopened(Unopened::Hidden { opaque }) => Some(WhyLeaf::HiddenUnknown { opaque: ItemKey(opaque.0) }),
                        Leaf::Unopened(Unopened::Interior { coroutine }) => self.why_coroutine(coroutine).ok().map(|coroutine| WhyLeaf::InteriorUnknown { coroutine }),
                        Leaf::Unopened(Unopened::Captures { closure }) => self.why_closure(closure).ok().map(|closure| WhyLeaf::CapturesUnknown { closure }),
                        Leaf::Normalize { alias, .. } => Some(WhyLeaf::Unnormalized { alias: ItemKey(alias.0) }),
                        Leaf::Rule { cand } => Some(WhyLeaf::RuleUndecided { cand: why_cand(cand) }),
                        Leaf::Cycle => Some(WhyLeaf::Cycle),
                        Leaf::NotKept => Some(WhyLeaf::NotKept),
                    };
                    // A closure or async block this context did not make: the chain says so
                    // (`why_rows_into`), never a refusal.
                    let whole = self.why_rows_into(&rows, None, steps)?;
                    return Ok(Some(match leaf {
                        Some(leaf) if whole => leaf,
                        _ => WhyLeaf::NotKept,
                    }));
                }
                Err(blaze_solve::Stop::Need(q)) => {
                    self.fill(q, reads)?;
                    self.need_rounds += 1;
                }
                Err(e) => solve_err(e)?,
            }
        }
        Err(CheckError::Limit(crate::error::Limit::Solver).into())
    }

    /// A solver step's [`blaze_solve::why_no::Reached`] in the record's terms: an item's def is
    /// its key; a closure's or async block's is this body's expression, or another body's
    /// ordinal, as this context made it.
    fn why_reached(&self, r: blaze_solve::why_no::Reached) -> Result<crate::diag::WhyReached, CheckError> {
        use blaze_solve::why_no::Reached as R;
        use crate::diag::WhyReached as W;
        let item = |d: SDef| ItemKey(d.0);
        Ok(match r {
            R::Root => W::Root,
            R::ImplClause { impl_, clause } => W::ImplClause { impl_: item(impl_), clause },
            R::Field { adt, field } => W::Field { adt: item(adt), field },
            R::AdtArg { adt, arg } => W::AdtArg { adt: item(adt), arg },
            R::RefTarget => W::RefTarget,
            R::PtrTarget => W::PtrTarget,
            R::Element => W::Element,
            R::TupleElement { index } => W::TupleElement { index },
            R::Capture { closure, capture } => W::Capture { closure: self.why_closure(closure)?, capture },
            R::CoroutineSlot { coroutine, slot } => W::CoroutineSlot { coroutine: self.why_coroutine(coroutine)?, slot },
            R::Captures { closure } => W::Captures { closure: self.why_closure(closure)? },
            R::Interior { coroutine } => W::Interior { coroutine: self.why_coroutine(coroutine)? },
            R::Phantom => W::Phantom,
            R::Hidden { opaque } => W::Hidden { opaque: item(opaque) },
            R::Other { nth, .. } => W::Other { nth },
        })
    }

    /// The closure the solver's def `d` is: this body's (`TyView::Closure`) or another's
    /// (`TyView::ClosureOf`, src/coroutine_id.rs `closure_of_def`).
    fn why_closure(&self, d: SDef) -> Result<crate::diag::WhyDef, CheckError> {
        if let Some(r) = self.rows.closures.get(&d.0) {
            return Ok(crate::diag::WhyDef::Expr(r.expr));
        }
        match self.rows.foreign.iter().find(|f| crate::coroutine_id::closure_of_def(f.owner, f.ordinal) == d.0) {
            Some(f) => Ok(crate::diag::WhyDef::Of { owner: f.owner, ordinal: f.ordinal }),
            None => Err(CheckError::Malformed(crate::error::Malformed::WhyDefUnmade { def: d.0 })),
        }
    }

    /// The async block's future the solver's def `d` is: this body's (`TyView::Future`) or
    /// another's (`TyView::CoroutineOf`, src/coroutine_id.rs `coroutine_def`).
    fn why_coroutine(&self, d: SDef) -> Result<crate::diag::WhyDef, CheckError> {
        if let Some(&e) = self.rows.futures.get(&d.0) {
            return Ok(crate::diag::WhyDef::Expr(e));
        }
        match self.rows.foreign_coroutines.iter().find(|f| crate::coroutine_id::coroutine_def(f.owner, f.ordinal) == d.0) {
            Some(f) => Ok(crate::diag::WhyDef::Of { owner: f.owner, ordinal: f.ordinal }),
            None => Err(CheckError::Malformed(crate::error::Malformed::WhyDefUnmade { def: d.0 })),
        }
    }

    fn select<W: World>(&mut self, reads: &mut Reads<'_, '_, W>) -> Result<(), Stop> {
        let env = self.env(reads)?;
        let mode = self.mode;
        for _ in 0..MAX_FILLS {
            let mut f: Fulfill<'a, Wake> = Fulfill::new_in(self.alloc);
            let mut ids: Vec<usize, Alloc<'a>> = Vec::new_in(self.alloc);
            for i in 0..self.obs.len() {
                if self.state[i] == State::Pending {
                    f.register(SGoal { env, pred: self.preds[i], bound: 0 }).map_err(CheckError::Solve)?;
                    ids.push(i);
                }
            }
            if ids.is_empty() {
                return Ok(());
            }
            // A fact a goal lacks is pulled where it is met (`Pull`): the obligations are
            // registered once, and again only under the test strategy's stop.
            let r = self.pulled(reads, |solver, terms, src, sr| f.select_from(solver, terms, src, mode, sr))?;
            for (j, &i) in ids.iter().enumerate() {
                let s = f.state[j];
                if s == State::Fails && self.state[i] == State::Pending {
                    let kind = match self.obs[i].goal {
                        Goal::Projection { .. } => FulfillErrorKind::Mismatch,
                        _ => FulfillErrorKind::Unsatisfied,
                    };
                    let because = match self.obs[i].goal {
                        Goal::Trait { trait_, args } => self.because(trait_, args),
                        Goal::ConstHolds { cond, args } => self.because_of(cond, args),
                        _ => None,
                    };
                    self.errors.push(FulfillError { obligation: self.obs[i], kind, because });
                }
                self.set_state(i, s);
            }
            match r {
                Ok(()) => return Ok(()),
                Err(blaze_solve::Stop::Need(q)) => {
                    self.fill(q, reads)?;
                    self.need_rounds += 1;
                }
                Err(e) => solve_err(e)?,
            }
        }
        Err(CheckError::Limit(crate::error::Limit::Solver).into())
    }

    /// Registers the projection goals relating emitted, each an obligation of `cause` (the
    /// place whose check related the types: [`TypeCtx::unify_at`]).
    fn defer(&mut self, pairs: &[(STy, STy)], cause: crate::ty::Cause) {
        for &(alias, term) in pairs {
            let ty = Ty(alias.0);
            let goal = if self.node(alias.0).as_ty().is_some()
                && let TyView::Alias { trait_, name, args } = self.view(ty)
            {
                Goal::Projection { trait_, name, args, term: Ty(term.0) }
            } else {
                // Const and opaque aliases have no associated-type name in a typeck goal.
                Goal::WellFormed(ty)
            };
            self.obs.push(Obligation { goal, cause });
            self.preds.push(Pred::Projection { alias, term });
            self.state.push(State::Pending);
        }
    }
}

/// A candidate of the solver's (`Choice`) as an undecided goal's chain names it.
fn why_cand(c: blaze_solve::witness::Choice) -> crate::diag::WhyCand {
    use blaze_solve::assembly::Source;
    use crate::diag::WhyCand;
    match c.source {
        Source::Impl(d) => WhyCand::Impl { impl_: ItemKey(d.0) },
        Source::ParamEnv { .. } => WhyCand::WhereClause { nth: c.n },
        Source::AliasBound { .. } => WhyCand::AliasBound,
        Source::Builtin(_) => WhyCand::Builtin,
        Source::CoherenceUnknowable => WhyCand::Unknowable,
    }
}

impl<'a> TypeCtx for InferCtx<'a> {
    type Snapshot = InferSnapshot;

    fn mk(&mut self, v: TyView) -> Result<Ty, CheckError> {
        use blaze_ty::Ty as B;
        let t = match v {
            TyView::Bool => B::BOOL,
            TyView::Char => B::CHAR,
            TyView::Int(i) => B::int(terms::b_int(facts_int(i))),
            TyView::Uint(u) => B::uint(terms::b_uint(facts_uint(u))),
            TyView::Float(f) => B::float(terms::b_float(facts_float(f))),
            TyView::Str => B::STR,
            TyView::Never => B::NEVER,
            TyView::Tuple(l) => {
                let l = self.blist(l);
                self.terms.cx.tys.tuple(l)
            }
            TyView::Record(s, l) => {
                let l = self.blist(l);
                self.terms.cx.tys.record(s.0, l)
            }
            TyView::Union(l) => {
                let l = self.blist(l);
                self.terms.cx.tys.union(l)
            }
            TyView::Ref(m, t) => {
                let t = self.bty(t);
                self.terms.cx.tys.reference(Region::ERASED, t, mutability(m))
            }
            TyView::Ptr(m, t) => {
                let t = self.bty(t);
                self.terms.cx.tys.ptr(t, mutability(m))
            }
            TyView::Array(t, n) => {
                let t = self.bty(t);
                let Some(n) = self.node(n.0).as_const() else {
                    return Err(CheckError::Malformed(crate::error::Malformed::Sort { node: n.0 }));
                };
                self.terms.cx.tys.array(t, n)
            }
            TyView::Slice(t) => {
                let t = self.bty(t);
                self.terms.cx.tys.slice(t)
            }
            TyView::Pat(t, l) => {
                let t = self.bty(t);
                let l = self.blist(l);
                self.terms.cx.tys.pat(t, l)
            }
            TyView::Adt(k, l) => {
                let d = self.terms.bdef(SDef(k.0));
                let l = self.blist(l);
                self.terms.cx.tys.adt(d, l)
            }
            TyView::FnDef(k, l) => {
                let d = self.terms.bdef(SDef(k.0));
                let l = self.blist(l);
                self.terms.cx.tys.fn_def(d, l)
            }
            TyView::FnPtr(l, h) => {
                let l = self.blist(l);
                self.terms.cx.tys.fn_ptr(l, terms::b_header(h))
            }
            TyView::Closure(e, sig) => {
                let mut h = crate::hash::StableHasher::new("blaze-typeck closure def");
                h.u32(e.0);
                let d = u128::from(h.finish());
                // Both call-kind and structural obligations wait on capture analysis.
                let (kind, upvars) = match self.rows.closures.get(&d) {
                    Some(r) => (r.kind, r.upvars),
                    None => (self.terms.cx.next_const_var(), self.terms.cx.next_ty_var()),
                };
                self.rows.closures.insert(d, ClosureRow { expr: e, sig, kind, upvars });
                let def = self.terms.bdef(SDef(d));
                let sig = self.rust_call_sig(sig)?;
                let args = self.terms.cx.tys.mk_list(&[kind.arg(), sig.arg(), upvars.arg()]);
                self.terms.cx.tys.closure(def, args)
            }
            TyView::ClosureOf { owner, ordinal, args, sig, kind, upvars } => {
                return self.imported_closure(owner, ordinal, args, sig, kind, upvars);
            }
            // The owner's record and a caller reading it use the same nominal instance.
            TyView::CoroutineOf { owner, ordinal, args, output, interior } => {
                return self.imported_coroutine(owner, ordinal, args, output, interior);
            }
            // `Coroutine(def, [output, interior])`; its def a hash of the block's expression, as
            // a closure's (one per block of this body; the owner's key separates bodies).
            TyView::Future(e, output, interior) => {
                let mut h = crate::hash::StableHasher::new("blaze-typeck future def");
                h.u32(e.0);
                let d = u128::from(h.finish());
                self.rows.futures.insert(d, e);
                let def = self.terms.bdef(SDef(d));
                let (o, i) = (self.bty(output), self.bty(interior));
                let args = self.terms.cx.tys.mk_list(&[o.arg(), i.arg()]);
                self.terms.cx.tys.coroutine(def, args)
            }
            TyView::Param(i) => self.terms.cx.tys.param(i),
            TyView::Alias { trait_, name, args } => self.alias(trait_, name, args),
            TyView::Opaque(k, l) => {
                let d = self.terms.bdef(SDef(k.0));
                let l = self.blist(l);
                // RIGID: to the checker an opaque type is itself (the Reference, "Abstract
                // return types": "a caller of the function is not permitted to know the
                // concrete type"; the body that defines it has a variable instead:
                // rules/opaque_def.rs). A non-rigid alias is unified LAZILY (the pair is
                // deferred and counted as related), so a variable was never bound to an opaque
                // (`let out = mk();` left `out` unknown, and `&mut out` then met `&mut dyn Tr`
                // by plain unification, E0308 instead of the unsizing coercion), and an
                // opaque "equalled" any type until the deferred pair failed. Its auto traits
                // are still read from its hidden type (blaze-solve structural.rs matches the
                // alias rigid or not).
                //
                // After type checking (`TypingMode::PostAnalysis`) an opaque is NEVER rigid: it
                // is its hidden type (the Reference, "Impl trait"; rustc's `Reveal::All`), a
                // non-rigid alias the solver replaces by that type, one layer, wherever it meets
                // it (a trait goal's self type: blaze-solve alias.rs `needs_normalizing`; a
                // relation: a deferred `Projection` goal). It is then also the SAME term as the
                // solver's own reading of a record (infer/facts.rs makes every opaque
                // `Rigid::No`): a rigid `O` in an instance's arguments (a recursive future's
                // interior `(Pin<Box<O>>,)`) met the revealed hidden type's non-rigid `O`, the
                // deferred `Projection { O, O_rigid }` related `O_rigid` with the hidden
                // coroutine as a constructor, and the impl was `NoImpl`
                // (suite/opaque_post_analysis_recursive.rs). Equal handles end the reveal: the
                // hidden coroutine's `O` against the instance's `O` is one term.
                match self.mode {
                    TypingMode::PostAnalysis => self.terms.cx.tys.alias(AliasKind::Opaque, d, l),
                    TypingMode::Analysis | TypingMode::Coherence => self.terms.cx.tys.rigid_alias(AliasKind::Opaque, d, l),
                }
            }
            TyView::Infer(v) | TyView::ConstInfer(v) => {
                let kind = match v.kind {
                    VarKind::Ty => blaze_ty::VarKind::Ty,
                    VarKind::Int => blaze_ty::VarKind::Int,
                    VarKind::Float => blaze_ty::VarKind::Float,
                    VarKind::Const => blaze_ty::VarKind::Const,
                };
                // A handle is a node's bits, whether the node is a type or a const.
                return match self.terms.cx.var_node(blaze_ty::VarId { kind, index: v.index }) {
                    Some(n) => Ok(Ty(n.bits())),
                    None => Err(CheckError::Malformed(crate::error::Malformed::HoleInAnswer)),
                };
            }
            TyView::Error => B::ERROR,
            TyView::ConstValue { ty, bits } => {
                let vt = self.bty(ty);
                // A scalar, or an enum without generics (`PortableTys::variant_value`).
                let kind = self.terms.cx.tys.ty_kind(vt);
                if terms::s_val_ty(kind).is_none() && !matches!(kind, TyKind::Adt(_, l) if l == blaze_ty::List::EMPTY) {
                    return Err(CheckError::Malformed(crate::error::Malformed::ConstValueType { node: ty.0 }));
                }
                return Ok(Ty(self.terms.cx.tys.const_value(vt, bits).node().bits()));
            }
            TyView::ConstParam(i) => return Ok(Ty(self.terms.cx.tys.const_param(i).node().bits())),
            // blaze-ty's `dynamic` keeps the rows in the order given: the view's one order.
            TyView::Dynamic(l) => {
                let l = self.blist(l);
                self.terms.cx.tys.dynamic(l, Region::ERASED)
            }
            TyView::ExTrait(k, l) => {
                let d = self.terms.bdef(SDef(k.0));
                let l = self.blist(l);
                self.terms.cx.tys.ex_trait(d, l)
            }
            TyView::ExProj { trait_, name, args_term } => {
                let d = assoc_def(trait_, name);
                self.facts.names.insert(d, (trait_, name));
                let def = self.terms.bdef(SDef(d));
                let l = self.blist(args_term);
                self.terms.cx.tys.ex_projection(def, l)
            }
            TyView::ExAuto(k) => {
                let d = self.terms.bdef(SDef(k.0));
                self.terms.cx.tys.ex_auto(d)
            }
            TyView::Foreign(k) => {
                let d = self.terms.bdef(SDef(k.0));
                self.terms.cx.tys.foreign(d)
            }
        };
        Ok(Ty(t.node().bits()))
    }

    fn take_malformed(&mut self) -> Option<crate::error::Malformed> {
        self.rows.malformed.take()
    }

    #[inline]
    fn ty_flags(&self, t: Ty) -> TyFlags {
        let node = self.node(t.0);
        if node.as_ty().is_none() && node.as_const().is_none() {
            return self.unreadable(t.0, crate::error::Unreadable::NotAType, TyFlags::default());
        }
        TyFlags::from_interned(self.terms.cx.tys.node_flags(node))
    }

    #[inline]
    fn list_flags(&self, l: TyList) -> TyFlags {
        TyFlags::from_interned(self.terms.cx.tys.node_flags(self.blist(l).node()))
    }

    /// One level of `t`. A handle it cannot view is noted (`unreadable`) and viewed as the error
    /// type: the body's check then fails with it by name (rule 20), never the process.
    fn view(&self, t: Ty) -> TyView {
        self.seen().view(t)
    }

    fn mk_list(&mut self, tys: &[Ty]) -> Result<TyList, CheckError> {
        // A list holds any argument: types and const terms (const generic arguments).
        let mut v: Vec<blaze_ty::Arg, Alloc<'a>> = Vec::with_capacity_in(tys.len(), self.alloc);
        for &t in tys {
            match self.node(t.0).arg() {
                Some(a) => v.push(a),
                None => return Err(CheckError::Malformed(crate::error::Malformed::Sort { node: t.0 })),
            }
        }
        Ok(TyList(self.terms.cx.tys.mk_list(&v).node().bits()))
    }

    fn list_len(&self, l: TyList) -> u32 {
        self.seen().list_len(l)
    }

    fn list_get(&self, l: TyList, i: u32) -> Ty {
        self.seen().list_get(l, i)
    }

    fn fresh(&mut self, kind: VarKind) -> Result<Ty, CheckError> {
        let t = match kind {
            VarKind::Ty => self.terms.cx.next_ty_var(),
            VarKind::Int => self.terms.cx.next_int_var(),
            VarKind::Float => self.terms.cx.next_float_var(),
            VarKind::Const => return Ok(Ty(self.terms.cx.next_const_var().node().bits())),
        };
        Ok(Ty(t.node().bits()))
    }

    fn shallow_resolve(&mut self, t: Ty) -> Ty {
        if let Some(c) = self.node(t.0).as_const() {
            return Ty(self.terms.cx.shallow_resolve_const(c).node().bits());
        }
        let b = self.bty(t);
        Ty(self.terms.cx.shallow_resolve(b).node().bits())
    }

    fn resolve(&mut self, t: Ty) -> Result<Ty, CheckError> {
        self.exporter().resolve(t)
    }

    fn list_has_infer(&mut self, args: TyList) -> bool {
        let l = self.slist(args);
        let mut scratch = blaze_solve::fold::Scratch::new_in(self.alloc);
        (0..l.len).any(|i| {
            let t = self.terms.list_get(l, i);
            match blaze_solve::fold::resolve(&mut self.terms, &mut scratch, t) {
                Ok(t) => self.terms.flags(t).has(blaze_solve::term::Flags::HAS_INFER),
                Err(_) => true,
            }
        })
    }

    fn var_mark(&self) -> crate::ty::VarMark {
        let count = |k: blaze_ty::VarKind| self.terms.cx.vars.var_count(k) as u32;
        crate::ty::VarMark([
            count(blaze_ty::VarKind::Ty),
            count(blaze_ty::VarKind::Int),
            count(blaze_ty::VarKind::Float),
            count(blaze_ty::VarKind::Const),
        ])
    }

    fn list_has_infer_since(&mut self, args: TyList, mark: crate::ty::VarMark) -> bool {
        let l = self.slist(args);
        let mut work = Vec::new_in(self.alloc);
        for i in 0..l.len {
            let t = self.terms.list_get(l, i);
            let mut made = false;
            // `visit` resolves each node it meets (`shallow_resolve`), so a bound variable is
            // seen as its value.
            let walked = blaze_solve::fold::visit(&self.terms, &mut work, t, |_, k| {
                if let blaze_solve::term::TyKind::Infer(v) = k {
                    made |= match terms::var_of_index(v.index) {
                        Some(id) => match id.kind {
                            blaze_ty::VarKind::Ty => id.index >= mark.0[0],
                            blaze_ty::VarKind::Int => id.index >= mark.0[1],
                            blaze_ty::VarKind::Float => id.index >= mark.0[2],
                            blaze_ty::VarKind::Const => id.index >= mark.0[3],
                            blaze_ty::VarKind::Region => false,
                        },
                        // Not an index this context makes: say it holds one (no hint taken).
                        None => true,
                    };
                }
                true
            });
            // A walk past its limit says it holds one: the hint is not taken.
            if walked.is_err() || made {
                return true;
            }
        }
        false
    }

    fn unify(&mut self, a: Ty, b: Ty) -> Result<bool, CheckError> {
        self.unify_at(a, b, crate::ty::Cause::Body)
    }

    fn unify_at(&mut self, a: Ty, b: Ty, cause: crate::ty::Cause) -> Result<bool, CheckError> {
        #[cfg(test)]
        let (a, b) = {
            let (mut a, mut b) = (a, b);
            while self.plant == InferPlant::AnyLength {
                let (ra, rb) = (self.shallow_resolve(a), self.shallow_resolve(b));
                let (TyView::Array(ea, _), TyView::Array(eb, _)) = (self.view(ra), self.view(rb)) else { break };
                (a, b) = (ea, eb);
            }
            while self.plant == InferPlant::PatIsBase {
                let (ra, rb) = (self.shallow_resolve(a), self.shallow_resolve(b));
                match (self.view(ra), self.view(rb)) {
                    (TyView::Pat(base, _), v) if !matches!(v, TyView::Pat(..)) => a = base,
                    (v, TyView::Pat(base, _)) if !matches!(v, TyView::Pat(..)) => b = base,
                    _ => break,
                }
            }
            (a, b)
        };
        let mut pairs: Vec<(STy, STy), Alloc<'a>> = Vec::new_in(self.alloc);
        let ok = self.terms.unify(STy(a.0), STy(b.0), blaze_solve::tys::Relate::Lazy, &mut pairs)?;
        if ok {
            self.defer(&pairs, cause);
        }
        Ok(ok)
    }

    fn snapshot(&mut self) -> InferSnapshot {
        InferSnapshot {
            cx: self.terms.cx.snapshot(),
            obs: self.obs.len(),
            undo: self.undo.len(),
            errors: self.errors.len(),
            error_at: self.error_at,
            env_clauses: self.env_clauses.len(),
            env: self.env,
        }
    }

    fn rollback_to(&mut self, s: InferSnapshot) {
        self.terms.cx.rollback_to(s.cx);
        while self.undo.len() > s.undo {
            let Some((i, old)) = self.undo.pop() else { break };
            if (i as usize) < self.state.len() {
                self.state[i as usize] = old;
            }
        }
        self.obs.truncate(s.obs);
        self.preds.truncate(s.obs);
        self.state.truncate(s.obs);
        self.errors.truncate(s.errors);
        self.error_at = s.error_at;
        self.env_clauses.truncate(s.env_clauses);
        self.env = s.env;
    }

    fn commit(&mut self, s: InferSnapshot) {
        self.terms.cx.commit(s.cx);
    }

    fn assume(&mut self, clause: Goal) -> Result<(), CheckError> {
        let pred = self.pred(clause);
        self.env_clauses.push(Clause { pred, bound: 0 });
        self.env = None;
        Ok(())
    }

    fn register(&mut self, ob: Obligation) -> Result<(), CheckError> {
        let p = self.pred(ob.goal);
        self.obs.push(ob);
        self.preds.push(p);
        self.state.push(State::Pending);
        Ok(())
    }

    fn select_where_possible<W: World>(&mut self, reads: &mut Reads<'_, '_, W>) -> Result<(), Stop> {
        self.select(reads)
    }

    fn select_all<W: World>(&mut self, reads: &mut Reads<'_, '_, W>) -> Result<(), Stop> {
        self.select(reads)?;
        #[cfg(test)]
        if self.plant == InferPlant::AmbiguityByOrder {
            self.settle_by_order(reads)?;
            self.select(reads)?;
        }
        for i in 0..self.obs.len() {
            if matches!(self.state[i], State::Pending | State::Overflow) {
                self.errors.push(FulfillError { obligation: self.obs[i], kind: FulfillErrorKind::Ambiguous, because: None });
                self.set_state(i, State::Fails);
            }
        }
        Ok(())
    }

    fn next_error(&mut self) -> Option<FulfillError> {
        let e = self.errors.get(self.error_at).copied()?;
        self.error_at += 1;
        Some(e)
    }

    fn pending_goals(&self, out: &mut blaze_own::Vec<Goal, blaze_own::Alloc<'_>>) {
        for i in 0..self.obs.len() {
            if self.state[i] == State::Pending {
                out.push(self.obs[i].goal);
            }
        }
    }

    fn may_hold<W: World>(&mut self, goal: Goal, reads: &mut Reads<'_, '_, W>) -> Result<bool, Stop> {
        let s = self.snapshot();
        let pred = self.pred(goal);
        let r = self.evaluate(pred, reads);
        self.rollback_to(s);
        Ok(!matches!(r?, Answer::No))
    }

    fn why_chain<W: World>(
        &mut self,
        goal: Goal,
        steps: &mut Vec<crate::diag::PendingWhyStep, Alloc<'_>>,
        reads: &mut Reads<'_, '_, W>,
    ) -> Result<Option<crate::diag::WhyLeaf>, Stop> {
        self.why_walk(goal, steps, reads)
    }

    fn why_maybe_chain<W: World>(
        &mut self,
        goal: Goal,
        steps: &mut Vec<crate::diag::PendingWhyStep, Alloc<'_>>,
        reads: &mut Reads<'_, '_, W>,
    ) -> Result<Option<crate::diag::WhyLeaf>, Stop> {
        self.why_maybe_walk(goal, steps, reads)
    }

    fn hidden_unknown<W: World>(
        &mut self,
        goal: Goal,
        skip: &dyn Fn(ItemKey) -> bool,
        reads: &mut Reads<'_, '_, W>,
    ) -> Result<Option<ItemKey>, Stop> {
        self.hidden_unknown_in(goal, skip, reads)
    }

    fn why_leaf<W: World>(&mut self, goal: Goal, reads: &mut Reads<'_, '_, W>) -> Result<Option<(ItemKey, TyList)>, Stop> {
        let mut steps: Vec<crate::diag::PendingWhyStep, Alloc<'a>> = Vec::new_in(self.alloc);
        if self.why_walk(goal, &mut steps, reads)?.is_none() || steps.len() < 2 {
            return Ok(None);
        }
        Ok(steps.last().and_then(|s| s.goal))
    }

    fn normalize<W: World>(
        &mut self,
        trait_: ItemKey,
        args: TyList,
        name: Name,
        reads: &mut Reads<'_, '_, W>,
    ) -> Result<Option<Normalized>, Stop> {
        let s = self.snapshot();
        let alias = self.alias(trait_, name, args);
        let var = self.terms.cx.next_ty_var();
        let pred = Pred::Projection { alias: STy(alias.node().bits()), term: STy(var.node().bits()) };
        match self.evaluate(pred, reads) {
            Ok(Answer::Yes) => {
                self.commit(s);
                let r = self.terms.cx.resolve_ty(var);
                Ok(Some(Normalized::Certain(Ty(r.node().bits()))))
            }
            // Selected, with a nested goal undecided: the answer's constraints are applied, so
            // the term is the answer when they decided it (`Normalized::Ambiguous`). An
            // overflow's constraints are not kept (rustc's `MaybeCause::Overflow { keep_constraints:
            // false }` in the root), nor a term still a bare variable (two candidates left).
            Ok(Answer::Maybe(blaze_solve::term::Certainty::Ambiguous)) => {
                let r = Ty(self.terms.cx.resolve_ty(var).node().bits());
                if matches!(self.view(r), TyView::Infer(_)) {
                    self.rollback_to(s);
                    return Ok(None);
                }
                self.commit(s);
                Ok(Some(Normalized::Ambiguous(r)))
            }
            Ok(_) => {
                self.rollback_to(s);
                Ok(None)
            }
            Err(e) => {
                self.rollback_to(s);
                Err(e)
            }
        }
    }

    fn settle_closure(&mut self, closure: ExprId, kind: crate::ty::ClosureKind, upvars: Option<Ty>) -> Result<(), CheckError> {
        use blaze_solve::term::{CLOSURE_FN, CLOSURE_FN_MUT, CLOSURE_FN_ONCE};
        let Some((&id, &row)) = self.rows.closures.iter().find(|(_, r)| r.expr == closure) else {
            return Err(CheckError::Malformed(crate::error::Malformed::ClosureNotMade(closure)));
        };
        let v = match kind {
            crate::ty::ClosureKind::Fn => CLOSURE_FN,
            crate::ty::ClosureKind::FnMut => CLOSURE_FN_MUT,
            crate::ty::ClosureKind::FnOnce => CLOSURE_FN_ONCE,
        };
        let value = self.terms.cx.tys.const_value(blaze_ty::Ty::uint(blaze_ty::UintTy::Usize), u128::from(v));
        let mut pairs: Vec<(STy, STy), Alloc<'a>> = Vec::new_in(self.alloc);
        let ok = self.terms.unify(
            STy(row.kind.node().bits()),
            STy(value.node().bits()),
            blaze_solve::tys::Relate::Lazy,
            &mut pairs,
        )?;
        let env = match upvars {
            Some(t) => t,
            None => {
                let def = self.terms.bdef(SDef(id));
                Ty(self.terms.cx.tys.foreign(def).node().bits())
            }
        };
        if !ok || !self.unify(Ty(row.upvars.node().bits()), env)? {
            return Err(CheckError::Malformed(crate::error::Malformed::ClosureCaptureConflict(closure)));
        }
        Ok(())
    }

    fn closure_hint(&mut self, var: Ty, fn_traits: [ItemKey; 3], output: Name) -> Result<Option<((TyList, crate::ty::Cause), Option<(Ty, crate::ty::Cause)>)>, CheckError> {
        let v = self.shallow_resolve(var);
        let mut found = None;
        for i in 0..self.obs.len() {
            if self.state[i] == State::Pending
                && let Goal::Trait { trait_, args } = self.obs[i].goal
                && fn_traits.contains(&trait_)
                && self.list_len(args) == 2
                && self.shallow_resolve(self.list_get(args, 0)) == v
            {
                let tuple = self.shallow_resolve(self.list_get(args, 1));
                if let TyView::Tuple(l) = self.view(tuple) {
                    // Where the expectation came from (D148): the obligation's cause, a
                    // clause of a use (`Cause::Clause`) when a declared bound filed it.
                    found = Some((l, self.obs[i].cause));
                    break;
                }
            }
        }
        let Some((inputs, cause)) = found else { return Ok(None) };
        // Output belongs to the Once supertrait at these same arguments. Another bound on
        // this variable may project a different Output, and may precede the callable bound:
        // a projection whose `Args` cannot equal the inputs is another bound's. "Can equal",
        // a rolled-back unification (rustc's `can_eq`), never the same list: each clause of a
        // bound is lowered on its own (machine.rs `register_predicates`), so an alias in `Args`
        // (reth's `F: FnOnce(RpcContext<'_, NodeAdapter<T, CB::Components>, AO::EthApi>) ->
        // eyre::Result<()>`) is one fresh normalization variable in the trait goal and another
        // in the projection. Compared as lists, the output was lost there and the closure's
        // return type was left to its body: tempo's crates/zones/node/tests/it/utils.rs:2658.
        let inputs_tuple = self.mk(TyView::Tuple(inputs))?;
        for i in 0..self.obs.len() {
            if self.state[i] == State::Pending
                && let Goal::Projection { trait_, args, name, term } = self.obs[i].goal
                && trait_ == fn_traits[2] && name == output && self.list_len(args) == 2
                && self.shallow_resolve(self.list_get(args, 0)) == v
            {
                let tuple = self.shallow_resolve(self.list_get(args, 1));
                let TyView::Tuple(l) = self.view(tuple) else { continue };
                let same = if l == inputs {
                    true
                } else if self.list_len(l) != self.list_len(inputs) {
                    false
                } else {
                    let s = self.snapshot();
                    let can = self.unify(tuple, inputs_tuple);
                    self.rollback_to(s);
                    can?
                };
                if same {
                    let ob_cause = self.obs[i].cause;
                    return Ok(Some(((inputs, cause), Some((term, ob_cause)))));
                }
            }
        }
        Ok(Some(((inputs, cause), None)))
    }
}

fn mutability(m: crate::ids::Mutbl) -> blaze_ty::Mutability {
    match m {
        crate::ids::Mutbl::Not => blaze_ty::Mutability::Not,
        crate::ids::Mutbl::Mut => blaze_ty::Mutability::Mut,
    }
}

fn mutbl(m: blaze_ty::Mutability) -> crate::ids::Mutbl {
    match m {
        blaze_ty::Mutability::Not => crate::ids::Mutbl::Not,
        blaze_ty::Mutability::Mut => crate::ids::Mutbl::Mut,
    }
}

fn facts_int(i: crate::ids::IntTy) -> blaze_solve::term::IntTy {
    use crate::ids::IntTy as T;
    use blaze_solve::term::IntTy as S;
    match i {
        T::Isize => S::Isize,
        T::I8 => S::I8,
        T::I16 => S::I16,
        T::I32 => S::I32,
        T::I64 => S::I64,
        T::I128 => S::I128,
    }
}

fn facts_uint(u: crate::ids::UintTy) -> blaze_solve::term::UintTy {
    use crate::ids::UintTy as T;
    use blaze_solve::term::UintTy as S;
    match u {
        T::Usize => S::Usize,
        T::U8 => S::U8,
        T::U16 => S::U16,
        T::U32 => S::U32,
        T::U64 => S::U64,
        T::U128 => S::U128,
    }
}

fn facts_float(f: crate::ids::FloatTy) -> blaze_solve::term::FloatTy {
    use crate::ids::FloatTy as T;
    use blaze_solve::term::FloatTy as S;
    match f {
        T::F16 => S::F16,
        T::F32 => S::F32,
        T::F64 => S::F64,
        T::F128 => S::F128,
    }
}

fn typeck_int(i: blaze_ty::IntTy) -> crate::ids::IntTy {
    use crate::ids::IntTy as T;
    match i {
        blaze_ty::IntTy::Isize => T::Isize,
        blaze_ty::IntTy::I8 => T::I8,
        blaze_ty::IntTy::I16 => T::I16,
        blaze_ty::IntTy::I32 => T::I32,
        blaze_ty::IntTy::I64 => T::I64,
        blaze_ty::IntTy::I128 => T::I128,
    }
}

fn typeck_uint(u: blaze_ty::UintTy) -> crate::ids::UintTy {
    use crate::ids::UintTy as T;
    match u {
        blaze_ty::UintTy::Usize => T::Usize,
        blaze_ty::UintTy::U8 => T::U8,
        blaze_ty::UintTy::U16 => T::U16,
        blaze_ty::UintTy::U32 => T::U32,
        blaze_ty::UintTy::U64 => T::U64,
        blaze_ty::UintTy::U128 => T::U128,
    }
}

fn typeck_float(f: blaze_ty::FloatTy) -> crate::ids::FloatTy {
    use crate::ids::FloatTy as T;
    match f {
        blaze_ty::FloatTy::F16 => T::F16,
        blaze_ty::FloatTy::F32 => T::F32,
        blaze_ty::FloatTy::F64 => T::F64,
        blaze_ty::FloatTy::F128 => T::F128,
    }
}
