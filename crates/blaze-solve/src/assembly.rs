//! Candidates and the context a rule runs in (solve.md, "Candidates and merging"): the API every
//! rule module (trait_goals, builtin, structural, alias, coherence) is written against.
//!
//! A goal's frame first runs its goal kind's *assembly*, which pushes [`Cand`] rows; then each
//! candidate runs in its own probe (a snapshot of the arena rolled back after): its rule's
//! [`step`] function is called with stage 0, adds equations ([`Cx::eq`]) and nested goals
//! ([`Cx::add`]), and returns a [`Step`]. `Step::Then(k)` asks the engine to evaluate the goals
//! added so far (the reference's `try_evaluate_added_goals`) and call the rule again with stage
//! `k`, with the candidate row (and its scratch fields `a`, `b`, `c`, `d`, `n`) as the rule left
//! it: a rule that needs a nested result (a normalized type, say) is a small state machine, so
//! the search keeps its explicit stack (solve.md (a)). `Step::Done(c)` evaluates what is left
//! and makes the candidate's response with shallow certainty `c`; `Step::No` drops it.
//!
//! Assembly itself is staged the same way ([`AStep`]), at the frame's level: goals it adds and
//! evaluates (normalizing the self type) constrain the frame, and those still pending are
//! copied into every candidate's probe, as the reference's probes inherit `nested_goals`.

use blaze_own::{Alloc, Vec};

use crate::error::{SolveError, Stop};
use crate::facts::{AdtDef, Ask, AssocDef, FnSigDef, ImplHeader, ImplSet, OpaqueDef, Portable, Read, TraitDef, Facts};
use crate::fold::{self, Inst, Scratch};
use crate::term::{Certainty, Def, GoalSource, Lang, List, Pred, Shape, Ty, TyKind, TypingMode, VarKind};
use crate::tys::Tys;

/// A clause of a param env: a predicate under `bound` bound type variables (`Bound(0, i)`,
/// `for<T> ..`; regions are not bound: they are erased).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Clause {
    pub pred: Pred,
    pub bound: u32,
}

/// A param env, interned by the solver (`Solver::env`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EnvId(pub u32);

/// A goal: a predicate under `bound` bound variables, in a param env.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Goal {
    pub env: EnvId,
    pub pred: Pred,
    pub bound: u32,
}

/// The source of a builtin impl candidate (`BuiltinImplSource`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BuiltinSource {
    /// No nested goals: preferred over every other candidate (`merge_trait_candidates`).
    Trivial,
    Misc,
    /// The object candidate of `dyn Trait: Trait` (a vtable entry).
    Object,
    TraitUpcasting,
}

/// Where a candidate comes from (`CandidateSource`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Source {
    Impl(Def),
    Builtin(BuiltinSource),
    /// A where-clause of the param env; `global` when it mentions no parameter, placeholder,
    /// inference variable or non-`'static` region (`ParamEnvSource::Global`).
    ParamEnv { global: bool },
    /// A bound of the self type's alias; `self_bounds` for the alias's own item bounds
    /// (`AliasBoundKind::SelfBounds`), not those of a nested alias.
    AliasBound { self_bounds: bool },
    /// Coherence: the goal may hold in a downstream or future crate.
    CoherenceUnknowable,
}

/// How a trait goal was proven, kept on its response for normalization (`TraitGoalProvenVia`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Via {
    None = 0,
    Misc = 1,
    ParamEnv = 2,
    AliasBound = 3,
    /// The trait goal has no solution (the reference's `compute_trait_goal(..)?` failing a
    /// `NormalizesTo` goal), as opposed to `None`: proven ambiguously.
    Failed = 4,
}

impl Via {
    pub fn from_byte(b: u8) -> Via {
        match b {
            1 => Via::Misc,
            2 => Via::ParamEnv,
            3 => Via::AliasBound,
            4 => Via::Failed,
            _ => Via::None,
        }
    }
}

/// The rule a candidate runs, by module. Each module's own `Rule` is a small `Copy` enum.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Rule {
    /// Holds with this certainty, no constraints (`forced_ambiguity`, trivial answers).
    Certain(Certainty),
    Trait(crate::trait_goals::Rule),
    Builtin(crate::builtin::Rule),
    Structural(crate::structural::Rule),
    Alias(crate::alias::Rule),
    Coherence(crate::coherence::Rule),
    Other(crate::eval::Rule),
}

/// One candidate: its source, its rule, and scratch fields its rule keeps across stages.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Cand {
    pub source: Source,
    pub rule: Rule,
    pub a: Ty,
    pub b: Ty,
    pub c: Ty,
    pub d: Def,
    pub n: u32,
}

impl Cand {
    pub fn new(source: Source, rule: Rule) -> Cand {
        Cand { source, rule, a: Ty::NONE, b: Ty::NONE, c: Ty::NONE, d: Def(0), n: 0 }
    }
}

/// What a rule's stage returns.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    /// Evaluate the goals added so far, then make the response with this shallow certainty.
    Done(Certainty),
    /// Evaluate the goals added so far, then call the rule again with this stage.
    Then(u8),
    /// The candidate does not apply.
    No,
}

/// What an assembly stage returns.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AStep {
    /// Assembly is complete.
    Done,
    /// Evaluate the goals added at the frame's level, then call assembly again with this stage.
    Then(u8),
    /// The goal fails before any candidate (a frame-level goal failed, say).
    No,
}

/// A nested goal a rule added.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Added {
    pub source: GoalSource,
    pub pred: Pred,
    pub bound: u32,
    /// Evaluated only to learn how it was proven (`Cx::via`): its response is not applied and
    /// its certainty not counted, as the reference's `probe(compute_trait_goal)`.
    pub via_only: bool,
}

/// The lang items, looked up once per solver (the read is recorded on every use).
pub struct Langs {
    pub(crate) state: [Option<Option<Def>>; LANGS],
    pub(crate) digests: [u64; LANGS],
}

pub(crate) const LANGS: usize = Lang::CallRefFuture as usize + 1;

impl Langs {
    pub fn new() -> Langs {
        Langs { state: [None; LANGS], digests: [0; LANGS] }
    }
}

impl Default for Langs {
    fn default() -> Langs {
        Langs::new()
    }
}

/// The context a rule or an assembly stage runs in.
pub struct Cx<'c, 'a, T: Tys, F: Facts> {
    pub tys: &'c mut T,
    pub(crate) ask: Ask<'c, F>,
    pub(crate) reads: &'c mut Vec<Read, Alloc<'a>>,
    pub(crate) added: &'c mut Vec<Added, Alloc<'a>>,
    pub(crate) deferred: &'c mut Vec<(Ty, Ty), Alloc<'a>>,
    pub(crate) scratch: &'c mut Scratch<'a>,
    pub(crate) langs: &'c mut Langs,
    pub(crate) universe: &'c mut u32,
    /// How the last `via_only` goal was proven (`Via::None` when it failed or was ambiguous).
    pub(crate) via: Via,
    /// The goal of the frame.
    pub goal: Goal,
    /// The goal's param env.
    pub env: &'c [Clause],
    pub mode: TypingMode,
    pub alloc: Alloc<'a>,
}

impl<'c, 'a, T: Tys, F: Facts> Cx<'c, 'a, T, F> {
    // Terms.

    /// `t` with a bound variable at its top replaced by its value.
    pub fn resolve(&self, t: Ty) -> Ty {
        self.tys.shallow_resolve(t)
    }

    /// The kind of `t` after `resolve`.
    pub fn kind(&self, t: Ty) -> TyKind {
        self.tys.kind(self.tys.shallow_resolve(t))
    }

    /// `t` with every bound variable replaced by its value.
    pub fn resolve_deep(&mut self, t: Ty) -> Result<Ty, Stop> {
        Ok(fold::resolve(self.tys, self.scratch, t)?)
    }

    pub fn arg(&self, l: List, i: u32) -> Ty {
        self.tys.list_get(l, i)
    }

    /// The terms of `l`, in a vector of the solver's allocator.
    pub fn list(&self, l: List) -> Vec<Ty, Alloc<'a>> {
        self.list_range(l, 0, l.len)
    }

    /// Elements `from..to` of `l` (clamped to its length). A list is a handle the `Tys` made,
    /// not a range to do arithmetic on: a sublist is read element by element and interned.
    pub fn list_range(&self, l: List, from: u32, to: u32) -> Vec<Ty, Alloc<'a>> {
        let to = to.min(l.len);
        let mut v = Vec::with_capacity_in(to.saturating_sub(from) as usize, self.alloc);
        for i in from..to {
            v.push(self.tys.list_get(l, i));
        }
        v
    }

    pub fn mk(&mut self, k: TyKind) -> Result<Ty, Stop> {
        Ok(self.tys.mk(k)?)
    }

    pub fn mk_list(&mut self, items: &[Ty]) -> Result<List, Stop> {
        Ok(self.tys.mk_list(items)?)
    }

    pub fn fresh(&mut self, kind: VarKind) -> Result<Ty, Stop> {
        Ok(self.tys.fresh(kind)?)
    }

    pub fn fresh_ty(&mut self) -> Result<Ty, Stop> {
        self.fresh(VarKind::Ty)
    }

    /// `n` fresh type variables as a list: the arguments of an impl or item being matched.
    pub fn fresh_args(&mut self, n: u32) -> Result<List, Stop> {
        let mut v: Vec<Ty, Alloc<'a>> = Vec::with_capacity_in(n as usize, self.alloc);
        for _ in 0..n {
            v.push(self.tys.fresh(VarKind::Ty)?);
        }
        self.mk_list(&v)
    }

    /// Fresh arguments for an impl being matched: a const variable for each const parameter
    /// (one the impl's own terms name as `ConstParam(i)`: `coherence::fresh_for`), a type
    /// variable for the rest, so a const position can only ever unify with a const
    /// (`fresh_args_for_item`).
    pub fn fresh_impl_args(&mut self, h: &ImplHeader<'_>) -> Result<List, Stop> {
        let v = crate::coherence::fresh_for(self.tys, h, self.alloc)?;
        self.mk_list(&v)
    }

    /// [`Cx::fresh_impl_args`] for the impl `impl_`, whose const parameters are found once per
    /// solver (coherence.rs `fresh_for_impl`).
    pub fn fresh_impl_args_of(&mut self, impl_: Def, h: &ImplHeader<'_>) -> Result<List, Stop> {
        let v = crate::coherence::fresh_for_impl(self.tys, self.scratch, impl_, h, self.alloc)?;
        self.mk_list(&v)
    }

    /// `n` placeholders of a new universe: a goal's bound variables when it is proven
    /// (`instantiate_binder_with_placeholders`).
    pub fn placeholders(&mut self, n: u32) -> Result<List, Stop> {
        *self.universe += 1;
        let u = *self.universe;
        let mut v: Vec<Ty, Alloc<'a>> = Vec::with_capacity_in(n as usize, self.alloc);
        for i in 0..n {
            v.push(self.tys.mk(TyKind::Placeholder(u, i))?);
        }
        self.mk_list(&v)
    }

    /// `t` with `Bound(0, i)` replaced by `with[i]` (and deeper binders' indices kept).
    pub fn instantiate_bound(&mut self, t: Ty, with: List) -> Result<Ty, Stop> {
        if !self.tys.flags(t).has(crate::term::Flags::HAS_BOUND) {
            return Ok(t);
        }
        Ok(fold::fold(self.tys, self.scratch, t, |tys, x| {
            Ok(match tys.kind(x) {
                TyKind::Bound(0, i) if i < with.len => Some(tys.list_get(with, i)),
                _ if !tys.flags(x).has(crate::term::Flags::HAS_BOUND) => Some(x),
                _ => None,
            })
        })?)
    }

    /// `p` with every term mapped by `f`.
    pub fn map_pred(&mut self, p: Pred, mut f: impl FnMut(&mut Self, Ty) -> Result<Ty, Stop>) -> Result<Pred, Stop> {
        Ok(match p {
            Pred::Trait { trait_, args, polarity } => {
                Pred::Trait { trait_, args: self.map_list(args, &mut f)?, polarity }
            }
            Pred::HostEffect { trait_, args, constness } => {
                Pred::HostEffect { trait_, args: self.map_list(args, &mut f)?, constness }
            }
            Pred::ConstHolds { cond, args } => Pred::ConstHolds { cond, args: self.map_list(args, &mut f)? },
            Pred::Projection { alias, term } => Pred::Projection { alias: f(self, alias)?, term: f(self, term)? },
            Pred::NormalizesTo { alias, term } => Pred::NormalizesTo { alias: f(self, alias)?, term: f(self, term)? },
            Pred::TypeOutlives { ty, region } => Pred::TypeOutlives { ty: f(self, ty)?, region: f(self, region)? },
            Pred::RegionOutlives { a, b } => Pred::RegionOutlives { a: f(self, a)?, b: f(self, b)? },
            Pred::WellFormed(t) => Pred::WellFormed(f(self, t)?),
            Pred::ConstEvaluatable(t) => Pred::ConstEvaluatable(f(self, t)?),
            Pred::Subtype { a, b } => Pred::Subtype { a: f(self, a)?, b: f(self, b)? },
            Pred::Coerce { a, b } => Pred::Coerce { a: f(self, a)?, b: f(self, b)? },
            Pred::ConstArgHasType { ct, ty } => Pred::ConstArgHasType { ct: f(self, ct)?, ty: f(self, ty)? },
            Pred::Eq { a, b } => Pred::Eq { a: f(self, a)?, b: f(self, b)? },
            Pred::DynCompatible(d) => Pred::DynCompatible(d),
        })
    }

    /// `l` with every term mapped by `f` (the same list when nothing changed).
    pub fn map_list(&mut self, l: List, f: &mut impl FnMut(&mut Self, Ty) -> Result<Ty, Stop>) -> Result<List, Stop> {
        let mut v = self.list(l);
        let mut same = true;
        for x in v.iter_mut() {
            let y = f(self, *x)?;
            same &= y == *x;
            *x = y;
        }
        if same { Ok(l) } else { self.mk_list(&v) }
    }

    /// The portable term `t` of `p` in the arena, `Param(i)` replaced by `args[i]`. A
    /// parameter past `args` is `Malformed::Param` naming the caller's line.
    #[track_caller]
    pub fn import(&mut self, p: Portable<'_>, t: Ty, args: List) -> Result<Ty, Stop> {
        let at = core::panic::Location::caller();
        let v = self.list(args);
        Ok(fold::import_at(self.tys, self.scratch, p, t, Inst { args: &v, bound: &[] }, at)?)
    }

    /// The portable list `l` of `p`, imported as `import` does each element.
    #[track_caller]
    pub fn import_list(&mut self, p: Portable<'_>, l: List, args: List) -> Result<List, Stop> {
        let site = core::panic::Location::caller();
        let v = self.list(args);
        let mut out: Vec<Ty, Alloc<'a>> = Vec::with_capacity_in(l.len as usize, self.alloc);
        for i in 0..l.len {
            let at = (l.start + i) as usize;
            let Some(&x) = p.lists.get(at) else {
                return Err(Stop::Error(SolveError::Malformed(crate::error::Malformed::PortableIndex {
                    node: at as u32,
                    len: p.lists.len() as u32,
                })));
            };
            out.push(fold::import_at(self.tys, self.scratch, p, x, Inst { args: &v, bound: &[] }, site)?);
        }
        self.mk_list(&out)
    }

    /// The portable predicate `pred` of `p`, imported.
    #[track_caller]
    pub fn import_pred(&mut self, p: Portable<'_>, pred: Pred, args: List) -> Result<Pred, Stop> {
        let at = core::panic::Location::caller();
        match pred {
            Pred::Trait { trait_, args: a, polarity } => {
                Ok(Pred::Trait { trait_, args: self.import_list(p, a, args)?, polarity })
            }
            Pred::HostEffect { trait_, args: a, constness } => {
                Ok(Pred::HostEffect { trait_, args: self.import_list(p, a, args)?, constness })
            }
            Pred::ConstHolds { cond, args: a } => Ok(Pred::ConstHolds { cond, args: self.import_list(p, a, args)? }),
            Pred::Eq { .. } => Err(Stop::Error(SolveError::Malformed(crate::error::bad_pred()))),
            other => {
                let v = self.list(args);
                self.map_pred(other, |cx, t| Ok(fold::import_at(cx.tys, cx.scratch, p, t, Inst { args: &v, bound: &[] }, at)?))
            }
        }
    }

    // Relating and adding goals.

    /// Relates `a` and `b` (the reference's `EvalCtxt::eq`, through rustc_type_ir
    /// relate/solver_relating.rs): a non-rigid alias met on one side becomes a nested
    /// `Projection { alias, term: fresh }` goal (`TypeRelating`) and `fresh` is related with the
    /// other side. `false`: they do not unify (nothing stays bound).
    pub fn eq(&mut self, a: Ty, b: Ty) -> Result<bool, Stop> {
        let base = self.deferred.len();
        if !self.tys.unify(a, b, crate::tys::Relate::Lazy, self.deferred)? {
            self.deferred.truncate(base);
            return Ok(false);
        }
        // Each deferred pair relates a fresh variable with the other side, which may defer
        // more pairs; every round removes one alias from the pair it relates, so the loop ends
        // within the size of the two terms.
        while self.deferred.len() > base {
            let Some((alias, other)) = self.deferred.pop() else { break };
            let kind = match self.tys.kind(alias) {
                TyKind::Alias(_, crate::term::AliasKind::Const, ..) => VarKind::Const,
                _ => VarKind::Ty,
            };
            let fresh = self.tys.fresh(kind)?;
            self.added.push(Added {
                source: GoalSource::TypeRelating,
                pred: Pred::Projection { alias, term: fresh },
                bound: 0,
                via_only: false,
            });
            if !self.tys.unify(fresh, other, crate::tys::Relate::Lazy, self.deferred)? {
                self.deferred.truncate(base);
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Relates `a` and `b` by structure, aliases included, deferring nothing (the relation the
    /// reference applies responses with: canonical/mod.rs `ResponseRelating`).
    pub fn eq_structurally(&mut self, a: Ty, b: Ty) -> Result<bool, Stop> {
        let base = self.deferred.len();
        let ok = self.tys.unify(a, b, crate::tys::Relate::Structural, self.deferred)?;
        self.deferred.truncate(base);
        Ok(ok)
    }

    /// Relates every pair of `a` and `b` (equal lengths).
    pub fn eq_lists(&mut self, a: List, b: List) -> Result<bool, Stop> {
        if a.len != b.len {
            return Ok(false);
        }
        for i in 0..a.len {
            let (x, y) = (self.arg(a, i), self.arg(b, i));
            if !self.eq(x, y)? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Adds a nested goal of the frame's param env.
    pub fn add(&mut self, source: GoalSource, pred: Pred) {
        self.added.push(Added { source, pred, bound: 0, via_only: false });
    }

    /// Adds a nested goal under `bound` bound variables.
    pub fn add_bound(&mut self, source: GoalSource, pred: Pred, bound: u32) {
        self.added.push(Added { source, pred, bound, via_only: false });
    }

    /// Adds a trait goal evaluated only to learn how it is proven: after `Step::Then`,
    /// [`Cx::via`] says (the reference computes the trait goal in a probe for its
    /// `TraitGoalProvenVia`).
    pub fn add_via(&mut self, pred: Pred) {
        self.added.push(Added { source: GoalSource::Misc, pred, bound: 0, via_only: true });
    }

    /// How the last `add_via` goal was proven.
    pub fn via(&self) -> Via {
        self.via
    }

    // Facts, each read recorded.

    pub fn trait_def(&mut self, t: Def) -> Result<TraitDef<'c>, Stop> {
        self.ask.trait_def(self.reads, t)
    }

    pub fn impls(&mut self, t: Def, shape: Shape) -> Result<ImplSet<'c>, Stop> {
        self.ask.impls(self.reads, t, shape)
    }

    pub fn impl_header(&mut self, i: Def) -> Result<ImplHeader<'c>, Stop> {
        self.ask.impl_header(self.reads, i)
    }

    /// Whether impl `a` specializes impl `b` (`Facts::specializes`).
    pub fn specializes(&mut self, a: Def, b: Def) -> Result<bool, Stop> {
        self.ask.specializes(self.reads, a, b)
    }

    pub fn adt(&mut self, d: Def) -> Result<AdtDef<'c>, Stop> {
        self.ask.adt(self.reads, d)
    }

    pub fn fn_sig(&mut self, d: Def) -> Result<FnSigDef<'c>, Stop> {
        self.ask.fn_sig(self.reads, d)
    }

    pub fn assoc(&mut self, d: Def) -> Result<AssocDef, Stop> {
        self.ask.assoc(self.reads, d)
    }

    pub fn opaque(&mut self, d: Def) -> Result<OpaqueDef<'c>, Stop> {
        self.ask.opaque(self.reads, d)
    }

    pub fn is_local(&mut self, d: Def) -> Result<bool, Stop> {
        self.ask.is_local(self.reads, d)
    }

    /// The `where const` condition `cond` at the ground `args`, whose terms `list` holds.
    pub fn const_holds(&mut self, cond: Def, list: List, args: &[Ty]) -> Result<crate::facts::ConstHolds, Stop> {
        self.ask.const_holds(self.reads, cond, list, args)
    }

    /// The lang item `l`'s def, if the crates in scope define it.
    pub fn lang(&mut self, l: Lang) -> Result<Option<Def>, Stop> {
        let i = l as usize;
        match self.langs.state[i] {
            Some(d) => {
                self.reads.push(Read { question: crate::facts::Question::Lang(l), digest: self.langs.digests[i] });
                Ok(d)
            }
            None => {
                let d = self.ask.lang(self.reads, l)?;
                self.langs.state[i] = Some(d);
                if let Some(r) = self.reads.last() {
                    self.langs.digests[i] = r.digest;
                }
                Ok(d)
            }
        }
    }

    /// Whether `d` is the lang item `l`.
    pub fn is_lang(&mut self, d: Def, l: Lang) -> Result<bool, Stop> {
        Ok(self.lang(l)? == Some(d))
    }
}

/// The result of one evaluated candidate: its row and its response.
#[derive(Clone, Copy, Debug)]
pub struct Done {
    pub cand: Cand,
    pub resp: crate::canon::RespId,
}

/// Whether a specializing candidate may constrain inference variables and still drop the
/// candidate it specializes (`AllowInferenceConstraints`): trait goals no, the others yes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InferenceConstraints {
    Allowed,
    NotAllowed,
}

/// `filter_specialized_impls` (assembly/mod.rs): outside coherence, an impl candidate is
/// dropped when another impl candidate's impl specializes its impl (RFC 1210: of the impls
/// that apply, the most specific is used). Under `NotAllowed` only a specializing candidate
/// whose response binds no variable drops one, so an undecided goal does not pick the
/// specializing impl by guessing. The question is asked only of two impl candidates of one
/// goal, so a goal with one impl candidate asks nothing.
pub fn filter_specialized_impls(
    m: &Merger<'_, '_>,
    mode: TypingMode,
    allow: InferenceConstraints,
    cands: &mut Vec<Done, Alloc<'_>>,
    specializes: &mut dyn FnMut(Def, Def) -> Result<bool, Stop>,
) -> Result<(), Stop> {
    if mode == TypingMode::Coherence {
        return Ok(());
    }
    let mut i = 0;
    'outer: while i < cands.len() {
        let Source::Impl(victim) = cands[i].cand.source else {
            i += 1;
            continue;
        };
        for j in 0..cands.len() {
            if i == j {
                continue;
            }
            let Source::Impl(other) = cands[j].cand.source else { continue };
            if (allow == InferenceConstraints::Allowed || m.vars_identity(cands[j].resp)) && specializes(other, victim)? {
                cands.remove(i);
                continue 'outer;
            }
        }
        i += 1;
    }
    Ok(())
}

/// Merges candidates' responses (the reference's `try_merge_candidates`, `bail_with_ambiguity`,
/// `flounder`): over the frame's response table.
pub struct Merger<'m, 'a> {
    pub(crate) resps: &'m mut crate::canon::Responses<'a>,
    /// The canonical input's variable count: the identity response's width.
    pub(crate) vars: u32,
}

/// `try_merge_candidates`'s second answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MergeInfo {
    AlwaysApplicable(usize),
    EqualResponse,
}

impl Merger<'_, '_> {
    pub fn certainty(&self, r: crate::canon::RespId) -> Certainty {
        self.resps.certainty(r)
    }

    /// No inference constraints and no nested goals (`has_no_inference_or_external_constraints`).
    pub fn no_constraints(&self, r: crate::canon::RespId) -> bool {
        self.resps.is_identity(r)
    }

    /// No inference constraints, nested goals aside (`has_only_region_constraints`).
    pub fn vars_identity(&self, r: crate::canon::RespId) -> bool {
        self.resps.vars_identity(r)
    }

    /// The same response, modulo how it was proven (`equal_response_modulo_region_constraints`).
    pub fn equal(&self, a: crate::canon::RespId, b: crate::canon::RespId) -> bool {
        self.resps.equal(a, b)
    }

    /// `try_merge_candidates` (solve/mod.rs).
    pub fn try_merge(&self, cands: &[Done]) -> Option<(crate::canon::RespId, MergeInfo)> {
        if cands.is_empty() {
            return None;
        }
        if let Some(i) =
            cands.iter().position(|c| self.certainty(c.resp) == Certainty::Yes && self.no_constraints(c.resp))
        {
            return Some((cands[i].resp, MergeInfo::AlwaysApplicable(i)));
        }
        let one = cands[0].resp;
        if cands[1..].iter().all(|c| self.equal(one, c.resp)) {
            return Some((one, MergeInfo::EqualResponse));
        }
        None
    }

    /// `bail_with_ambiguity` (solve/mod.rs): the `or` of the candidates' maybe-causes, from
    /// ambiguity, with no constraints.
    pub fn bail_with_ambiguity(&mut self, cands: &[Done]) -> Result<crate::canon::RespId, SolveError> {
        let mut c = Certainty::Ambiguous;
        for d in cands {
            let x = self.certainty(d.resp);
            if x != Certainty::Yes {
                c = c.or(x);
            }
        }
        self.resps.no_constraints(self.vars, c, Via::None)
    }

    /// `flounder`: no candidates is no solution, else ambiguity.
    pub fn flounder(&mut self, cands: &[Done]) -> Result<Option<crate::canon::RespId>, SolveError> {
        if cands.is_empty() { Ok(None) } else { Ok(Some(self.bail_with_ambiguity(cands)?)) }
    }

    /// The response `r` with how it was proven set to `via`.
    pub fn with_via(&mut self, r: crate::canon::RespId, via: Via) -> Result<crate::canon::RespId, SolveError> {
        self.resps.with_via(r, via)
    }

    /// A response with no constraints.
    pub fn no_constraints_resp(&mut self, c: Certainty) -> Result<crate::canon::RespId, SolveError> {
        self.resps.no_constraints(self.vars, c, Via::None)
    }

    /// `try_merge_responses` (solve/mod.rs), for goal kinds without a merge of their own: one
    /// response, or all equal, merges; else ambiguity.
    pub fn merge_responses(&mut self, cands: &[Done]) -> Result<Option<crate::canon::RespId>, SolveError> {
        match self.try_merge(cands) {
            Some((r, _)) => Ok(Some(r)),
            None => self.flounder(cands),
        }
    }
}
