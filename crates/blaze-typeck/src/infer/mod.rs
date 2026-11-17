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
//! A solver question the cache lacks stops the solve; the cache reads it through `Reads` and
//! the solve runs again ([`InferCtx::need_rounds`] counts these). A question the world lacks
//! stops the body with typeck's `Need`, as every read does.

mod facts;
mod terms;

pub use facts::{FactCache, assoc_def};
pub use terms::{Terms, def_of, item_key, var_index, var_of_index};

use blaze_hash::HashMap;
use blaze_own::{Alloc, Vec};
use blaze_solve::eval::Answer;
use blaze_solve::fulfill::{Fulfill, State, Wake};
use blaze_solve::term::{Def as SDef, List as SList, Polarity, Pred, Ty as STy, TypingMode};
use blaze_solve::{Clause, EnvId, Goal as SGoal, Solver, Tys as _};
use blaze_ty::{AliasKind, ConstKind, FnHeader, Node, Region, TyKind};

use crate::error::{CheckError, Stop};
use crate::ids::{ExprId, ItemKey, Name};
use crate::ty::{FulfillError, FulfillErrorKind, Goal, Obligation, Ty, TyList, TyView, TypeCtx, Var, VarKind};
use crate::world::{Reads, World};

/// The most fact-fill rounds one solver call may take (rule 18): each round fills one question
/// no earlier round filled, so a world never comes near it.
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
    closures: HashMap<u128, ClosureRow, Alloc<'a>>,
    /// Each closure of another body this context made, in the order made (a body names few).
    foreign: Vec<ForeignClosure, Alloc<'a>>,
    /// Each async block's future this body made, by its def: its expression.
    futures: HashMap<u128, ExprId, Alloc<'a>>,
    /// Each future of another body this context made (`TyView::CoroutineOf`).
    foreign_coroutines: Vec<ForeignCoroutine, Alloc<'a>>,
    /// Solver reads (the solver's own log; the body's read list is `Reads`).
    solver_reads: Vec<blaze_solve::Read, Alloc<'a>>,
    /// Solver calls repeated after a fact was filled: the Need rounds inside the context.
    pub need_rounds: u64,
    /// A planted bug of the body suite; `InferPlant::None` in every other test.
    #[cfg(test)]
    pub(crate) plant: InferPlant,
}

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
/// (`fn(inputs.., output)`), and its kind variable.
#[derive(Clone, Copy, Debug)]
struct ClosureRow {
    expr: ExprId,
    sig: Ty,
    kind: blaze_ty::Const,
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
/// solver type `mk` made for it: `view` gives it back (the identity's `args` are not in the
/// solver's list, so the made term finds them, as `ForeignClosure` does).
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

/// A snapshot of the context: the arena's tables and the obligation columns.
#[derive(Clone, Copy, Debug)]
pub struct InferSnapshot {
    cx: blaze_ty::Snapshot,
    obs: usize,
    undo: usize,
    errors: usize,
    env_clauses: usize,
    env: Option<EnvId>,
}

fn ty_of(n: Node) -> blaze_ty::Ty {
    match n.as_ty() {
        Some(t) => t,
        None => panic!("blaze-typeck: node {} is not a type", n.bits()),
    }
}

fn solve_err(e: blaze_solve::Stop) -> Result<(), CheckError> {
    match e {
        blaze_solve::Stop::Error(e) => Err(CheckError::Solve(e)),
        blaze_solve::Stop::Need(_) => Ok(()),
    }
}

impl<'a> InferCtx<'a> {
    /// The allocator the check's caller handed in (D102): what this check's scratch lives in.
    pub(crate) fn alloc(&self) -> Alloc<'a> {
        self.alloc
    }

    pub fn new_in(alloc: Alloc<'a>) -> InferCtx<'a> {
        InferCtx {
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
            closures: HashMap::new_in(alloc),
            foreign: Vec::new_in(alloc),
            futures: HashMap::new_in(alloc),
            foreign_coroutines: Vec::new_in(alloc),
            solver_reads: Vec::new_in(alloc),
            need_rounds: 0,
            #[cfg(test)]
            plant: InferPlant::None,
        }
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
        let st = self.solver.stats;
        s.refills += self.need_rounds;
        s.frames += st.frames;
        s.cache_hits += st.cache_hits;
        s.fast_path += st.fast_path;
        s.candidates += st.candidates;
        s.reruns += st.reruns;
    }

    fn node(&self, bits: u32) -> Node {
        match self.terms.cx.tys.node_at(bits) {
            Some(n) => n,
            None => panic!("blaze-typeck: handle {bits} is not in this context's arena"),
        }
    }

    fn bty(&self, t: Ty) -> blaze_ty::Ty {
        ty_of(self.node(t.0))
    }

    /// The closure of another body this context made whose owner's arguments, with the
    /// variables bound since resolved, are `l`'s: the solver rebuilds `{closure#0 of gen<?T>}`
    /// as `{closure#0 of gen<u8>}` once `?T = u8`, a new term of the same closure. Each
    /// argument is compared after a shallow probe: a variable nested inside an argument
    /// (`gen::<Vec<?T>>`) is not seen through, and its closure stays unviewable (a panic).
    fn foreign_resolved(&self, d: blaze_ty::Def, l: blaze_ty::List) -> Option<&ForeignClosure> {
        let tys = &self.terms.cx.tys;
        let n = tys.list_len(l);
        let same = |a: blaze_ty::Arg, b: blaze_ty::Arg| match (a.ty(), b.ty()) {
            (Some(x), Some(y)) => self.terms.cx.probe(x) == self.terms.cx.probe(y),
            _ => a == b,
        };
        self.foreign.iter().find(|f| {
            let TyKind::Closure(d2, l2) = tys.ty_kind(f.made) else { return false };
            d2 == d
                && tys.list_len(l2) == n
                && (0..n.saturating_sub(3)).all(|i| match (tys.list_get(l, i), tys.list_get(l2, i)) {
                    (Some(a), Some(b)) => same(a, b),
                    _ => false,
                })
        })
    }

    /// A closure's signature (a `FnPtr`, inputs then output) in the solver's "rust-call" form:
    /// its one input the tuple of the closure's inputs (blaze-solve builtin.rs `callable`).
    fn rust_call_sig(&mut self, sig: Ty) -> Result<blaze_ty::Ty, CheckError> {
        let TyView::FnPtr(io, _) = self.view(sig) else {
            panic!("blaze-typeck: a closure's signature {sig:?} is not a fn pointer")
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
        match self.node(l.0).as_list() {
            Some(l) => l,
            None => panic!("blaze-typeck: handle {} is not a list", l.0),
        }
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
        let blaze_solve::facts::Question::ConstHolds(cond, list) = q else { return self.facts.fill(q, reads) };
        let mut args: Vec<STy, Alloc<'a>> = Vec::with_capacity_in(list.len as usize, self.alloc);
        for i in 0..list.len {
            args.push(self.terms.list_get(list, i));
        }
        if self.facts.const_holds_known(cond, &args) {
            return Ok(());
        }
        let mut ground = crate::portable::PortableTys::new_in(self.alloc);
        let mut out: Vec<crate::portable::PTy, Alloc<'a>> = Vec::with_capacity_in(args.len(), self.alloc);
        for &s in args.iter() {
            match crate::instance::export(self, Ty(s.0), &mut ground, 0)? {
                Some(p) => out.push(p),
                None => return Err(CheckError::Malformed(crate::error::Malformed::NotGround { cond: ItemKey(cond.0) }).into()),
            }
        }
        let got = reads.const_holds(ItemKey(cond.0), &out, ground.view())?;
        self.facts.keep_const_holds(cond, &args, got)?;
        Ok(())
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
            let clauses = &self.env_clauses;
            let at = reads.stamp();
            let r = self.solver.env(&mut self.terms, &self.facts, clauses, &mut self.solver_reads);
            reads.count_solve(at);
            match r {
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
        for _ in 0..MAX_FILLS {
            let g = SGoal { env, pred, bound: 0 };
            let at = reads.stamp();
            let r = self.solver.evaluate(&mut self.terms, &self.facts, TypingMode::Analysis, g, &mut self.solver_reads);
            reads.count_solve(at);
            match r {
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

    fn select<W: World>(&mut self, reads: &mut Reads<'_, '_, W>) -> Result<(), Stop> {
        let env = self.env(reads)?;
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
            let at = reads.stamp();
            let r = f.select(&mut self.solver, &mut self.terms, &self.facts, TypingMode::Analysis, &mut self.solver_reads);
            reads.count_solve(at);
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

    /// Registers the projection goals relating emitted.
    fn defer(&mut self, pairs: &[(STy, STy)]) {
        for &(alias, term) in pairs {
            self.obs.push(Obligation { goal: Goal::WellFormed(Ty(alias.0)), cause: crate::ty::Cause::Body });
            self.preds.push(Pred::Projection { alias, term });
            self.state.push(State::Pending);
        }
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
                // The closure kind (`Fn`, `FnMut`, `FnOnce`): a const variable, one per closure,
                // set by `settle_closure_kind` once its captures are known.
                let kind = match self.closures.get(&d) {
                    Some(r) => r.kind,
                    None => self.terms.cx.next_const_var(),
                };
                self.closures.insert(d, ClosureRow { expr: e, sig, kind });
                let def = self.terms.bdef(SDef(d));
                let sig = self.rust_call_sig(sig)?;
                let args = self.terms.cx.tys.mk_list(&[kind.arg(), sig.arg(), B::UNIT.arg()]);
                self.terms.cx.tys.closure(def, args)
            }
            // A closure of another body: its def from its identity, its arguments the owner's
            // then rustc's `[kind, sig, upvars]` (upvars `()`: the captures are the owner's).
            TyView::ClosureOf { owner, ordinal, args, sig, kind } => {
                use blaze_solve::term::{CLOSURE_FN, CLOSURE_FN_MUT, CLOSURE_FN_ONCE};
                let def = self.terms.bdef(SDef(crate::coroutine_id::closure_of_def(owner, ordinal)));
                let k = match kind {
                    crate::ty::ClosureKind::Fn => CLOSURE_FN,
                    crate::ty::ClosureKind::FnMut => CLOSURE_FN_MUT,
                    crate::ty::ClosureKind::FnOnce => CLOSURE_FN_ONCE,
                };
                let k = self.terms.cx.tys.const_value(blaze_ty::Ty::uint(blaze_ty::UintTy::Usize), u128::from(k));
                let solver_sig = self.rust_call_sig(sig)?;
                let bl = self.blist(args);
                let n = self.terms.cx.tys.list_len(bl);
                let mut all: Vec<blaze_ty::Arg, Alloc<'a>> = Vec::with_capacity_in(n as usize + 3, self.alloc);
                for i in 0..n {
                    let Some(a) = self.terms.cx.tys.list_get(bl, i) else {
                        panic!("blaze-typeck: argument {i} of a list of {n}")
                    };
                    all.push(a);
                }
                all.push(k.arg());
                all.push(solver_sig.arg());
                all.push(B::UNIT.arg());
                let l = self.terms.cx.tys.mk_list(&all);
                let t = self.terms.cx.tys.closure(def, l);
                self.foreign.push(ForeignClosure { made: t, owner, ordinal, args, sig, kind });
                t
            }
            // A future of another body: its def from its identity alone ("blaze-typeck
            // coroutine def", coroutine_id.rs), so the owner's record and a caller reading it
            // name one coroutine; its arguments `[output, interior]` as the body's own form.
            TyView::CoroutineOf { owner, ordinal, args, output, interior } => {
                let def = self.terms.bdef(SDef(crate::coroutine_id::coroutine_def(owner, ordinal)));
                let (o, i) = (self.bty(output), self.bty(interior));
                let l = self.terms.cx.tys.mk_list(&[o.arg(), i.arg()]);
                let t = self.terms.cx.tys.coroutine(def, l);
                self.foreign_coroutines.push(ForeignCoroutine { made: t, owner, ordinal, args, output, interior });
                t
            }
            // `Coroutine(def, [output, interior])`; its def a hash of the block's expression, as
            // a closure's (one per block of this body; the owner's key separates bodies).
            TyView::Future(e, output, interior) => {
                let mut h = crate::hash::StableHasher::new("blaze-typeck future def");
                h.u32(e.0);
                let d = u128::from(h.finish());
                self.futures.insert(d, e);
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
                self.terms.cx.tys.rigid_alias(AliasKind::Opaque, d, l)
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
        };
        Ok(Ty(t.node().bits()))
    }

    fn view(&self, t: Ty) -> TyView {
        let tys = &self.terms.cx.tys;
        let ty = |x: blaze_ty::Ty| Ty(x.node().bits());
        let list = |l: blaze_ty::List| TyList(l.node().bits());
        let key = |d: blaze_ty::Def| ItemKey(def_of(tys.item_key(d)));
        if let Some(c) = self.node(t.0).as_const() {
            return match tys.const_kind(c) {
                ConstKind::Value(vt, bits) => TyView::ConstValue { ty: ty(vt), bits },
                ConstKind::Param(i) => TyView::ConstParam(i),
                ConstKind::Var(v) => TyView::ConstInfer(Var { kind: VarKind::Const, index: v }),
                ConstKind::Error => TyView::Error,
                k => panic!("blaze-typeck: a const typeck has no view of yet: {k:?}"),
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
                    panic!("blaze-typeck: a closure without its signature")
                }
                if let Some(r) = self.closures.get(&k) {
                    return TyView::Closure(r.expr, r.sig);
                }
                let made = self.bty(t);
                let exact = self.foreign.iter().find(|f| f.made == made);
                let Some(f) = exact.or_else(|| self.foreign_resolved(d, l)) else {
                    panic!("blaze-typeck: a closure this context did not make")
                };
                TyView::ClosureOf { owner: f.owner, ordinal: f.ordinal, args: f.args, sig: f.sig, kind: f.kind }
            }
            TyKind::Coroutine(d, l) => {
                let k = def_of(tys.item_key(d));
                let made = self.bty(t);
                if let Some(f) = self.foreign_coroutines.iter().find(|f| f.made == made) {
                    return TyView::CoroutineOf { owner: f.owner, ordinal: f.ordinal, args: f.args, output: f.output, interior: f.interior };
                }
                let Some(&e) = self.futures.get(&k) else { panic!("blaze-typeck: a future this body did not make") };
                if tys.list_len(l) != 2 {
                    panic!("blaze-typeck: a future without its output and interior")
                }
                let at = |i: u32| match tys.list_get(l, i).and_then(blaze_ty::Arg::ty) {
                    Some(t) => ty(t),
                    None => panic!("blaze-typeck: a future's argument is not a type"),
                };
                TyView::Future(e, at(0), at(1))
            }
            TyKind::Param(i) => TyView::Param(i),
            TyKind::Alias { kind: AliasKind::Opaque, def, args, .. } => TyView::Opaque(key(def), list(args)),
            TyKind::Alias { def, args, .. } => {
                let d = def_of(tys.item_key(def));
                match self.facts.names.get(&d) {
                    Some(&(trait_, name)) => TyView::Alias { trait_, name, args: list(args) },
                    None => panic!("blaze-typeck: an alias whose trait and name this context never saw"),
                }
            }
            TyKind::TyVar(v) => TyView::Infer(Var { kind: VarKind::Ty, index: v }),
            TyKind::IntVar(v) => TyView::Infer(Var { kind: VarKind::Int, index: v }),
            TyKind::FloatVar(v) => TyView::Infer(Var { kind: VarKind::Float, index: v }),
            TyKind::Error => TyView::Error,
            TyKind::Dynamic(preds, _) => TyView::Dynamic(list(preds)),
            TyKind::NotAType => {
                let Some(p) = tys.as_pred(self.bty(t)) else { panic!("blaze-typeck: a row neither type nor predicate") };
                match tys.pred_kind(p) {
                    blaze_ty::PredKind::ExTrait(d, l) => TyView::ExTrait(key(d), list(l)),
                    blaze_ty::PredKind::ExAuto(d) => TyView::ExAuto(key(d)),
                    blaze_ty::PredKind::ExProjection(d, l) => {
                        let Some(&(trait_, name)) = self.facts.names.get(&def_of(tys.item_key(d))) else {
                            panic!("blaze-typeck: an object binding whose trait and name this context never saw")
                        };
                        TyView::ExProj { trait_, name, args_term: list(l) }
                    }
                    k => panic!("blaze-typeck: a predicate typeck has no view of: {k:?}"),
                }
            }
            k => panic!("blaze-typeck: a type typeck has no view of yet: {k:?}"),
        }
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
        self.terms.cx.tys.list_len(self.blist(l))
    }

    fn list_get(&self, l: TyList, i: u32) -> Ty {
        match self.terms.cx.tys.list_get(self.blist(l), i) {
            Some(a) => Ty(a.bits()),
            None => panic!("blaze-typeck: element {i} past a list's end"),
        }
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
        let n = self.node(t.0);
        Ok(Ty(self.terms.cx.resolve(n).bits()))
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
            self.defer(&pairs);
        }
        Ok(ok)
    }

    fn snapshot(&mut self) -> InferSnapshot {
        InferSnapshot {
            cx: self.terms.cx.snapshot(),
            obs: self.obs.len(),
            undo: self.undo.len(),
            errors: self.errors.len(),
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
        self.error_at = self.error_at.min(s.errors);
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

    fn may_hold<W: World>(&mut self, goal: Goal, reads: &mut Reads<'_, '_, W>) -> Result<bool, Stop> {
        let s = self.snapshot();
        let pred = self.pred(goal);
        let r = self.evaluate(pred, reads);
        self.rollback_to(s);
        Ok(!matches!(r?, Answer::No))
    }

    fn normalize<W: World>(
        &mut self,
        trait_: ItemKey,
        args: TyList,
        name: Name,
        reads: &mut Reads<'_, '_, W>,
    ) -> Result<Option<Ty>, Stop> {
        let s = self.snapshot();
        let alias = self.alias(trait_, name, args);
        let var = self.terms.cx.next_ty_var();
        let pred = Pred::Projection { alias: STy(alias.node().bits()), term: STy(var.node().bits()) };
        match self.evaluate(pred, reads) {
            Ok(Answer::Yes) => {
                self.commit(s);
                let r = self.terms.cx.resolve_ty(var);
                Ok(Some(Ty(r.node().bits())))
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

    fn settle_closure_kind(&mut self, closure: ExprId, kind: crate::ty::ClosureKind) -> Result<bool, CheckError> {
        use blaze_solve::term::{CLOSURE_FN, CLOSURE_FN_MUT, CLOSURE_FN_ONCE};
        let Some(row) = self.closures.values().find(|r| r.expr == closure).copied() else {
            panic!("blaze-typeck: closure {closure:?} was never made")
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
        Ok(ok)
    }

    fn closure_hint(&mut self, var: Ty, fn_traits: [ItemKey; 3], output: Name) -> Option<(TyList, Option<Ty>)> {
        let v = self.shallow_resolve(var);
        let mut inputs = None;
        let mut out = None;
        for i in 0..self.obs.len() {
            if self.state[i] != State::Pending {
                continue;
            }
            match self.obs[i].goal {
                Goal::Trait { trait_, args } if inputs.is_none() && fn_traits.contains(&trait_) => {
                    if self.list_len(args) == 2 && self.shallow_resolve(self.list_get(args, 0)) == v {
                        let tuple = self.shallow_resolve(self.list_get(args, 1));
                        if let TyView::Tuple(l) = self.view(tuple) {
                            inputs = Some(l);
                        }
                    }
                }
                Goal::Projection { trait_, args, name, term } if out.is_none() && name == output => {
                    let _ = trait_;
                    if self.list_len(args) >= 1 && self.shallow_resolve(self.list_get(args, 0)) == v {
                        out = Some(term);
                    }
                }
                _ => {}
            }
        }
        inputs.map(|l| (l, out))
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
