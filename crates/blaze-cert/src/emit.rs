//! The emitter (certificates.md, "The emitter is the next piece"): the derivation of a goal a
//! recording solver (`Solver<'_, Derivations<'_>>`, blaze-solve witness.rs) proved, as the
//! bytes the checker reads.
//!
//! The solver records only its CHOICE per framed goal (the candidate its merge kept); this
//! module rebuilds each row from that choice with the checker's own rule function
//! ([`Checker::expect`]): the impl's arguments by matching its header against the goal, and
//! the row's nested goals as `expect` computes them. So a row's nested steps are what the
//! check compares, by construction, and every row is checked once as it is built (a solver
//! choice the certificate cannot express is a typed [`NotCertifiable`], never a row that fails
//! later). The rules and what each becomes:
//!
//! | solver rule | certificate |
//! |---|---|
//! | trait `Impl` | `Impl` |
//! | trait `Env(i)` | `Env` (a `MetaSized` goal by a `Sized` clause: refused by `expect`) |
//! | builtin `Sizedness`, `CopyClone` | `Builtin` Sized / MetaSized / CopyClone |
//! | builtin `Certain(Yes)` | `Builtin` Error, PointeeSized, Tuple, FnPtr, AnyType by lang; else unsupported |
//! | structural `Auto` | `Auto` |
//! | builtin `Object` | `Object` (the auto bound, or the principal itself; a supertrait: unsupported) |
//! | alias `Bound` (self bounds) on a trait goal | `AliasBound` |
//! | alias `Impl` on a projection's `NormalizesTo` | `Normalize` |
//! | everything else (FnTrait, AsyncFnTrait, TraitAlias, Unsize*, WellFormed, alias Env, Object, Rigid, builtin normalizations, coherence) | [`NotCertifiable::Unsupported`] |
//!
//! Cycles: a nested goal already open on the walk is a back-edge row closed with its ancestor
//! (`Builder::close_cycle`). The emitter never decides a cycle is allowed: the checker refuses
//! an inductive one (`InductiveCycle`), so it stays the only trust anchor.
//!
//! Inference: the root is taken through the final substitution when emitted; a variable left
//! is `NotCertifiable::Inference`. An impl parameter the goal does not fix is `Unconstrained`.
//!
//! Every walk is an explicit stack of at most [`MAX_EMIT_DEPTH`] frames; a certificate holds at
//! most [`MAX_EMIT_ROWS`] rows (rule 18).

use blaze_int::len32;
use blaze_own::{Alloc, Vec};
use blaze_solve::assembly::{Clause, EnvId, Goal, Rule, Source};
use blaze_solve::error::{SolveError, Stop};
use blaze_solve::eval::Solver;
use blaze_solve::facts::{Ask, Facts, Read};
use blaze_solve::fold::{self, Scratch};
use blaze_solve::term::{Certainty, Def, Flags, Lang, List, Pred, Ty, TyKind, TypingMode, VarKind};
use blaze_solve::tys::{Relate, Tys};
use blaze_solve::witness::Derivations;
use blaze_solve::{alias, builtin, coherence, structural, trait_goals};

use crate::check::{CheckError, Checker, Cost, RowRule, import_list};
use crate::derivation::{BuildError, Builder, Builtin, By, StepId};

pub const MAX_EMIT_ROWS: usize = 1 << 16;
pub const MAX_EMIT_DEPTH: usize = 1 << 10;

/// Why a proved goal gets no certificate. The whole certificate is refused: no partial one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NotCertifiable {
    /// The solver chose `rule` for `goal` and the certificate has no step for it.
    Unsupported { rule: Rule, goal: Pred },
    /// A nested goal neither a trait goal nor a projection (an outlives clause, say).
    Goal { goal: Pred },
    /// The goal holds an inference variable after the final substitution.
    Inference { goal: Pred },
    /// No `Yes` choice was recorded for a goal the derivation needs.
    NoChoice { goal: Pred },
    /// An impl parameter the goal does not fix.
    Unconstrained { goal: Pred, impl_: Def },
    /// The row the chosen rule gives does not check (the solver and the checker disagree).
    Refused(CheckError),
    Build(BuildError),
    Stop(Stop),
    TooMany { rows: usize },
    TooDeep { depth: usize },
}

impl NotCertifiable {
    pub const REASONS: [&'static str; 10] =
        ["unsupported", "goal", "inference", "no_choice", "unconstrained", "refused", "build", "stop", "too_many", "too_deep"];

    pub fn reason(&self) -> usize {
        match self {
            NotCertifiable::Unsupported { .. } => 0,
            NotCertifiable::Goal { .. } => 1,
            NotCertifiable::Inference { .. } => 2,
            NotCertifiable::NoChoice { .. } => 3,
            NotCertifiable::Unconstrained { .. } => 4,
            NotCertifiable::Refused(_) => 5,
            NotCertifiable::Build(_) => 6,
            NotCertifiable::Stop(_) => 7,
            NotCertifiable::TooMany { .. } => 8,
            NotCertifiable::TooDeep { .. } => 9,
        }
    }
}

fn se(e: SolveError) -> NotCertifiable {
    NotCertifiable::Stop(Stop::Error(e))
}

/// What the emitter did, counted: the count line.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Count {
    /// Goals certified.
    pub goals: u64,
    /// Rows written, by tag (certificates.md's order; 7 is the back-edge).
    pub steps: [u64; 8],
    /// Goals refused, by [`NotCertifiable::reason`].
    pub not_certifiable: [u64; 10],
}

const TAGS: [&str; 8] = ["impl", "env", "builtin", "auto", "object", "alias_bound", "normalize", "coinductive"];

impl core::fmt::Display for Count {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "certificates: goals={} steps:", self.goals)?;
        for (n, c) in TAGS.iter().zip(self.steps) {
            write!(f, " {n}={c}")?;
        }
        write!(f, " not_certifiable:")?;
        for (n, c) in NotCertifiable::REASONS.iter().zip(self.not_certifiable) {
            write!(f, " {n}={c}")?;
        }
        Ok(())
    }
}

fn tag(r: RowRule) -> usize {
    match r {
        RowRule::Impl(_) => 0,
        RowRule::Env(_) => 1,
        RowRule::Builtin(_) => 2,
        RowRule::Auto => 3,
        RowRule::Object(_) => 4,
        RowRule::AliasBound(_) => 5,
        RowRule::Normalize(_) => 6,
    }
}

/// An open row of the walk: its nested goals `kids[kids_lo..kids_hi]`, the next one `next`,
/// the rows written for those before it from `ids_lo`.
#[derive(Clone, Copy)]
struct Open {
    goal: Pred,
    rule: RowRule,
    args_lo: usize,
    args_hi: usize,
    kids_hi: usize,
    kids_lo: usize,
    next: usize,
    ids_lo: usize,
}

/// The emitter's reusable tables, in the allocator the caller names.
pub struct Emitter<'a> {
    alloc: Alloc<'a>,
    checker: Checker<'a>,
    scratch: Scratch<'a>,
    reads: Vec<Read, Alloc<'a>>,
    env: Vec<Clause, Alloc<'a>>,
    frames: Vec<Open, Alloc<'a>>,
    args: Vec<Ty, Alloc<'a>>,
    kids: Vec<Pred, Alloc<'a>>,
    ids: Vec<StepId, Alloc<'a>>,
    /// Rows written, by goal (sorted): a goal two rows need is one row.
    memo: Vec<(Pred, StepId), Alloc<'a>>,
    /// Back-edges waiting for their ancestor's row: (back-edge, the ancestor's frame index).
    backs: Vec<(StepId, usize), Alloc<'a>>,
    list: Vec<Ty, Alloc<'a>>,
    deferred: Vec<(Ty, Ty), Alloc<'a>>,
    count: Count,
}

impl<'a> Emitter<'a> {
    pub fn new_in(alloc: Alloc<'a>) -> Emitter<'a> {
        Emitter {
            alloc,
            checker: Checker::new_in(alloc),
            scratch: Scratch::new_in(alloc),
            reads: Vec::new_in(alloc),
            env: Vec::new_in(alloc),
            frames: Vec::new_in(alloc),
            args: Vec::new_in(alloc),
            kids: Vec::new_in(alloc),
            ids: Vec::new_in(alloc),
            memo: Vec::new_in(alloc),
            backs: Vec::new_in(alloc),
            list: Vec::new_in(alloc),
            deferred: Vec::new_in(alloc),
            count: Count::default(),
        }
    }

    pub fn count(&self) -> Count {
        self.count
    }

    /// Writes to `out` the certificate of `root`, which `solver` answered `Yes` in `env` and
    /// `mode`. The arena's bindings are as they were after (a snapshot is rolled back).
    pub fn emit<'s, T: Tys, F: Facts>(
        &mut self,
        solver: &mut Solver<'s, Derivations<'s>>,
        tys: &mut T,
        facts: &F,
        mode: TypingMode,
        env: EnvId,
        root: Pred,
        out: &mut Vec<u8, Alloc<'_>>,
    ) -> Result<(), NotCertifiable> {
        let snap = tys.snapshot();
        let mut steps = [0u64; 8];
        let r = self.run(solver, tys, facts, mode, env, root, out, &mut steps);
        tys.rollback_to(snap);
        match &r {
            Ok(()) => {
                self.count.goals += 1;
                for (c, s) in self.count.steps.iter_mut().zip(steps) {
                    *c += s;
                }
            }
            Err(e) => self.count.not_certifiable[e.reason()] += 1,
        }
        r
    }

    fn run<'s, T: Tys, F: Facts>(
        &mut self,
        solver: &mut Solver<'s, Derivations<'s>>,
        tys: &mut T,
        facts: &F,
        mode: TypingMode,
        env: EnvId,
        root: Pred,
        out: &mut Vec<u8, Alloc<'_>>,
        steps: &mut [u64; 8],
    ) -> Result<(), NotCertifiable> {
        self.frames.clear();
        self.args.clear();
        self.kids.clear();
        self.ids.clear();
        self.memo.clear();
        self.backs.clear();
        self.reads.clear();
        self.env.clear();
        self.env.extend_from_slice(solver.env_clauses(env));
        let ask = Ask::new(facts);
        let root = self.resolved(tys, root)?;
        let mut b = Builder::new_in(self.alloc);
        self.open(solver, tys, &ask, mode, env, root, b.rows())?;
        while let Some(top) = self.frames.last().copied() {
            if top.next < top.kids_hi {
                let g = self.kids[top.next];
                if let Some(f) = self.frames.last_mut() {
                    f.next += 1;
                }
                if let Ok(i) = self.memo.binary_search_by(|(p, _)| p.cmp(&g)) {
                    self.ids.push(self.memo[i].1);
                } else if let Some(j) = self.frames.iter().position(|f| f.goal == g) {
                    let s = b.back_edge(&*tys, g).map_err(NotCertifiable::Build)?;
                    steps[7] += 1;
                    self.backs.push((s, j));
                    self.ids.push(s);
                } else {
                    if self.frames.len() >= MAX_EMIT_DEPTH {
                        return Err(NotCertifiable::TooDeep { depth: self.frames.len() });
                    }
                    self.open(solver, tys, &ask, mode, env, g, b.rows())?;
                }
                continue;
            }
            // Every nested step is written: this row.
            self.frames.pop();
            let args = &self.args[top.args_lo..top.args_hi];
            let by = match top.rule {
                RowRule::Impl(impl_) => By::Impl { impl_, args },
                RowRule::Normalize(impl_) => By::Normalize { impl_, args },
                RowRule::Env(clause) => By::Env { clause },
                RowRule::Builtin(r) => By::Builtin(r),
                RowRule::Auto => By::Auto,
                RowRule::Object(bound) => By::Object { bound },
                RowRule::AliasBound(bound) => By::AliasBound { bound },
            };
            let s = b.push(&*tys, top.goal, by, &self.ids[top.ids_lo..]).map_err(NotCertifiable::Build)?;
            steps[tag(top.rule)] += 1;
            if b.rows() > MAX_EMIT_ROWS {
                return Err(NotCertifiable::TooMany { rows: b.rows() });
            }
            self.ids.truncate(top.ids_lo);
            self.args.truncate(top.args_lo);
            self.kids.truncate(top.kids_lo);
            let depth = self.frames.len();
            let mut i = 0;
            while i < self.backs.len() {
                if self.backs[i].1 == depth {
                    b.close_cycle(self.backs[i].0, s).map_err(NotCertifiable::Build)?;
                    self.backs.swap_remove(i);
                } else {
                    i += 1;
                }
            }
            if let Err(at) = self.memo.binary_search_by(|(p, _)| p.cmp(&top.goal)) {
                self.memo.insert(at, (top.goal, s));
            }
            self.ids.push(s);
        }
        b.finish(out).map_err(NotCertifiable::Build)
    }

    /// `goal` through the final substitution; a variable left refuses it.
    fn resolved<T: Tys>(&mut self, tys: &mut T, goal: Pred) -> Result<Pred, NotCertifiable> {
        let (p, terms) = match goal {
            Pred::Trait { trait_, args, polarity } => {
                self.list.clear();
                for i in 0..args.len {
                    let t = tys.list_get(args, i);
                    let t = fold::resolve(tys, &mut self.scratch, t).map_err(se)?;
                    self.list.push(t);
                }
                let args = tys.mk_list(&self.list).map_err(se)?;
                (Pred::Trait { trait_, args, polarity }, args)
            }
            Pred::Projection { alias, term } => {
                let alias = fold::resolve(tys, &mut self.scratch, alias).map_err(se)?;
                let term = fold::resolve(tys, &mut self.scratch, term).map_err(se)?;
                let both = tys.mk_list(&[alias, term]).map_err(se)?;
                (Pred::Projection { alias, term }, both)
            }
            _ => return Err(NotCertifiable::Goal { goal }),
        };
        for i in 0..terms.len {
            if tys.flags(tys.list_get(terms, i)).has(Flags::HAS_INFER) {
                return Err(NotCertifiable::Inference { goal: p });
            }
        }
        Ok(p)
    }

    /// Opens the row of `goal`: its rule from the solver's choice, its arguments by matching,
    /// its nested goals by the checker's rule.
    fn open<'s, T: Tys, F: Facts>(
        &mut self,
        solver: &mut Solver<'s, Derivations<'s>>,
        tys: &mut T,
        ask: &Ask<'_, F>,
        mode: TypingMode,
        env: EnvId,
        goal: Pred,
        row: usize,
    ) -> Result<(), NotCertifiable> {
        let step = StepId(len32(row).map_err(|_| NotCertifiable::TooMany { rows: row })?);
        let rule = self.rule_of(solver, tys, ask, mode, env, goal)?;
        let args_lo = self.args.len();
        match (rule, goal) {
            (RowRule::Impl(d), Pred::Trait { args, .. }) => self.match_header(tys, ask, goal, step, d, args)?,
            (RowRule::Normalize(d), Pred::Projection { alias, .. }) => {
                let TyKind::Alias(_, _, _, aargs) = tys.kind(tys.shallow_resolve(alias)) else {
                    return Err(NotCertifiable::Refused(CheckError::NotProjection { step }));
                };
                self.match_header(tys, ask, goal, step, d, aargs)?;
            }
            _ => {}
        }
        let args_hi = self.args.len();
        self.checker.set_terms(&self.args[args_lo..args_hi]);
        let mut cost = Cost::default();
        self.checker.expect(tys, ask, &mut self.reads, &self.env, step, goal, rule, &mut cost).map_err(NotCertifiable::Refused)?;
        let kids_lo = self.kids.len();
        self.kids.extend_from_slice(self.checker.expected());
        let o = Open { goal, rule, args_lo, args_hi, kids_lo, kids_hi: self.kids.len(), next: kids_lo, ids_lo: self.ids.len() };
        self.frames.push(o);
        Ok(())
    }

    /// The impl's arguments: its header's trait arguments, over fresh variables, related by
    /// structure with `want` (the goal's arguments, or the alias's: its parent's come first),
    /// then resolved; pushed to `self.args`.
    fn match_header<T: Tys, F: Facts>(
        &mut self,
        tys: &mut T,
        ask: &Ask<'_, F>,
        goal: Pred,
        step: StepId,
        impl_: Def,
        want: List,
    ) -> Result<(), NotCertifiable> {
        let h = ask.impl_header(&mut self.reads, impl_).map_err(NotCertifiable::Stop)?;
        let vars = coherence::fresh_for(tys, &mut self.scratch, &h, self.alloc).map_err(se)?;
        let header = import_list(tys, &mut self.scratch, h.portable, h.args, &vars, &mut self.list).map_err(se)?;
        let mismatch = NotCertifiable::Refused(CheckError::HeaderMismatch { step, impl_ });
        if header.len > want.len {
            return Err(mismatch);
        }
        for i in 0..header.len {
            let (x, y) = (tys.list_get(header, i), tys.list_get(want, i));
            self.deferred.clear();
            if !tys.unify(x, y, Relate::Structural, &mut self.deferred).map_err(se)? {
                return Err(mismatch);
            }
        }
        for &v in vars.iter() {
            let t = fold::resolve(tys, &mut self.scratch, v).map_err(se)?;
            if tys.flags(t).has(Flags::HAS_INFER) {
                return Err(NotCertifiable::Unconstrained { goal, impl_ });
            }
            self.args.push(t);
        }
        Ok(())
    }

    /// The row rule of the solver's choice for `goal` (the module's table).
    fn rule_of<'s, T: Tys, F: Facts>(
        &mut self,
        solver: &mut Solver<'s, Derivations<'s>>,
        tys: &mut T,
        ask: &Ask<'_, F>,
        mode: TypingMode,
        env: EnvId,
        goal: Pred,
    ) -> Result<RowRule, NotCertifiable> {
        match goal {
            Pred::Trait { trait_, args, .. } => {
                let c = solver.choice(&*tys, mode, Goal { env, pred: goal, bound: 0 }).map_err(NotCertifiable::Stop)?;
                let c = c.ok_or(NotCertifiable::NoChoice { goal })?;
                let un = NotCertifiable::Unsupported { rule: c.rule, goal };
                Ok(match (c.source, c.rule) {
                    (Source::Impl(d), Rule::Trait(trait_goals::Rule::Impl)) => RowRule::Impl(d),
                    (_, Rule::Trait(trait_goals::Rule::Env(i))) => RowRule::Env(i),
                    (_, Rule::Builtin(builtin::Rule::Sizedness { meta })) => {
                        RowRule::Builtin(if meta { Builtin::MetaSized } else { Builtin::Sized })
                    }
                    (_, Rule::Builtin(builtin::Rule::CopyClone)) => RowRule::Builtin(Builtin::CopyClone),
                    (_, Rule::Structural(structural::Rule::Auto)) => RowRule::Auto,
                    (_, Rule::Builtin(builtin::Rule::Object)) => RowRule::Object(object_bound(&*tys, trait_, args).ok_or(un)?),
                    (Source::AliasBound { self_bounds: true }, Rule::Alias(alias::Rule::Bound { .. })) => RowRule::AliasBound(c.n),
                    (Source::Builtin(_), Rule::Certain(Certainty::Yes)) => {
                        RowRule::Builtin(self.certain_rule(&*tys, ask, trait_, args)?.ok_or(un)?)
                    }
                    _ => return Err(un),
                })
            }
            Pred::Projection { alias, .. } => {
                // The solver proved the projection through `NormalizesTo { alias, ?term }`: the
                // same canonical input, whatever the variable.
                let v = tys.fresh(VarKind::Ty).map_err(se)?;
                let q = Goal { env, pred: Pred::NormalizesTo { alias, term: v }, bound: 0 };
                let c = solver.choice(&*tys, mode, q).map_err(NotCertifiable::Stop)?;
                let c = c.ok_or(NotCertifiable::NoChoice { goal })?;
                match (c.source, c.rule) {
                    (Source::Impl(d), Rule::Alias(alias::Rule::Impl)) => Ok(RowRule::Normalize(d)),
                    _ => Err(NotCertifiable::Unsupported { rule: c.rule, goal }),
                }
            }
            _ => Err(NotCertifiable::Goal { goal }),
        }
    }

    /// A builtin candidate that holds with no step (`certain`): which certificate rule, by the
    /// self type and the trait's lang item.
    fn certain_rule<T: Tys, F: Facts>(&mut self, tys: &T, ask: &Ask<'_, F>, trait_: Def, args: List) -> Result<Option<Builtin>, NotCertifiable> {
        if args.len == 0 {
            return Ok(None);
        }
        if matches!(tys.kind(tys.shallow_resolve(tys.list_get(args, 0))), TyKind::Error) {
            return Ok(Some(Builtin::Error));
        }
        let td = ask.trait_def(&mut self.reads, trait_).map_err(NotCertifiable::Stop)?;
        Ok(match td.lang {
            Some(Lang::PointeeSized) => Some(Builtin::PointeeSized),
            Some(Lang::Tuple) => Some(Builtin::Tuple),
            Some(Lang::FnPtrTrait) => Some(Builtin::FnPtr),
            Some(Lang::Pointee | Lang::DiscriminantKind | Lang::Destruct) => Some(Builtin::AnyType),
            _ => None,
        })
    }
}

/// The index of the `dyn` self type's bound the object candidate used: `+ Trait` for the goal's
/// trait, or the principal (index 0) when it is the goal's trait itself. A supertrait of the
/// principal has no certificate step.
fn object_bound<T: Tys>(tys: &T, trait_: Def, args: List) -> Option<u32> {
    if args.len == 0 {
        return None;
    }
    let TyKind::Dynamic(preds, _) = tys.kind(tys.shallow_resolve(tys.list_get(args, 0))) else { return None };
    for i in 0..preds.len {
        match tys.kind(tys.list_get(preds, i)) {
            TyKind::ExAuto(d) if d == trait_ => return Some(i),
            TyKind::ExTrait(d, _) if d == trait_ && i == 0 => return Some(0),
            _ => {}
        }
    }
    None
}
