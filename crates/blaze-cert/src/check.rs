//! The checker (certificates.md, "The checker"): proves a derivation against today's facts by
//! MATCHING ONLY. It never enumerates candidates, never binds an inference variable, never
//! backtracks: each row's goal is decoded once, each impl header named is instantiated once with
//! the arguments the row gives, and every comparison is structural equality of terms with
//! regions equal to any region (the solver does not relate regions: solve.md, "Not ported").
//! No normalisation is applied implicitly: where the solver normalised, the derivation says so
//! with a `Normalize` step, and a goal compared with a clause must equal it as written.
//!
//! # The invariant (rule 53)
//!
//! A certificate is VALID TODAY iff every question in its list answers as recorded
//! ([`crate::Questions::moved`] is `None`) AND its derivation checks against today's facts
//! ([`Checker::check`] returns [`Checked`]).
//!
//! The claim the tests raise evidence for (src/tests.rs): valid today implies the solver, run
//! today, returns the same response for the root goal. A derivation that checks proves "these
//! steps apply", which is the positive half; the negative half (that no other candidate applies
//! where the language requires one, or that the candidate the solver prefers is still this one)
//! is the question list's: every read of the solve except the content this check makes again.
//!
//! The cost is linear in the rows plus the sizes of the headers instantiated and the terms
//! compared; [`Cost`] counts each. Cycles: a back-edge from row `s` to its ancestor `t` (a later
//! row) is allowed exactly where the solver's path kind is coinductive: every row in `(s, t]`
//! (a superset of the cycle's goals, since nested references only go to earlier rows) is a
//! goal of an auto or coinductive trait or a projection, and at least one is a trait's.

use blaze_int::{Int, idx, len32};
use blaze_own::{Alloc, Vec};
use blaze_solve::SolveError;
use blaze_solve::assembly::Clause;
use blaze_solve::canon::Decode;
use blaze_solve::error::{Limit, Malformed, Stop};
use blaze_solve::facts::{AdtFlags, Ask, Facts, ImplPolarity, Portable, Question, Read, TraitFlags};
use blaze_solve::fold::{self, Inst, Scratch};
use blaze_solve::term::{AliasKind, Children, Def, Lang, List, Polarity, Pred, Rigid, Ty, TyKind, UintTy, Var, VarKind, children, is_region};
use blaze_solve::tys::Tys;

use crate::derivation::{Builtin, Step, StepId, View};

/// What one check did, counted.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Cost {
    /// Rows checked.
    pub rows: u64,
    /// Headers instantiated: impl headers, ADT field lists, trait and alias bound lists.
    pub headers: u64,
    /// Term node pairs compared.
    pub nodes_compared: u64,
    /// Canonical words decoded (goals and arguments).
    pub words_decoded: u64,
}

/// Why a derivation does not check today. Each names the step (row) it is about; a failed
/// check is a value, never a panic.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CheckError {
    /// The row's goal words do not decode.
    Goal { step: StepId, error: SolveError },
    /// The goal words decode to something other than exactly one predicate.
    GoalWords { step: StepId },
    /// The argument words do not decode, or hold a predicate.
    ArgWords { step: StepId },
    /// The step proves trait goals and the goal is not one (or has no self type).
    NotTrait { step: StepId },
    /// A builtin, auto, object or alias-bound step on a negative goal.
    Negative { step: StepId },
    /// The step proves projections and the goal is not `<.. as Trait>::A == U`.
    NotProjection { step: StepId },
    /// The impl is of another trait, or its header instantiated with the row's arguments is
    /// not the goal.
    HeaderMismatch { step: StepId, impl_: Def },
    Polarity { step: StepId, impl_: Def },
    /// The world has no such impl today.
    UnknownImpl { step: StepId, impl_: Def },
    ArgCount { step: StepId, want: u32, have: usize },
    NestedCount { step: StepId, want: usize, have: u32 },
    /// The nested step at position `clause` does not prove that clause.
    NestedMismatch { step: StepId, clause: u32 },
    EnvIndex { step: StepId, index: u32, len: usize },
    /// The environment's clause does not hold the goal.
    EnvMismatch { step: StepId, index: u32 },
    /// A clause under `for<..>` (not checked by matching yet).
    HigherRanked { step: StepId },
    /// The builtin rule does not give this goal.
    Builtin { step: StepId, rule: Builtin },
    /// The trait is not auto, or the self type has no structural auto impl.
    NotAuto { step: StepId },
    ObjectBound { step: StepId, index: u32 },
    /// A `dyn` with projection bounds (the solver's replacement of projections is not checked
    /// yet): refused, never accepted.
    ObjectProjections { step: StepId },
    AliasBound { step: StepId, index: u32 },
    /// The associated item is missing, of another trait, or a `default` (specializable) value.
    Assoc { step: StepId, item: Def },
    /// The impl's value for the projection is not the goal's term.
    Projection { step: StepId },
    /// A back-edge whose goal is not its target's.
    BackEdgeGoal { step: StepId, target: StepId },
    /// A back-edge that closes a cycle the language does not allow (an inductive one).
    InductiveCycle { step: StepId },
    /// A fact the world lacks today.
    Need { step: StepId, question: Question },
    /// The solver's own failure while instantiating (a bound, malformed input).
    Solve { step: StepId, error: SolveError },
}

impl core::fmt::Display for CheckError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "blaze-cert: the derivation does not check: {self:?}")
    }
}

impl core::error::Error for CheckError {}

/// The proof that a derivation checked against the facts it was handed (rule 74): the only
/// thing a later consumer (the question list, a cache that keeps certificates) accepts. Only
/// [`Checker::check`] makes one.
#[derive(Debug)]
pub struct Checked<'d, 'a> {
    view: View<'d>,
    reads: Vec<Read, Alloc<'a>>,
    cost: Cost,
}

impl<'d, 'a> Checked<'d, 'a> {
    pub fn view(&self) -> View<'d> {
        self.view
    }

    /// The reads the check made, through the solver's recorder: the content it rests on.
    pub fn reads(&self) -> &[Read] {
        &self.reads
    }

    pub fn cost(&self) -> Cost {
        self.cost
    }
}

/// The scratch tables a step uses.
struct Work<'a> {
    scratch: Scratch<'a>,
    list: Vec<Ty, Alloc<'a>>,
    cons: Vec<Ty, Alloc<'a>>,
    pairs: Vec<(Ty, Ty), Alloc<'a>>,
}

/// The checker's reusable state, in the allocator the caller names.
pub struct Checker<'a> {
    alloc: Alloc<'a>,
    /// One decoder for the whole certificate: canonical variable `i` is one fresh variable in
    /// every row.
    decode: Decode<'a>,
    words: Vec<u32, Alloc<'a>>,
    terms: Vec<Ty, Alloc<'a>>,
    goals: Vec<Pred, Alloc<'a>>,
    expected: Vec<Pred, Alloc<'a>>,
    /// Per row: 1 a projection, 2 an auto or coinductive trait's goal, 3 anything else.
    class: Vec<u8, Alloc<'a>>,
    forced: Vec<u32, Alloc<'a>>,
    coinductive: Vec<u32, Alloc<'a>>,
    work: Work<'a>,
}

const PROJECTION: u8 = 1;
const COINDUCTIVE: u8 = 2;
const FORCED: u8 = 3;

fn stop(step: StepId, s: Stop) -> CheckError {
    match s {
        Stop::Need(Question::ImplHeader(i)) => CheckError::UnknownImpl { step, impl_: i },
        Stop::Need(question) => CheckError::Need { step, question },
        Stop::Error(error) => CheckError::Solve { step, error },
    }
}

fn solve(step: StepId) -> impl Fn(SolveError) -> CheckError {
    move |error| CheckError::Solve { step, error }
}

/// A row's rule as the checker and the emitter share it ([`Checker::expect`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RowRule {
    Impl(Def),
    Normalize(Def),
    Env(u32),
    Builtin(Builtin),
    Auto,
    Object(u32),
    AliasBound(u32),
}


impl<'a> Checker<'a> {
    pub fn new_in(alloc: Alloc<'a>) -> Checker<'a> {
        Checker {
            alloc,
            decode: Decode::new_in(alloc),
            words: Vec::new_in(alloc),
            terms: Vec::new_in(alloc),
            goals: Vec::new_in(alloc),
            expected: Vec::new_in(alloc),
            class: Vec::new_in(alloc),
            forced: Vec::new_in(alloc),
            coinductive: Vec::new_in(alloc),
            work: Work { scratch: Scratch::new_in(alloc), list: Vec::new_in(alloc), cons: Vec::new_in(alloc), pairs: Vec::new_in(alloc) },
        }
    }

    /// Checks `view` against `facts` and the elaborated environment `env` (the clauses an
    /// `Env` step's index names; the certificate is keyed by it). The goals are decoded into
    /// `tys`; a caller that wants the arena back takes a snapshot first. Every fact is read
    /// through the solver's recorder, so the check's reads are recorded as the solver's are.
    pub fn check<'d, T: Tys, F: Facts>(&mut self, view: View<'d>, tys: &mut T, facts: &F, env: &[Clause]) -> Result<Checked<'d, 'a>, CheckError> {
        let ask = Ask::new(facts);
        let mut reads: Vec<Read, Alloc<'a>> = Vec::new_in(self.alloc);
        let mut cost = Cost::default();
        // A fresh numbering: the last check's variables were of its own arena.
        self.decode.reset();
        self.goals.clear();
        self.class.clear();
        for row in 0..view.rows() {
            let step = StepId(row);
            cost.rows += 1;
            let goal = self.decode_goal(tys, view, step, &mut cost)?;
            self.goals.push(goal);
            let class = class_of(&ask, &mut reads, goal).map_err(|s| stop(step, s))?;
            self.class.push(class);
            let rule = match view.step(step) {
                // Checked once every row is decoded: its target is a later row.
                Step::Coinductive { .. } => continue,
                Step::Impl { impl_, args } => {
                    self.decode_args(tys, args, step, &mut cost)?;
                    RowRule::Impl(impl_)
                }
                Step::Normalize { impl_, args } => {
                    self.decode_args(tys, args, step, &mut cost)?;
                    RowRule::Normalize(impl_)
                }
                Step::Env { clause } => RowRule::Env(clause),
                Step::Builtin(rule) => RowRule::Builtin(rule),
                Step::Auto => RowRule::Auto,
                Step::Object { bound } => RowRule::Object(bound),
                Step::AliasBound { bound } => RowRule::AliasBound(bound),
            };
            self.expect(tys, &ask, &mut reads, env, step, goal, rule, &mut cost)?;
            self.nested_are(tys, view, step, &mut cost)?;
        }
        self.cycles(tys, view, &mut cost)?;
        Ok(Checked { view, reads, cost })
    }

    /// The row `step` proving `goal` by `rule` applies, and `self.expected` is its nested goals
    /// in order. `Impl` and `Normalize` take their arguments from `self.terms`. The one home of
    /// each rule's check: the checker runs it per row, the emitter (emit.rs) runs it to build
    /// the row, so a built row's nested steps are what the check will compare by construction.
    pub(crate) fn expect<T: Tys, F: Facts>(
        &mut self,
        tys: &mut T,
        ask: &Ask<'_, F>,
        reads: &mut Vec<Read, Alloc<'a>>,
        env: &[Clause],
        step: StepId,
        goal: Pred,
        rule: RowRule,
        cost: &mut Cost,
    ) -> Result<(), CheckError> {
        self.expected.clear();
        match rule {
            RowRule::Impl(impl_) => {
                let Pred::Trait { trait_, args: gargs, polarity } = goal else { return Err(CheckError::NotTrait { step }) };
                let h = ask.impl_header(reads, impl_).map_err(|s| stop(step, s))?;
                cost.headers += 1;
                if h.trait_ != trait_ {
                    return Err(CheckError::HeaderMismatch { step, impl_ });
                }
                if !matches!((h.polarity, polarity), (ImplPolarity::Positive, Polarity::Positive) | (ImplPolarity::Negative, Polarity::Negative)) {
                    return Err(CheckError::Polarity { step, impl_ });
                }
                if len32(self.terms.len()) != Ok(h.generics) {
                    return Err(CheckError::ArgCount { step, want: h.generics, have: self.terms.len() });
                }
                let w = &mut self.work;
                let header = import_list(tys, &mut w.scratch, h.portable, h.args, &self.terms, &mut w.list).map_err(solve(step))?;
                if !same_list(tys, header, gargs, &mut w.pairs, cost).map_err(solve(step))? {
                    return Err(CheckError::HeaderMismatch { step, impl_ });
                }
                for &p in h.where_clauses {
                    let p = import_pred(tys, &mut w.scratch, h.portable, p, &self.terms, &mut w.list).map_err(solve(step))?;
                    self.expected.push(p);
                }
            }
            RowRule::Normalize(impl_) => self.by_normalize(tys, ask, reads, step, goal, impl_, cost)?,
            RowRule::Env(clause) => {
                let Some(c) = env.get(idx(clause)) else { return Err(CheckError::EnvIndex { step, index: clause, len: env.len() }) };
                if c.bound != 0 {
                    return Err(CheckError::HigherRanked { step });
                }
                if !same_pred(tys, c.pred, goal, &mut self.work.pairs, cost).map_err(solve(step))? {
                    return Err(CheckError::EnvMismatch { step, index: clause });
                }
            }
            RowRule::Builtin(rule) => self.by_builtin(tys, ask, reads, step, goal, rule, cost)?,
            RowRule::Auto => self.by_auto(tys, ask, reads, step, goal, cost)?,
            RowRule::Object(bound) => self.by_object(tys, ask, reads, step, goal, bound, cost)?,
            RowRule::AliasBound(bound) => self.by_alias_bound(tys, ask, reads, step, goal, bound, cost)?,
        }
        Ok(())
    }

    /// The arguments [`Checker::expect`] reads for an `Impl` or `Normalize` row.
    pub(crate) fn set_terms(&mut self, args: &[Ty]) {
        self.terms.clear();
        self.terms.extend_from_slice(args);
    }

    /// The nested goals the last [`Checker::expect`] gave, in order.
    pub(crate) fn expected(&self) -> &[Pred] {
        &self.expected
    }

    fn decode_goal<T: Tys>(&mut self, tys: &mut T, view: View<'_>, step: StepId, cost: &mut Cost) -> Result<Pred, CheckError> {
        self.words.clear();
        view.goal(step).copy_into(&mut self.words);
        cost.words_decoded += self.words.len().widen_to::<u64>();
        let p = self.decode.run(tys, &self.words).map_err(|error| CheckError::Goal { step, error })?;
        let mut extra = false;
        while self.decode.take_ty().is_some() {
            extra = true;
        }
        match p {
            Some(p) if !extra => Ok(p),
            _ => Err(CheckError::GoalWords { step }),
        }
    }

    /// The row's argument terms into `self.terms`, in order.
    fn decode_args<T: Tys>(&mut self, tys: &mut T, args: crate::derivation::Words<'_>, step: StepId, cost: &mut Cost) -> Result<(), CheckError> {
        self.words.clear();
        args.copy_into(&mut self.words);
        cost.words_decoded += self.words.len().widen_to::<u64>();
        let p = self.decode.run(tys, &self.words).map_err(|error| CheckError::Goal { step, error })?;
        self.terms.clear();
        while let Some(t) = self.decode.take_ty() {
            self.terms.push(t);
        }
        self.terms.reverse();
        if p.is_some() {
            return Err(CheckError::ArgWords { step });
        }
        Ok(())
    }

    /// The row's nested steps are exactly `self.expected`, in order.
    fn nested_are<T: Tys>(&mut self, tys: &T, view: View<'_>, step: StepId, cost: &mut Cost) -> Result<(), CheckError> {
        let have = view.nested_len(step);
        if len32(self.expected.len()) != Ok(have) {
            return Err(CheckError::NestedCount { step, want: self.expected.len(), have });
        }
        for (clause, child) in (0u32..).zip(view.nested(step)) {
            // `child < step` (View::read): decoded already.
            let got = self.goals[idx(child.0)];
            let want = self.expected[idx(clause)];
            if !same_pred(tys, want, got, &mut self.work.pairs, cost).map_err(solve(step))? {
                return Err(CheckError::NestedMismatch { step, clause });
            }
        }
        Ok(())
    }

    /// Every back-edge: its goal is its target's, and the rows between are coinductive (the
    /// module doc). Prefix counts make each one O(1).
    fn cycles<T: Tys>(&mut self, tys: &T, view: View<'_>, cost: &mut Cost) -> Result<(), CheckError> {
        self.forced.clear();
        self.coinductive.clear();
        let (mut f, mut c) = (0u32, 0u32);
        self.forced.push(0);
        self.coinductive.push(0);
        for &k in self.class.iter() {
            f += u32::from(k == FORCED);
            c += u32::from(k == COINDUCTIVE);
            self.forced.push(f);
            self.coinductive.push(c);
        }
        for row in 0..view.rows() {
            let step = StepId(row);
            let Step::Coinductive { target } = view.step(step) else { continue };
            let (g, t) = (self.goals[idx(row)], self.goals[idx(target.0)]);
            if !same_pred(tys, g, t, &mut self.work.pairs, cost).map_err(solve(step))? {
                return Err(CheckError::BackEdgeGoal { step, target });
            }
            let (lo, hi) = (idx(row) + 1, idx(target.0) + 1);
            if self.forced[hi] != self.forced[lo] || self.coinductive[hi] == self.coinductive[lo] {
                return Err(CheckError::InductiveCycle { step });
            }
        }
        Ok(())
    }

    fn by_normalize<T: Tys, F: Facts>(
        &mut self,
        tys: &mut T,
        ask: &Ask<'_, F>,
        reads: &mut Vec<Read, Alloc<'a>>,
        step: StepId,
        goal: Pred,
        impl_: Def,
        cost: &mut Cost,
    ) -> Result<(), CheckError> {
        let (alias, term) = match goal {
            Pred::Projection { alias, term } | Pred::NormalizesTo { alias, term } => (alias, term),
            _ => return Err(CheckError::NotProjection { step }),
        };
        let TyKind::Alias(_, AliasKind::Projection, item, aargs) = tys.kind(tys.shallow_resolve(alias)) else {
            return Err(CheckError::NotProjection { step });
        };
        let ad = ask.assoc(reads, item).map_err(|s| stop(step, s))?;
        let Some(tr) = ad.trait_ else { return Err(CheckError::Assoc { step, item }) };
        let h = ask.impl_header(reads, impl_).map_err(|s| stop(step, s))?;
        cost.headers += 1;
        if h.trait_ != tr {
            return Err(CheckError::HeaderMismatch { step, impl_ });
        }
        if h.polarity != ImplPolarity::Positive {
            return Err(CheckError::Polarity { step, impl_ });
        }
        if len32(self.terms.len()) != Ok(h.generics) {
            return Err(CheckError::ArgCount { step, want: h.generics, have: self.terms.len() });
        }
        if ad.parent_generics.checked_add(ad.own_generics) != Some(aargs.len) {
            return Err(CheckError::Assoc { step, item });
        }
        let w = &mut self.work;
        let header = import_list(tys, &mut w.scratch, h.portable, h.args, &self.terms, &mut w.list).map_err(solve(step))?;
        if header.len != ad.parent_generics {
            return Err(CheckError::HeaderMismatch { step, impl_ });
        }
        for i in 0..header.len {
            let (x, y) = (tys.list_get(header, i), tys.list_get(aargs, i));
            if !same(tys, x, y, &mut w.pairs, cost).map_err(solve(step))? {
                return Err(CheckError::HeaderMismatch { step, impl_ });
            }
        }
        let Some(ia) = h.assoc.iter().find(|a| a.trait_item == item) else { return Err(CheckError::Assoc { step, item }) };
        if ia.is_default || ia.own_generics != ad.own_generics {
            return Err(CheckError::Assoc { step, item });
        }
        // The value is over the impl's generics, then the item's own (the alias's tail).
        w.cons.clear();
        w.cons.extend_from_slice(&self.terms);
        for i in ad.parent_generics..aargs.len {
            w.cons.push(tys.list_get(aargs, i));
        }
        let value = fold::import(tys, &mut w.scratch, h.portable, ia.value, Inst { args: &w.cons, bound: &[] }).map_err(solve(step))?;
        if !same(tys, value, term, &mut w.pairs, cost).map_err(solve(step))? {
            return Err(CheckError::Projection { step });
        }
        for &p in h.where_clauses {
            let p = import_pred(tys, &mut w.scratch, h.portable, p, &self.terms, &mut w.list).map_err(solve(step))?;
            self.expected.push(p);
        }
        Ok(())
    }

    /// The expected nested goals `trait_(t)` for each constituent in `self.work.cons`.
    fn constituent_goals<T: Tys>(&mut self, tys: &mut T, step: StepId, trait_: Def) -> Result<(), CheckError> {
        for i in 0..self.work.cons.len() {
            let t = self.work.cons[i];
            let args = tys.mk_list(&[t]).map_err(solve(step))?;
            self.expected.push(Pred::Trait { trait_, args, polarity: Polarity::Positive });
        }
        Ok(())
    }

    fn by_builtin<T: Tys, F: Facts>(
        &mut self,
        tys: &mut T,
        ask: &Ask<'_, F>,
        reads: &mut Vec<Read, Alloc<'a>>,
        step: StepId,
        goal: Pred,
        rule: Builtin,
        cost: &mut Cost,
    ) -> Result<(), CheckError> {
        let (trait_, self_ty, _) = trait_self(tys, goal, step)?;
        let td = ask.trait_def(reads, trait_).map_err(|s| stop(step, s))?;
        let k = tys.kind(self_ty);
        let w = &mut self.work;
        w.cons.clear();
        let holds = match rule {
            Builtin::Sized | Builtin::MetaSized => {
                let meta = rule == Builtin::MetaSized;
                let lang = if meta { Lang::MetaSized } else { Lang::Sized };
                td.lang == Some(lang) && sizedness(tys, ask, reads, w, k, meta, cost).map_err(|s| stop(step, s))?
            }
            Builtin::CopyClone => matches!(td.lang, Some(Lang::Copy | Lang::Clone)) && copy_clone(tys, k, &mut w.cons),
            Builtin::Tuple => td.lang == Some(Lang::Tuple) && matches!(k, TyKind::Tuple(_)),
            Builtin::FnPtr => td.lang == Some(Lang::FnPtrTrait) && matches!(k, TyKind::FnPtr(..)),
            Builtin::PointeeSized => td.lang == Some(Lang::PointeeSized),
            Builtin::AnyType => matches!(td.lang, Some(Lang::Pointee | Lang::DiscriminantKind | Lang::Destruct)),
            Builtin::Error => matches!(k, TyKind::Error),
        };
        if !holds {
            return Err(CheckError::Builtin { step, rule });
        }
        self.constituent_goals(tys, step, trait_)
    }

    fn by_auto<T: Tys, F: Facts>(
        &mut self,
        tys: &mut T,
        ask: &Ask<'_, F>,
        reads: &mut Vec<Read, Alloc<'a>>,
        step: StepId,
        goal: Pred,
        cost: &mut Cost,
    ) -> Result<(), CheckError> {
        let (trait_, self_ty, _) = trait_self(tys, goal, step)?;
        let td = ask.trait_def(reads, trait_).map_err(|s| stop(step, s))?;
        let k = tys.kind(self_ty);
        // "Coroutines can only be `Unpin` if they are movable" (structural.rs `assemble_auto`).
        if !td.flags.has(TraitFlags::AUTO) || (matches!(k, TyKind::Coroutine(..)) && td.lang == Some(Lang::Unpin)) {
            return Err(CheckError::NotAuto { step });
        }
        self.work.cons.clear();
        if !auto_constituents(tys, ask, reads, &mut self.work, k, cost).map_err(|s| stop(step, s))? {
            return Err(CheckError::NotAuto { step });
        }
        self.constituent_goals(tys, step, trait_)
    }

    fn by_object<T: Tys, F: Facts>(
        &mut self,
        tys: &mut T,
        ask: &Ask<'_, F>,
        reads: &mut Vec<Read, Alloc<'a>>,
        step: StepId,
        goal: Pred,
        bound: u32,
        cost: &mut Cost,
    ) -> Result<(), CheckError> {
        let refuse = CheckError::ObjectBound { step, index: bound };
        let (trait_, self_ty, gargs) = trait_self(tys, goal, step)?;
        let td = ask.trait_def(reads, trait_).map_err(|s| stop(step, s))?;
        if matches!(td.lang, Some(Lang::Sized | Lang::MetaSized | Lang::PointeeSized)) {
            return Err(refuse);
        }
        let TyKind::Dynamic(preds, _) = tys.kind(self_ty) else { return Err(refuse) };
        if bound >= preds.len {
            return Err(refuse);
        }
        for i in 0..preds.len {
            if matches!(tys.kind(tys.list_get(preds, i)), TyKind::ExProj(..)) {
                return Err(CheckError::ObjectProjections { step });
            }
        }
        // `assemble_object`: no object candidate at all when the principal is not dyn
        // compatible.
        if preds.len > 0
            && let TyKind::ExTrait(pd, _) = tys.kind(tys.list_get(preds, 0))
            && !ask.trait_def(reads, pd).map_err(|s| stop(step, s))?.flags.has(TraitFlags::DYN_COMPATIBLE)
        {
            return Err(refuse);
        }
        let w = &mut self.work;
        w.list.clear();
        match tys.kind(tys.list_get(preds, bound)) {
            TyKind::ExAuto(d) if d == trait_ && gargs.len == 1 => w.list.push(self_ty),
            TyKind::ExTrait(d, l) if d == trait_ && bound == 0 && Some(gargs.len) == l.len.checked_add(1) => {
                w.list.push(self_ty);
                for i in 0..l.len {
                    let (x, y) = (tys.list_get(gargs, i + 1), tys.list_get(l, i));
                    if !same(tys, x, y, &mut w.pairs, cost).map_err(solve(step))? {
                        return Err(refuse);
                    }
                    w.list.push(x);
                }
            }
            _ => return Err(refuse),
        }
        cost.headers += 1;
        // `step_object`: the trait's supertraits, then its non-generic associated types' bounds,
        // over the assumption's arguments.
        for &p in td.supertraits {
            let p = import_pred(tys, &mut w.scratch, td.portable, p, &w.list, &mut w.cons).map_err(solve(step))?;
            self.expected.push(p);
        }
        for at in td.assoc {
            if at.own_generics != 0 {
                continue;
            }
            let (s, e) = (idx(at.bounds_start), idx(at.bounds_start) + idx(at.bounds_len));
            let Some(bs) = td.item_bounds.get(s..e) else { return Err(CheckError::Solve { step, error: SolveError::Malformed(bad_pred()) }) };
            for &p in bs {
                let p = import_pred(tys, &mut w.scratch, td.portable, p, &w.list, &mut w.cons).map_err(solve(step))?;
                self.expected.push(p);
            }
        }
        Ok(())
    }

    fn by_alias_bound<T: Tys, F: Facts>(
        &mut self,
        tys: &mut T,
        ask: &Ask<'_, F>,
        reads: &mut Vec<Read, Alloc<'a>>,
        step: StepId,
        goal: Pred,
        bound: u32,
        cost: &mut Cost,
    ) -> Result<(), CheckError> {
        let refuse = CheckError::AliasBound { step, index: bound };
        let (_, self_ty, _) = trait_self(tys, goal, step)?;
        let w = &mut self.work;
        w.list.clear();
        let want = match tys.kind(self_ty) {
            // Only a rigid projection has alias bounds: one that normalizes is proven through
            // its value.
            TyKind::Alias(Rigid::Yes, AliasKind::Projection, item, aargs) => {
                let ad = ask.assoc(reads, item).map_err(|s| stop(step, s))?;
                let Some(tr) = ad.trait_ else { return Err(refuse) };
                let td = ask.trait_def(reads, tr).map_err(|s| stop(step, s))?;
                let Some(at) = td.assoc.iter().find(|a| a.def == item) else { return Err(refuse) };
                if bound >= at.bounds_len {
                    return Err(refuse);
                }
                let Some(&p) = td.item_bounds.get(idx(at.bounds_start) + idx(bound)) else { return Err(refuse) };
                list_into(tys, aargs, &mut w.list);
                cost.headers += 1;
                import_pred(tys, &mut w.scratch, td.portable, p, &w.list, &mut w.cons).map_err(solve(step))?
            }
            TyKind::Alias(_, AliasKind::Opaque, d, oargs) => {
                let op = ask.opaque(reads, d).map_err(|s| stop(step, s))?;
                // Where the hidden type is visible, the solver proves through it.
                if op.hidden.is_some() {
                    return Err(refuse);
                }
                let Some(&p) = op.bounds.get(idx(bound)) else { return Err(refuse) };
                list_into(tys, oargs, &mut w.list);
                cost.headers += 1;
                import_pred(tys, &mut w.scratch, op.portable, p, &w.list, &mut w.cons).map_err(solve(step))?
            }
            _ => return Err(refuse),
        };
        if !same_pred(tys, want, goal, &mut w.pairs, cost).map_err(solve(step))? {
            return Err(refuse);
        }
        Ok(())
    }
}

/// A positive trait goal's trait, its self type (resolved) and its arguments.
fn trait_self<T: Tys>(tys: &T, goal: Pred, step: StepId) -> Result<(Def, Ty, List), CheckError> {
    let Pred::Trait { trait_, args, polarity } = goal else { return Err(CheckError::NotTrait { step }) };
    if args.len == 0 {
        return Err(CheckError::NotTrait { step });
    }
    if polarity != Polarity::Positive {
        return Err(CheckError::Negative { step });
    }
    Ok((trait_, tys.shallow_resolve(tys.list_get(args, 0)), args))
}

/// The goal's class for the cycle rule (eval.rs, `step_kind_for_source`).
fn class_of<F: Facts>(ask: &Ask<'_, F>, reads: &mut Vec<Read, Alloc<'_>>, goal: Pred) -> Result<u8, Stop> {
    Ok(match goal {
        Pred::Trait { trait_, .. } => {
            let d = ask.trait_def(reads, trait_)?;
            if d.flags.has(TraitFlags::AUTO) || d.flags.has(TraitFlags::COINDUCTIVE) { COINDUCTIVE } else { FORCED }
        }
        Pred::Projection { .. } | Pred::NormalizesTo { .. } => PROJECTION,
        _ => FORCED,
    })
}

fn list_into<T: Tys>(tys: &T, l: List, out: &mut Vec<Ty, Alloc<'_>>) {
    out.clear();
    for i in 0..l.len {
        out.push(tys.list_get(l, i));
    }
}

/// The ADT `d`'s fields at `args`, pushed to `w.cons` (`adt_fields`), or its sized tail only.
fn adt_parts<T: Tys, F: Facts>(
    tys: &mut T,
    ask: &Ask<'_, F>,
    reads: &mut Vec<Read, Alloc<'_>>,
    w: &mut Work<'_>,
    d: Def,
    args: List,
    tail_only: bool,
    cost: &mut Cost,
) -> Result<AdtFlags, Stop> {
    let adt = ask.adt(reads, d)?;
    cost.headers += 1;
    list_into(tys, args, &mut w.list);
    if tail_only {
        if let Some(t) = adt.sized_tail {
            let t = fold::import(tys, &mut w.scratch, adt.portable, t, Inst { args: &w.list, bound: &[] })?;
            w.cons.push(t);
        }
        return Ok(adt.flags);
    }
    if adt.flags.has(AdtFlags::PHANTOM_DATA) {
        let Some(&t) = w.list.first() else { return Err(Stop::Error(SolveError::Malformed(bad_pred()))) };
        w.cons.push(t);
        return Ok(adt.flags);
    }
    for &f in adt.fields {
        let t = fold::import(tys, &mut w.scratch, adt.portable, f, Inst { args: &w.list, bound: &[] })?;
        w.cons.push(t);
    }
    Ok(adt.flags)
}

/// `step_sizedness` (builtin.rs): whether the rule gives `k: {Meta,}Sized`, its one constituent
/// (if any) pushed to `w.cons`.
fn sizedness<T: Tys, F: Facts>(
    tys: &mut T,
    ask: &Ask<'_, F>,
    reads: &mut Vec<Read, Alloc<'_>>,
    w: &mut Work<'_>,
    k: TyKind,
    meta: bool,
    cost: &mut Cost,
) -> Result<bool, Stop> {
    Ok(match k {
        TyKind::Infer(Var { kind: VarKind::Int | VarKind::Float, .. })
        | TyKind::Uint(_)
        | TyKind::Int(_)
        | TyKind::Bool
        | TyKind::Float(_)
        | TyKind::FnDef(..)
        | TyKind::FnPtr(..)
        | TyKind::RawPtr(..)
        | TyKind::Char
        | TyKind::Ref(..)
        | TyKind::Array(..)
        | TyKind::Closure(..)
        | TyKind::Coroutine(..)
        | TyKind::Never
        | TyKind::Pat(..)
        | TyKind::Error => true,
        TyKind::Str | TyKind::Slice(_) | TyKind::Dynamic(..) => meta,
        TyKind::Tuple(l) => {
            if l.len > 0 {
                w.cons.push(tys.list_get(l, l.len - 1));
            }
            true
        }
        TyKind::Record(_, l) | TyKind::Union(l) => {
            for i in 0..l.len {
                w.cons.push(tys.list_get(l, i));
            }
            true
        }
        TyKind::Adt(d, args) => {
            adt_parts(tys, ask, reads, w, d, args, true, cost)?;
            true
        }
        _ => false,
    })
}

/// `step_copy_clone` (builtin.rs): whether the builtin rule gives `k: Copy`/`Clone`, its
/// constituents pushed to `out`.
fn copy_clone<T: Tys>(tys: &T, k: TyKind, out: &mut Vec<Ty, Alloc<'_>>) -> bool {
    match k {
        TyKind::FnDef(..) | TyKind::FnPtr(..) | TyKind::Error => true,
        TyKind::Tuple(l) | TyKind::Record(_, l) | TyKind::Union(l) => {
            for i in 0..l.len {
                out.push(tys.list_get(l, i));
            }
            true
        }
        TyKind::Pat(base, _) => {
            out.push(base);
            true
        }
        TyKind::Closure(_, args) if args.len >= 3 => {
            out.push(tys.list_get(args, args.len - 1));
            true
        }
        _ => false,
    }
}

/// `step_auto` (structural.rs) without the summaries' shortcut (the same goals, field by field):
/// whether the structural rule applies to `k`, its constituents pushed to `w.cons`.
fn auto_constituents<T: Tys, F: Facts>(
    tys: &mut T,
    ask: &Ask<'_, F>,
    reads: &mut Vec<Read, Alloc<'_>>,
    w: &mut Work<'_>,
    k: TyKind,
    cost: &mut Cost,
) -> Result<bool, Stop> {
    Ok(match k {
        TyKind::Uint(_)
        | TyKind::Int(_)
        | TyKind::Bool
        | TyKind::Float(_)
        | TyKind::FnDef(..)
        | TyKind::FnPtr(..)
        | TyKind::Error
        | TyKind::Infer(Var { kind: VarKind::Int | VarKind::Float, .. })
        | TyKind::Never
        | TyKind::Char => true,
        // `str` is `[u8]`.
        TyKind::Str => {
            let u8_ = tys.mk(TyKind::Uint(UintTy::U8))?;
            w.cons.push(tys.mk(TyKind::Slice(u8_))?);
            true
        }
        TyKind::RawPtr(e, _) | TyKind::Ref(_, e, _) | TyKind::Array(e, _) | TyKind::Slice(e) | TyKind::Pat(e, _) => {
            w.cons.push(e);
            true
        }
        TyKind::Tuple(l) | TyKind::Record(_, l) | TyKind::Union(l) => {
            for i in 0..l.len {
                w.cons.push(tys.list_get(l, i));
            }
            true
        }
        TyKind::Closure(_, args) if args.len >= 3 => {
            w.cons.push(tys.list_get(args, args.len - 1));
            true
        }
        TyKind::Coroutine(_, args) if args.len == 2 => {
            w.cons.push(tys.list_get(args, 1));
            true
        }
        TyKind::Adt(d, args) => {
            adt_parts(tys, ask, reads, w, d, args, false, cost)?;
            true
        }
        TyKind::Alias(_, AliasKind::Opaque, d, args) => {
            let op = ask.opaque(reads, d)?;
            match op.hidden {
                Some(h) => {
                    list_into(tys, args, &mut w.list);
                    let t = fold::import(tys, &mut w.scratch, op.portable, h, Inst { args: &w.list, bound: &[] })?;
                    w.cons.push(t);
                    true
                }
                None => false,
            }
        }
        _ => false,
    })
}

/// The portable list `l` of `p` with its parameters instantiated by `args`.
pub(crate) fn import_list<T: Tys>(
    tys: &mut T,
    s: &mut Scratch<'_>,
    p: Portable<'_>,
    l: List,
    args: &[Ty],
    out: &mut Vec<Ty, Alloc<'_>>,
) -> Result<List, SolveError> {
    out.clear();
    for i in 0..l.len {
        let bad = SolveError::Malformed(Malformed::PortableIndex { node: l.start, len: len32(p.lists.len()).unwrap_or(u32::MAX) });
        let at = l.start.checked_add(i).ok_or(bad)?;
        let Some(&x) = p.lists.get(idx(at)) else { return Err(bad) };
        out.push(fold::import(tys, s, p, x, Inst { args, bound: &[] })?);
    }
    tys.mk_list(out)
}

/// The portable predicate `pred` of `p` with its parameters instantiated by `args` (coherence.rs
/// `import_pred`, the same rule).
fn import_pred<T: Tys>(
    tys: &mut T,
    s: &mut Scratch<'_>,
    p: Portable<'_>,
    pred: Pred,
    args: &[Ty],
    out: &mut Vec<Ty, Alloc<'_>>,
) -> Result<Pred, SolveError> {
    let inst = Inst { args, bound: &[] };
    Ok(match pred {
        Pred::Trait { trait_, args: a, polarity } => Pred::Trait { trait_, args: import_list(tys, s, p, a, args, out)?, polarity },
        Pred::ForFields { container, trait_, args_after_self: a } => Pred::ForFields {
            container: fold::import(tys, s, p, container, inst)?,
            trait_,
            args_after_self: import_list(tys, s, p, a, args, out)?,
        },
        Pred::HostEffect { trait_, args: a, constness } => Pred::HostEffect { trait_, args: import_list(tys, s, p, a, args, out)?, constness },
        Pred::ConstHolds { cond, args: a } => Pred::ConstHolds { cond, args: import_list(tys, s, p, a, args, out)? },
        Pred::Projection { alias, term } => Pred::Projection { alias: fold::import(tys, s, p, alias, inst)?, term: fold::import(tys, s, p, term, inst)? },
        Pred::NormalizesTo { alias, term } => Pred::NormalizesTo { alias: fold::import(tys, s, p, alias, inst)?, term: fold::import(tys, s, p, term, inst)? },
        Pred::TypeOutlives { ty, region } => Pred::TypeOutlives { ty: fold::import(tys, s, p, ty, inst)?, region: fold::import(tys, s, p, region, inst)? },
        Pred::RegionOutlives { a, b } => Pred::RegionOutlives { a: fold::import(tys, s, p, a, inst)?, b: fold::import(tys, s, p, b, inst)? },
        Pred::WellFormed(x) => Pred::WellFormed(fold::import(tys, s, p, x, inst)?),
        Pred::ConstEvaluatable(x) => Pred::ConstEvaluatable(fold::import(tys, s, p, x, inst)?),
        Pred::Subtype { a, b } => Pred::Subtype { a: fold::import(tys, s, p, a, inst)?, b: fold::import(tys, s, p, b, inst)? },
        Pred::Coerce { a, b } => Pred::Coerce { a: fold::import(tys, s, p, a, inst)?, b: fold::import(tys, s, p, b, inst)? },
        Pred::ConstArgHasType { ct, ty } => Pred::ConstArgHasType { ct: fold::import(tys, s, p, ct, inst)?, ty: fold::import(tys, s, p, ty, inst)? },
        Pred::DynCompatible(d) => Pred::DynCompatible(d),
        // A where-clause is never an `Eq` (coherence.rs `import_pred`, the same refusal).
        Pred::Eq { .. } => return Err(SolveError::Malformed(bad_pred())),
    })
}

/// `Malformed::Pred` refused at the caller's line (blaze-solve's own helper is crate-private).
#[track_caller]
fn bad_pred() -> Malformed {
    Malformed::Pred { at: core::panic::Location::caller(), goal: None, asked_by: None }
}

/// `k` with its children replaced by a fixed handle (lists keep their length): what two terms
/// must share at one level to be equal.
fn bare(k: TyKind) -> TyKind {
    const Z: Ty = Ty(0);
    let l0 = |l: List| List { start: 0, len: l.len };
    match k {
        TyKind::Tuple(l) => TyKind::Tuple(l0(l)),
        TyKind::Record(s, l) => TyKind::Record(s, l0(l)),
        TyKind::Union(l) => TyKind::Union(l0(l)),
        TyKind::Ref(_, _, m) => TyKind::Ref(Z, Z, m),
        TyKind::RawPtr(_, m) => TyKind::RawPtr(Z, m),
        TyKind::Array(..) => TyKind::Array(Z, Z),
        TyKind::Slice(_) => TyKind::Slice(Z),
        TyKind::Pat(_, l) => TyKind::Pat(Z, l0(l)),
        TyKind::Adt(d, l) => TyKind::Adt(d, l0(l)),
        TyKind::FnDef(d, l) => TyKind::FnDef(d, l0(l)),
        TyKind::FnPtr(l, h) => TyKind::FnPtr(l0(l), h),
        TyKind::Closure(d, l) => TyKind::Closure(d, l0(l)),
        TyKind::Coroutine(d, l) => TyKind::Coroutine(d, l0(l)),
        TyKind::Dynamic(l, _) => TyKind::Dynamic(l0(l), Z),
        TyKind::Alias(r, a, d, l) => TyKind::Alias(r, a, d, l0(l)),
        TyKind::ExTrait(d, l) => TyKind::ExTrait(d, l0(l)),
        TyKind::ExProj(d, l, _) => TyKind::ExProj(d, l0(l), Z),
        other => other,
    }
}

fn push_list<T: Tys>(tys: &T, l: List, m: List, pairs: &mut Vec<(Ty, Ty), Alloc<'_>>) {
    for i in 0..l.len.min(m.len) {
        pairs.push((tys.list_get(l, i), tys.list_get(m, i)));
    }
}

/// Whether `a` and `b` are the same term, any region equal to any region: an explicit stack,
/// no binding, at most `fold::MAX_WALK` pairs.
fn same<T: Tys>(tys: &T, a: Ty, b: Ty, pairs: &mut Vec<(Ty, Ty), Alloc<'_>>, cost: &mut Cost) -> Result<bool, SolveError> {
    pairs.clear();
    pairs.push((a, b));
    let mut walked = 0usize;
    while let Some((x, y)) = pairs.pop() {
        walked += 1;
        if walked > fold::MAX_WALK {
            pairs.clear();
            return Err(SolveError::Limit(Limit::Walk));
        }
        cost.nodes_compared += 1;
        let (x, y) = (tys.shallow_resolve(x), tys.shallow_resolve(y));
        if x == y {
            // One handle is one term (the arena hash-conses).
            continue;
        }
        let (kx, ky) = (tys.kind(x), tys.kind(y));
        if is_region(kx) && is_region(ky) {
            continue;
        }
        if bare(kx) != bare(ky) {
            pairs.clear();
            return Ok(false);
        }
        // Equal bare kinds have the same children's shape.
        match (children(kx), children(ky)) {
            (Children::None, Children::None) => {}
            (Children::One(p), Children::One(q)) => pairs.push((p, q)),
            (Children::Two(p1, p2), Children::Two(q1, q2)) => {
                pairs.push((p1, q1));
                pairs.push((p2, q2));
            }
            (Children::List(l), Children::List(m)) => push_list(tys, l, m, pairs),
            (Children::ListThen(l, p), Children::ListThen(m, q)) => {
                push_list(tys, l, m, pairs);
                pairs.push((p, q));
            }
            _ => {
                pairs.clear();
                return Ok(false);
            }
        }
    }
    Ok(true)
}

fn same_list<T: Tys>(tys: &T, l: List, m: List, pairs: &mut Vec<(Ty, Ty), Alloc<'_>>, cost: &mut Cost) -> Result<bool, SolveError> {
    if l.len != m.len {
        return Ok(false);
    }
    for i in 0..l.len {
        if !same(tys, tys.list_get(l, i), tys.list_get(m, i), pairs, cost)? {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Whether two predicates are the same (terms by [`same`]).
fn same_pred<T: Tys>(tys: &T, a: Pred, b: Pred, pairs: &mut Vec<(Ty, Ty), Alloc<'_>>, cost: &mut Cost) -> Result<bool, SolveError> {
    // Two-term predicates of one variant: their terms, pairwise.
    let two = match (a, b) {
        (Pred::Trait { trait_: t1, args: a1, polarity: p1 }, Pred::Trait { trait_: t2, args: a2, polarity: p2 }) => {
            return Ok(t1 == t2 && p1 == p2 && same_list(tys, a1, a2, pairs, cost)?);
        }
        (Pred::HostEffect { trait_: t1, args: a1, constness: c1 }, Pred::HostEffect { trait_: t2, args: a2, constness: c2 }) => {
            return Ok(t1 == t2 && c1 == c2 && same_list(tys, a1, a2, pairs, cost)?);
        }
        (Pred::ConstHolds { cond: c1, args: a1 }, Pred::ConstHolds { cond: c2, args: a2 }) => {
            return Ok(c1 == c2 && same_list(tys, a1, a2, pairs, cost)?);
        }
        (Pred::WellFormed(x1), Pred::WellFormed(x2)) | (Pred::ConstEvaluatable(x1), Pred::ConstEvaluatable(x2)) => {
            return same(tys, x1, x2, pairs, cost);
        }
        (Pred::DynCompatible(d1), Pred::DynCompatible(d2)) => return Ok(d1 == d2),
        (Pred::Projection { alias: x1, term: y1 }, Pred::Projection { alias: x2, term: y2 })
        | (Pred::NormalizesTo { alias: x1, term: y1 }, Pred::NormalizesTo { alias: x2, term: y2 })
        | (Pred::TypeOutlives { ty: x1, region: y1 }, Pred::TypeOutlives { ty: x2, region: y2 })
        | (Pred::RegionOutlives { a: x1, b: y1 }, Pred::RegionOutlives { a: x2, b: y2 })
        | (Pred::Subtype { a: x1, b: y1 }, Pred::Subtype { a: x2, b: y2 })
        | (Pred::Coerce { a: x1, b: y1 }, Pred::Coerce { a: x2, b: y2 })
        | (Pred::Eq { a: x1, b: y1 }, Pred::Eq { a: x2, b: y2 })
        | (Pred::ConstArgHasType { ct: x1, ty: y1 }, Pred::ConstArgHasType { ct: x2, ty: y2 }) => ((x1, x2), (y1, y2)),
        _ => return Ok(false),
    };
    Ok(same(tys, two.0.0, two.0.1, pairs, cost)? && same(tys, two.1.0, two.1.1, pairs, cost)?)
}
