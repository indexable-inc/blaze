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
//! Omitted for now: raw-pointer autoref (`pick_const_ptr_method`), the 2021 `into_iter` array
//! shadowing, unstable-candidate and private-candidate reporting, and `self: Box<Self>`-style
//! receivers beyond what autoderef reaches.

use blaze_own::{Alloc, Vec};

use crate::body::Segment;
use crate::columns::AdjustKind;
use crate::diag::{At, DiagCode};
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
                            self.diag(DiagCode::NoMethod, At::Expr(e), None, Some(recv), Some(name));
                            let errs = self.err_list(arg_ids.len())?;
                            return self.start_args(f, e, arg_ids, errs, self.common.err, 2, false);
                        };
                        self.start_args(f, e, arg_ids, inputs, output, 2, true)
                    }
                    Err(code) => {
                        self.diag(code, At::Expr(e), None, Some(recv), Some(name));
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

    /// `probe_op` in `Mode::MethodCall`: the pick, or the code of the error.
    fn probe_method(&mut self, recv: Ty, name: Name, scope: ScopeId) -> Result<Result<Pick<'a>, DiagCode>, Stop> {
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
                    ext.push(Cand { container: tr, item: it.item, trait_: Some(tr), is_method: self_param });
                }
            }
        }
        // The inherent candidates of every step, assembled once before any is tried
        // (`assemble_inherent_candidates` over all steps, as Relax's `assemble_probe_candidates`):
        // an inherent `fn m(&self)` of `Foo` is found by value at the `&Foo` step, before any
        // extension candidate of `&Foo`.
        let mut not_method = false;
        let mut inh: Vec<Cand, Alloc<'a>> = Vec::new_in(self.alloc);
        let mut bounds: Vec<ItemKey, Alloc<'a>> = Vec::new_in(self.alloc);
        for i in 0..steps.len() {
            let step = steps[i];
            if let Some(shape) = self.shape(step.ty) {
                for it in self.reads.inherent(shape, name)? {
                    if let AssocKind::Fn { self_param } = it.kind {
                        let c = Cand { container: it.container, item: it.item, trait_: None, is_method: self_param };
                        if !inh.iter().any(|x| x.item == c.item) {
                            inh.push(c);
                        }
                    }
                }
            }
            if let TyView::Param(p) = self.view(step.ty) {
                bounds.clear();
                self.param_bound_traits(p, &mut bounds)?;
                for &tr in bounds.iter() {
                    if let Some(it) = self.reads.assoc_item(tr, name)? {
                        if let AssocKind::Fn { self_param } = it.kind {
                            let c = Cand { container: tr, item: it.item, trait_: Some(tr), is_method: self_param };
                            if !inh.iter().any(|x| x.item == c.item) {
                                inh.push(c);
                            }
                        }
                    }
                }
            }
        }
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
                    let cands = if group == 0 { &inh } else { &ext };
                    for &c in cands.iter() {
                        if group == 1 && inh.iter().any(|x| x.item == c.item) {
                            continue;
                        }
                        if !c.is_method {
                            not_method = true;
                            continue;
                        }
                        if self.probe(|m| m.instantiate_method(c, adjusted, None))?.is_some() {
                            count += 1;
                            if found.is_none() {
                                found = Some(c);
                            }
                        }
                    }
                    if count > 1 {
                        return Ok(Err(DiagCode::AmbiguousMethod));
                    }
                    if let Some(cand) = found {
                        return Ok(Ok(Pick { cand, steps, step: i, autoref, unsize }));
                    }
                }
            }
        }
        Ok(Err(if not_method {
            DiagCode::NotAMethod
        } else if limit {
            DiagCode::RecursionLimit
        } else {
            DiagCode::NoMethod
        }))
    }

    /// The impl's arguments with its self type unified with `self_ty`, or `None`.
    pub(crate) fn impl_args(&mut self, imp: ItemKey, self_ty: Ty) -> Result<Option<TyList>, Stop> {
        let Some(header) = self.reads.impl_header(imp)? else { return Ok(None) };
        let args = self.fresh_args(header.generics)?;
        let impl_self = self.lower(header.types, header.self_ty, Lower::Args(args))?;
        Ok(if self.unify(impl_self, self_ty)? { Some(args) } else { None })
    }

    /// Instantiates candidate `c` with fresh parent arguments (an impl's, or a trait's `Self`)
    /// and its `self` parameter unified with `adjusted` (probe.rs `xform_self_ty` against the
    /// receiver, `consider_probe`): the parent's arguments and all of them, or `None` when it
    /// does not apply. A candidate is so matched at every step, whichever step it was assembled
    /// from. With `seg`, the method's own arguments are the written ones (confirmation);
    /// without, fresh (the probe, where no diagnostic may be made).
    fn instantiate_method(
        &mut self,
        c: Cand,
        adjusted: Ty,
        seg: Option<(At, Segment)>,
    ) -> Result<Option<(TyList, TyList)>, Stop> {
        let self_ty = self.fresh()?;
        let parent = match c.trait_ {
            None => match self.impl_args(c.container, self_ty)? {
                Some(a) => a,
                None => return Ok(None),
            },
            Some(_) => self.trait_args_for(c.item, self_ty)?,
        };
        let Some(sig) = self.reads.fn_sig(c.item)? else { return Ok(None) };
        let own = match seg {
            Some((at, s)) => self.explicit_args(at, Some(s), sig.own_count)?,
            None => self.fresh_args(sig.own_count)?,
        };
        let args = self.concat(parent, own)?;
        let Some(&first) = sig.inputs.first() else { return Ok(None) };
        let self_in = self.lower(sig.types, first, Lower::Args(args))?;
        if !self.unify(self_in, adjusted)? {
            return Ok(None);
        }
        if let Some(tr) = c.trait_ {
            if !self.ty.may_hold(Goal::Trait { trait_: tr, args: parent }, &mut self.reads)? {
                return Ok(None);
            }
        }
        Ok(Some((parent, args)))
    }

    /// `ConfirmContext::confirm`: the inputs after `self` and the output.
    fn confirm(&mut self, e: ExprId, receiver: ExprId, pick: &Pick<'a>, seg: Segment) -> Result<Option<(TyList, Ty)>, Stop> {
        let step_ty = pick.steps[pick.step].ty;
        let adjusted = match pick.autoref {
            None => step_ty,
            Some(m) => self.mk(TyView::Ref(m, step_ty))?,
        };
        let Some((parent, args)) = self.instantiate_method(pick.cand, adjusted, Some((At::Expr(e), seg)))? else {
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
        self.register_predicates(pick.cand.item, args, Cause::Expr(e))?;
        if let Some(tr) = pick.cand.trait_ {
            self.register_trait(tr, parent, Cause::Expr(e))?;
        }
        let Some((all, output)) = self.fn_def_sig(pick.cand.item, args)? else { return Ok(None) };
        let all = self.list(all);
        let rest = self.mk_list(all.get(1..).unwrap_or(&[]))?;
        Ok(Some((rest, output)))
    }
}
