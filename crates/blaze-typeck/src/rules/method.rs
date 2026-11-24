//! Method calls: the probe and the confirmation.
//!
//! Follows rustc_hir_typeck/src/method/probe.rs (`probe_op`, `create_steps`, `pick_all_method`,
//! `pick_by_value_method`, `pick_autorefd_method`, `pick_method`,
//! `assemble_inherent_candidates_from_param`) and method/confirm.rs (`confirm`,
//! `adjust_self_ty`, `instantiate_method_args`); Relax maps them in typeck/method.rs.
//!
//! The order is rustc's, checked against Relax's line-for-line port of probe.rs @8d1a764
//! (types/src/typeck/probe.rs: `method_autoderef_steps`, `assemble_probe_candidates`,
//! `pick_all_method`): the receiver is checked with no expectation and resolved structurally
//! (E0282 on a variable: a method call is never deferred). The steps are its autoderef steps;
//! an array step is followed by its `[T]` step and autoderef stops there (an array does not
//! deref, so this is the last step). The inherent candidates of every step (the steps' inherent
//! impls, and the bound traits of a parameter) are assembled once, before any is tried. At each
//! step, by value (a reborrow at a reference step; not on the `[T]` step), then `&`, then
//! `&mut`; at each of those, the inherent candidates before the extension candidates (the
//! traits in scope). Two applicable candidates
//! in one group is E0034; none at all is E0599. Confirmation re-instantiates the pick for real,
//! records the receiver's adjustments (derefs, the autoref with two-phase for `&mut`, the
//! unsize), the callee, the method's where-clauses and the trait obligation, then checks the
//! arguments as a call does.
//!
//! An inherent candidate the body may not name ([`World::member_visible`]) is set aside before
//! any is tried (probe.rs `push_candidate`'s `private_candidates`); only when nothing visible
//! applies does one that applies make the error E0624.
//!
//! An inherent candidate unstable under a library feature the body's crate does not turn on
//! ([`World::member_unstable`]) is left out of a first pass and taken only by a second when the
//! first picks nothing (probe.rs `pick_core`, `consider_candidates`). The stability of a TRAIT's
//! method is not asked yet (std's unstable `Iterator::intersperse` beside itertools'
//! `Itertools::intersperse` would still be E0034 here, where rustc picks itertools').
//!
//! Omitted for now: raw-pointer autoref (`pick_const_ptr_method`), the 2021 `into_iter` array
//! shadowing, unstable-candidate reporting (rustc's E0658 and the
//! `unstable_name_collisions` lint), and `self: Box<Self>`-style receivers beyond what
//! autoderef reaches.

use blaze_own::{Alloc, Vec};

use crate::body::Segment;
use crate::columns::AdjustKind;
use crate::diag::{About, At, DiagCode, PendingDiag, PendingGoal};
use crate::error::Stop;
use crate::ids::{ExprId, ItemKey, List, Mutbl, Name, ScopeId};
use crate::machine::{Expectation, Flow, Frame, Lower, Machine, Step, Then};
use crate::ty::{Cause, Goal, Ty, TyList, TyView, TypeCtx};
use crate::world::{AssocKind, World};

/// A candidate: the impl or trait holding the item, and whether it takes `self`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Cand {
    container: ItemKey,
    item: ItemKey,
    trait_: Option<ItemKey>,
    is_method: bool,
    /// For a candidate of a parameter's bound or an object's principal (and their
    /// supertraits): the step type it was found at, which is the trait's `Self` (probe.rs
    /// `WhereClauseCandidate` and `ObjectCandidate`: their `xform_self_ty` is the method's
    /// `self` with `Self` the parameter or the object). `None`: an impl's item, or a trait in
    /// scope, whose `Self` is a fresh variable the receiver decides. Without it a bound
    /// candidate applied at every step as if in scope, AND in the inherent group: tempo's
    /// zone_sequencer settlement.rs:96 `error.fmt(f)` on a `&eyre::Report` (whose deref step
    /// `dyn Error + Send + Sync` brings `Debug::fmt` and `Display::fmt`) matched both at the
    /// `&Report` step, a false E0034 ("found `&Report`, about `fmt`").
    on: Option<Ty>,
    /// An inherent candidate unstable under a library feature the body's crate does not turn
    /// on ([`World::member_unstable`]): left out of the first pass ([`Machine::pick_pass`]).
    denied: bool,
}

/// Why the probe found no method: the code. For a plain "no method" (`DiagCode::NoMethod`),
/// also the first inherent candidate whose `self` matched at some step but whose impl's
/// where-clause cannot hold, with that adjusted receiver (rustc's `NoMatchData::
/// unsatisfied_predicates`).
struct Miss {
    code: DiagCode,
    unmet: Option<(Cand, Ty)>,
}

/// One pass's answer ([`Machine::pick_pass`]).
enum Pass {
    Picked { cand: Cand, step: usize, autoref: Option<Mutbl>, unsize: bool },
    Ambiguous,
    Nothing,
}

/// The probe's answer.
struct Pick<'a> {
    cand: Cand,
    steps: Vec<Step, Alloc<'a>>,
    step: usize,
    autoref: Option<Mutbl>,
    unsize: bool,
}

impl<'a, W: World, C: TypeCtx> Machine<'_, '_, '_, 'a, W, C> {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn expr_method_call(
        &mut self,
        f: Frame,
        e: ExprId,
        receiver: ExprId,
        name: Name,
        generic_args: List,
        args: List,
        scope: ScopeId,
    ) -> Result<Flow, Stop> {
        let arg_ids = self.body.expr_list(args)?;
        match f.state {
            0 => {
                self.resume(f, 1)?;
                self.push_expr(receiver, Expectation::None, Then::Nothing)?;
                Ok(Flow::Wait)
            }
            1 => {
                let recv = self.structurally_resolve(self.ret, At::Expr(receiver))?;
                if self.is_err(recv) {
                    let errs = self.err_list(arg_ids.len())?;
                    return self.start_args(f, e, arg_ids, errs, self.common.err, 2, false);
                }
                let seg = Segment { name, generic_args, has_args: generic_args.len > 0 };
                match self.probe_method(recv, name, scope)? {
                    Ok(pick) => {
                        let Some((inputs, output)) = self.confirm(e, receiver, &pick, seg)? else {
                            self.diag(DiagCode::NoMethod, At::Expr(e), About::found_named(recv, name));
                            let errs = self.err_list(arg_ids.len())?;
                            return self.start_args(f, e, arg_ids, errs, self.common.err, 2, false);
                        };
                        // The call's expected type decides what it can of the formal inputs
                        // before an argument is typed, as for a call (rules/expect_inputs.rs;
                        // rustc's `check_method_argument_types` passes its expectation to the
                        // same `check_argument_types`). mime_guess 2.0.5's `MimeGuess::iter`:
                        // `Iter(self.iter_raw().map(expect_mime))`, the field
                        // `Map<IterRaw, fn(&'static str) -> Mime>`: the expected output makes
                        // `F` the fn pointer, and the fn item `expect_mime` then coerces to it
                        // ("Coercion types": a fn item to a fn pointer) instead of making `F`
                        // the fn item's own type, which the field refuses (E0308).
                        self.expected_inputs(f.expect, inputs, output, arg_ids)?;
                        self.start_args(f, e, arg_ids, inputs, output, 2, true)
                    }
                    // rustc's E0599 for a method whose bounds fail (method/suggest.rs): "the
                    // method `get` exists for struct `W<B>`, but its trait bounds were not
                    // satisfied", at the method's name, naming the bound (rule 19).
                    Err(Miss { code, unmet }) => {
                        let goal = match unmet {
                            Some((c, adjusted)) if code == DiagCode::NoMethod => self.unmet_impl_clause(c, adjusted)?,
                            _ => None,
                        };
                        match goal {
                            Some(goal) => {
                                let at = self.body.method_name_range(e).map_or(At::Expr(e), At::Written);
                                self.errors += 1;
                                self.diags.push(PendingDiag::unmet(DiagCode::NoMethod, at, Some(recv), goal, None).with_name(Some(name)));
                            }
                            None => self.diag(code, At::Expr(e), About::found_named(recv, name)),
                        }
                        let errs = self.err_list(arg_ids.len())?;
                        self.start_args(f, e, arg_ids, errs, self.common.err, 2, false)
                    }
                }
            }
            _ => {
                if self.next_arg(f, arg_ids, 2)? { Ok(Flow::Wait) } else { Ok(Flow::Done(f.u)) }
            }
        }
    }

    /// The trait of the impl whose item this body is (`impl Tr for T { fn f() { .. } }` gives
    /// `Tr`), or `None` outside a trait impl. rustc_resolve counts it among the traits in scope
    /// of every body in the impl (`traits_in_scope` adds `current_trait_ref`), so a method or
    /// path of that trait needs no `use`; both the method probe and a type-relative path
    /// (rules/lit_path.rs) take it from here.
    pub(super) fn current_impl_trait(&mut self) -> Result<Option<ItemKey>, Stop> {
        let Some(parent) = self.reads.assoc_parent(self.body.owner)? else { return Ok(None) };
        Ok(self.reads.impl_header(parent.container)?.and_then(|h| h.trait_))
    }

    /// One `pick_all_method` over the steps: at each step by value, `&`, then `&mut`; at each,
    /// the inherent group before the extension group; two applicable in one group is E0034.
    /// With `stable_only` (rustc's first pass, `pick_diag_hints.unstable_candidates` some), a
    /// candidate unstable under a feature this crate does not turn on ([`Cand::denied`]) is
    /// probed (an unmet bound of it is still noted) but not counted, as `consider_candidates`
    /// drops it after `consider_probe`.
    #[allow(clippy::too_many_arguments)]
    fn pick_pass(
        &mut self,
        steps: &[Step],
        unsize_at: Option<usize>,
        inh: &[Cand],
        ext: &[Cand],
        stable_only: bool,
        not_method: &mut bool,
        first_unmet: &mut Option<(Cand, Ty)>,
    ) -> Result<Pass, Stop> {
        for i in 0..steps.len() {
            let step = steps[i];
            let unsize = unsize_at == Some(i);
            for autoref in [None, Some(Mutbl::Not), Some(Mutbl::Mut)] {
                if unsize && autoref.is_none() {
                    continue;
                }
                let adjusted = match autoref {
                    None => step.ty,
                    Some(m) => self.mk(TyView::Ref(m, step.ty))?,
                };
                for group in 0..2 {
                    let mut found = None;
                    let mut count = 0u32;
                    let cands = if group == 0 { inh } else { ext };
                    // A trait in scope is tried here even when a bound candidate names the same
                    // item: that one's `Self` is its own step's type, this one's the receiver's.
                    for &c in cands.iter() {
                        if !c.is_method {
                            *not_method = true;
                            continue;
                        }
                        let mut unmet = false;
                        if self.probe(|m| m.instantiate_method(c, adjusted, None, &mut unmet))?.is_some() {
                            if stable_only && c.denied {
                                continue;
                            }
                            count += 1;
                            if found.is_none() {
                                found = Some(c);
                            }
                        } else if unmet && first_unmet.is_none() {
                            *first_unmet = Some((c, adjusted));
                        }
                    }
                    if count > 1 {
                        return Ok(Pass::Ambiguous);
                    }
                    if let Some(cand) = found {
                        return Ok(Pass::Picked { cand, step: i, autoref, unsize });
                    }
                }
            }
        }
        Ok(Pass::Nothing)
    }

    /// `probe_op` in `Mode::MethodCall`: the pick, or the code of the error.
    fn probe_method(&mut self, recv: Ty, name: Name, scope: ScopeId) -> Result<Result<Pick<'a>, Miss>, Stop> {
        let (mut steps, limit) = self.autoderef(recv, false)?;
        let mut unsize_at = None;
        if let Some(last) = steps.last().copied() {
            if let TyView::Array(elem, _) = self.view(last.ty) {
                let slice = self.mk(TyView::Slice(elem))?;
                unsize_at = Some(steps.len());
                steps.push(Step { ty: slice, overloaded: false });
            }
        }
        // The extension candidates: one per trait in scope with an item of this name.
        let mut ext: Vec<Cand, Alloc<'a>> = Vec::new_in(self.alloc);
        for &tr in self.reads.traits_in_scope(scope)? {
            if let Some(it) = self.reads.assoc_item(tr, name)? {
                if let AssocKind::Fn { self_param } = it.kind {
                    ext.push(Cand { container: tr, item: it.item, trait_: Some(tr), is_method: self_param, on: None, denied: false });
                }
            }
        }
        // The trait of the impl this body is an item of is in scope too, imported or not
        // (`Self::current_impl_trait`): inside `impl fmt::Display for E` with only `fmt`
        // imported, `inner.fmt(f)` on a `&eyre::Report` is `Display::fmt` (tempo's
        // zone_sequencer settlement.rs:96, a false "found `&Report`, about `fmt`").
        if let Some(tr) = self.current_impl_trait()?
            && !ext.iter().any(|c| c.trait_ == Some(tr))
            && let Some(it) = self.reads.assoc_item(tr, name)?
            && let AssocKind::Fn { self_param } = it.kind
        {
            ext.push(Cand { container: tr, item: it.item, trait_: Some(tr), is_method: self_param, on: None, denied: false });
        }
        // The inherent candidates of every step, assembled once before any is tried
        // (`assemble_inherent_candidates` over all steps, as Relax's `assemble_probe_candidates`):
        // an inherent `fn m(&self)` of `Foo` is found by value at the `&Foo` step, before any
        // extension candidate of `&Foo`.
        let mut not_method = false;
        let mut inh: Vec<Cand, Alloc<'a>> = Vec::new_in(self.alloc);
        // The inherent candidates the body may not name (the Reference, "Visibility and
        // privacy"): set aside, never tried, as rustc's probe.rs `push_candidate` puts an
        // inaccessible candidate in `private_candidates`. So a visible method of the name
        // further on (an extension candidate, a later step) is the pick: reth's private
        // `impl Tx<RW> { fn put<T>(&self, kind: PutKind, key, value) }` beside the trait
        // method `DbTxMut::put<T>(&self, key, value)` that `tx.put::<T>(k, v)` calls from
        // another crate. Only when nothing visible applies is it E0624.
        let mut private: Vec<Cand, Alloc<'a>> = Vec::new_in(self.alloc);
        let mut bounds: Vec<ItemKey, Alloc<'a>> = Vec::new_in(self.alloc);
        // The first inherent candidate dropped only by its impl's where-clause ([`Miss`]).
        let mut first_unmet: Option<(Cand, Ty)> = None;
        for i in 0..steps.len() {
            let step = steps[i];
            if let Some(shape) = self.shape(step.ty) {
                for it in self.reads.inherent(shape, name)? {
                    if let AssocKind::Fn { self_param } = it.kind {
                        let c = Cand { container: it.container, item: it.item, trait_: None, is_method: self_param, on: None, denied: false };
                        if inh.iter().chain(private.iter()).any(|x| x.item == c.item) {
                            continue;
                        }
                        if self.reads.member_visible(it.container, name)? {
                            let denied = self.reads.member_unstable(it.container, name)?;
                            inh.push(Cand { denied, ..c });
                        } else {
                            private.push(c);
                        }
                    }
                }
            }
            // A parameter's bound traits, and an object's principal with its supertraits, are
            // found at their step whether or not in scope (typeck.md "Trait objects" (c)), with
            // `Self` that step's type (`Cand::on`): they apply where the receiver's adjusted
            // type is the method's `self` of THAT type (`x: &T` still picks `T`'s bound method
            // at the `&T` step, by value), never with another `Self`.
            let view = self.view(step.ty);
            if let TyView::Param(_) | TyView::Dynamic(_) = view {
                bounds.clear();
                match view {
                    TyView::Param(p) => self.param_bound_traits(p, &mut bounds)?,
                    TyView::Dynamic(l) => {
                        if let Some((k, _)) = self.principal_args(l) {
                            bounds.push(k);
                            self.elaborate_bound_traits(&mut bounds, 0)?;
                        }
                    }
                    _ => {}
                }
                for &tr in bounds.iter() {
                    if let Some(it) = self.reads.assoc_item(tr, name)? {
                        if let AssocKind::Fn { self_param } = it.kind {
                            let c = Cand { container: tr, item: it.item, trait_: Some(tr), is_method: self_param, on: Some(step.ty), denied: false };
                            if !inh.iter().any(|x| x.item == c.item && x.on == c.on) {
                                inh.push(c);
                            }
                        }
                    }
                }
            }
        }
        // rustc_hir_typeck method/probe.rs `pick_core`: "Pick stable methods only first, and
        // consider unstable candidates if not found." The second pass runs only when the first
        // left a candidate out (work once), and its unmet bounds go to a throwaway list ("no
        // need to duplicate ourselves in the unsatisifed predicates list"). turso's
        // `spill_state.distinct_key_places[i].try_insert(hash, place)?` on a `HashMap`: std's
        // `#[unstable(feature = "map_try_insert")]` inherent `try_insert` is left out, and the
        // in-scope `TursoHashMapExt::try_insert` is the pick.
        let any_denied = inh.iter().any(|c| c.denied);
        let mut pass = self.pick_pass(&steps, unsize_at, &inh, &ext, any_denied, &mut not_method, &mut first_unmet)?;
        if any_denied && matches!(pass, Pass::Nothing) {
            pass = self.pick_pass(&steps, unsize_at, &inh, &ext, false, &mut not_method, &mut None)?;
        }
        match pass {
            Pass::Picked { cand, step, autoref, unsize } => return Ok(Ok(Pick { cand, steps, step, autoref, unsize })),
            Pass::Ambiguous => return Ok(Err(Miss { code: DiagCode::AmbiguousMethod, unmet: None })),
            Pass::Nothing => {}
        }
        // Nothing visible applies: a set-aside candidate that applies at some step makes it
        // E0624 (rustc's `MethodError::PrivateMatch`), not "no method".
        for i in 0..steps.len() {
            let step = steps[i];
            let unsize = unsize_at == Some(i);
            for autoref in [None, Some(Mutbl::Not), Some(Mutbl::Mut)] {
                if unsize && autoref.is_none() {
                    continue;
                }
                let adjusted = match autoref {
                    None => step.ty,
                    Some(m) => self.mk(TyView::Ref(m, step.ty))?,
                };
                for &c in private.iter() {
                    if c.is_method && self.probe(|m| m.instantiate_method(c, adjusted, None, &mut false))?.is_some() {
                        return Ok(Err(Miss { code: DiagCode::PrivateMethod, unmet: None }));
                    }
                }
            }
        }
        Ok(Err(if not_method {
            Miss { code: DiagCode::NotAMethod, unmet: None }
        } else if limit {
            Miss { code: DiagCode::RecursionLimit, unmet: None }
        } else {
            Miss { code: DiagCode::NoMethod, unmet: first_unmet }
        }))
    }

    /// The impl's arguments with its self type unified with `self_ty`, or `None`.
    pub(crate) fn impl_args(&mut self, imp: ItemKey, self_ty: Ty, cause: Cause) -> Result<Option<TyList>, Stop> {
        let Some(header) = self.reads.impl_header(imp)? else { return Ok(None) };
        let args = self.fresh_generic_args(imp, 0, header.generics)?;
        let impl_self = self.lower(header.types, header.self_ty, Lower::Args(args), cause)?;
        Ok(if self.unify(impl_self, self_ty)? { Some(args) } else { None })
    }

    /// Instantiates candidate `c` with fresh parent arguments (an impl's, or a trait's `Self`)
    /// and its `self` parameter unified with `adjusted` (probe.rs `xform_self_ty` against the
    /// receiver, `consider_probe`): the parent's arguments and all of them, or `None` when it
    /// does not apply. A candidate is so matched at every step, whichever step it was assembled
    /// from; a bound candidate's `Self` is fixed to its own step's type (`Cand::on`), so it
    /// applies only where the adjusted receiver is its `self` of that type. With `seg`, the method's own arguments are the written ones (confirmation);
    /// without, fresh (the probe, where no diagnostic may be made).
    fn instantiate_method(
        &mut self,
        c: Cand,
        adjusted: Ty,
        seg: Option<(At, Segment)>,
        unmet: &mut bool,
    ) -> Result<Option<(TyList, TyList)>, Stop> {
        // A bound candidate's `Self` is the step it was found at (`Cand::on`).
        let self_ty = match c.on {
            Some(t) => t,
            None => self.fresh()?,
        };
        // Confirmation reports at the call; the probe makes no diagnostic and is rolled back.
        let cause = seg.map_or(Cause::Body, |(at, _)| at.cause());
        let parent = match c.trait_ {
            None => match self.impl_args(c.container, self_ty, cause)? {
                Some(a) => a,
                None => return Ok(None),
            },
            Some(_) => self.trait_args_for(c.item, self_ty)?,
        };
        let Some(sig) = self.reads.fn_sig(c.item)? else { return Ok(None) };
        let own = match seg {
            Some((at, s)) => self.explicit_fn_args(at, Some(s), c.item, &sig)?,
            None => self.fresh_generic_args(c.item, sig.parent_count, sig.own_count)?,
        };
        let args = self.concat(parent, own)?;
        let Some(&first) = sig.inputs.first() else { return Ok(None) };
        let self_in = self.lower(sig.types, first, Lower::Args(args), cause)?;
        if !self.unify(self_in, adjusted)? {
            return Ok(None);
        }
        if let Some(tr) = c.trait_ {
            if !self.ty.may_hold(Goal::Trait { trait_: tr, args: parent }, &mut self.reads)? {
                return Ok(None);
            }
        } else if !self.impl_clauses_may_hold(c.container, parent, cause)? {
            *unmet = true;
            return Ok(None);
        }
        Ok(Some((parent, args)))
    }

    /// Whether every where-clause of the inherent impl `imp` may hold at its arguments `args`:
    /// an inherent candidate whose impl's clause is decided false does not apply, at this step
    /// and autoref, and the probe goes on (rustc_hir_typeck method/probe.rs `consider_probe`,
    /// `InherentImplCandidate`: "Check whether the impl imposes obligations we have to worry
    /// about", the impl's `clauses_of` registered and any error `ProbeResult::NoMatch`). So the
    /// "autoref specialization" two crates turso reaches rely on picks as rustc does:
    /// - pyo3 0.29 impl_/wrap.rs: `converter(&r).wrap(r)` at the step
    ///   `IntoPyObjectConverter<Result<(), PyErr>>` finds `impl<T: IntoPyObject> ..<T>` and
    ///   `impl<T: IntoPyObject, E> ..<Result<T, E>>`; only the second's clauses hold, so it is
    ///   the pick, not E0034 (turso c1851p, 22 errors).
    /// - miette 7.6 macro_helpers.rs: `OptionalWrapper::<SourceSpan>::new().to_option(&s)` finds
    ///   `impl<T: IsOption> OptionalWrapper<T>` by value, refused since `SourceSpan: IsOption`
    ///   does not hold, and then the trait `ToOption for &OptionalWrapper<T>` at the autoref,
    ///   not E0277 at the inherent pick.
    /// The answer has three values (end-state.md D193 point 5: unknown is not absent):
    /// - `Ok(false)`: some clause is decided false. The candidate is dropped (rustc's NoMatch).
    /// - `Ok(true)`: every clause holds or may hold. A clause still undecided because an
    ///   argument is a variable keeps the candidate, as rustc's `try_evaluate_obligations`
    ///   reports only true errors.
    /// - `Err(Stop)`: a clause the world could not read or answer (`Stop::Need`, or the gap
    ///   it becomes). This is the candidate kept as unknown. The probe tries clauses only after
    ///   the candidate's `self` matched the adjusted receiver, and it stops at the first group
    ///   where any candidate applies. So an unknown candidate is always either the pick or one
    ///   of an ambiguous group, and the body is "not checked: <the gap>" (a note), never an
    ///   error and never a quiet drop. A candidate whose `self` does not match is never asked,
    ///   so the right pick beside it stands (suite/inherent_impl_clauses.rs, `r` and `s`).
    fn impl_clauses_may_hold(&mut self, imp: ItemKey, args: TyList, cause: Cause) -> Result<bool, Stop> {
        let wc = self.reads.predicates(imp)?;
        for &clause in wc.clauses.iter() {
            let Some(goal) = self.clause_goal(wc.types, clause, args, cause)? else { continue };
            if !self.ty.may_hold(goal, &mut self.reads)? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// The first where-clause of inherent candidate `c`'s impl that cannot hold, with its
    /// `self` matched to `adjusted`. These are the probe's `unsatisfied_predicates`, which
    /// rustc's E0599 lists: "the method `get` exists for struct `W<B>`, but its trait bounds
    /// were not satisfied". It runs outside a probe, so the goal's terms stay in the table
    /// for writeback. It is asked only after the probe found nothing, for the candidate it
    /// dropped by this clause.
    fn unmet_impl_clause(&mut self, c: Cand, adjusted: Ty) -> Result<Option<PendingGoal>, Stop> {
        let self_ty = self.fresh()?;
        let Some(parent) = self.impl_args(c.container, self_ty, Cause::Body)? else { return Ok(None) };
        let Some(sig) = self.reads.fn_sig(c.item)? else { return Ok(None) };
        let own = self.fresh_generic_args(c.item, sig.parent_count, sig.own_count)?;
        let args = self.concat(parent, own)?;
        let Some(&first) = sig.inputs.first() else { return Ok(None) };
        let self_in = self.lower(sig.types, first, Lower::Args(args), Cause::Body)?;
        if !self.unify(self_in, adjusted)? {
            return Ok(None);
        }
        let wc = self.reads.predicates(c.container)?;
        for &clause in wc.clauses.iter() {
            let Some(goal) = self.clause_goal(wc.types, clause, parent, Cause::Body)? else { continue };
            if !self.ty.may_hold(goal, &mut self.reads)? {
                return Ok(match goal {
                    Goal::Trait { trait_, args } => Some(PendingGoal::Trait { trait_, args }),
                    Goal::Projection { trait_, args, name, term } => Some(PendingGoal::Projection { trait_, args, name, term }),
                    // A well-formedness, `where const`, or `for<F in fields(..)>` clause: no trait goal to name.
                    Goal::WellFormed(_) | Goal::ConstHolds { .. } | Goal::ForFields { .. } => None,
                });
            }
        }
        Ok(None)
    }

    /// `ConfirmContext::confirm`: the inputs after `self` and the output.
    fn confirm(&mut self, e: ExprId, receiver: ExprId, pick: &Pick<'a>, seg: Segment) -> Result<Option<(TyList, Ty)>, Stop> {
        let step_ty = pick.steps[pick.step].ty;
        let adjusted = match pick.autoref {
            None => step_ty,
            Some(m) => self.mk(TyView::Ref(m, step_ty))?,
        };
        let Some((parent, args)) = self.instantiate_method(pick.cand, adjusted, Some((At::Expr(e), seg)), &mut false)? else {
            return Ok(None);
        };
        // The receiver's adjustments: derefs to the step (to the array before an unsize), the
        // autoref, the unsize.
        let last_deref = if pick.unsize { pick.step - 1 } else { pick.step };
        self.adjust_autoderef(receiver, &pick.steps, last_deref);
        // A by-value pick at a `&T` or `&mut T` step reborrows: one more deref and the borrow
        // again (probe.rs `pick_by_value_method`; Relax's `pick_all_method`, adjustment 0).
        if pick.autoref.is_none() && !pick.unsize {
            if let TyView::Ref(m, inner) = self.view(step_ty) {
                self.adjust.push((receiver, AdjustKind::Deref { overloaded: None }, inner));
                let kind = AdjustKind::Borrow { mutbl: m, two_phase: m == Mutbl::Mut, raw: false };
                self.adjust.push((receiver, kind, step_ty));
            }
        }
        if let Some(m) = pick.autoref {
            let base = pick.steps[last_deref].ty;
            let borrowed = self.mk(TyView::Ref(m, base))?;
            let kind = AdjustKind::Borrow { mutbl: m, two_phase: m == Mutbl::Mut, raw: false };
            self.adjust.push((receiver, kind, borrowed));
            if pick.unsize {
                self.adjust.push((receiver, AdjustKind::Unsize, adjusted));
            }
        }
        self.callees.push((e, pick.cand.item, args));
        #[cfg(test)]
        let drop_clauses = self.plant == crate::machine::Plant::MethodClausesDropped;
        #[cfg(not(test))]
        let drop_clauses = false;
        if !drop_clauses {
            self.register_predicates(pick.cand.item, args, Cause::Expr(e))?;
        }
        if let Some(tr) = pick.cand.trait_ {
            self.register_trait(tr, parent, Cause::Expr(e))?;
        }
        let Some((all, output)) = self.fn_def_sig(pick.cand.item, args, Cause::Expr(e))? else { return Ok(None) };
        let all = self.list(all);
        let rest = self.mk_list(all.get(1..).unwrap_or(&[]))?;
        Ok(Some((rest, output)))
    }
}
