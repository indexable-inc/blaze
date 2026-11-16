//! Calls of fn items, fn pointers and closures.
//!
//! Follows rustc_hir_typeck/src/callee.rs (`check_call`, `try_overloaded_call_step`,
//! `confirm_builtin_call`) and fn_ctxt/checks.rs (`check_argument_types`); Relax maps them in
//! typeck.rs (`check_expr_inner`'s call arm, `check_args_of_wrong_count`) and typeck/call.rs.
//!
//! The callee is checked with no expectation and resolved structurally; then autoderef finds
//! the first callable step: a fn item (its signature instantiated with the item's arguments),
//! a fn pointer, or a closure (its signature directly, as rustc does while the closure's kind
//! is not yet known: a deferred call resolution). Arguments are checked in two passes, closures
//! last after a selection (`Machine::next_arg`). Any other step (a type parameter bounded by
//! `Fn*`, `dyn Fn*`, a reference to either) is an overloaded call through the `Fn*` traits
//! (`Machine::overloaded_call`). The expected-input deduction from the expected output
//! (`expected_inputs_for_expected_output`) is `Machine::expected_inputs`
//! (rules/expect_inputs.rs), for a fn item, a fn pointer and a closure; an overloaded call and
//! a method call do not ask it yet.

use blaze_own::{Alloc, Vec};

use crate::columns::AdjustKind;
use crate::diag::{At, DiagCode};
use crate::error::Stop;
use crate::ids::{ExprId, ItemKey, LangItem, List, Mutbl, Name};
use crate::machine::{Expectation, Flow, Frame, Lower, Machine, Then};
use crate::ty::{Cause, Goal, Ty, TyList, TyView, TypeCtx};
use crate::world::World;

impl<'a, W: World, C: TypeCtx> Machine<'_, '_, '_, 'a, W, C> {
    /// A fn item's inputs and output, instantiated with `args`.
    /// `cause` is the place that instantiates it: a projection in the signature that fails is
    /// reported there.
    pub(crate) fn fn_def_sig(&mut self, k: ItemKey, args: TyList, cause: Cause) -> Result<Option<(TyList, Ty)>, Stop> {
        let Some(sig) = self.reads.fn_sig(k)? else { return Ok(None) };
        let mut inputs: Vec<Ty, Alloc<'a>> = Vec::with_capacity_in(sig.inputs.len(), self.alloc);
        for &t in sig.inputs {
            inputs.push(self.lower(sig.types, t, Lower::Args(args), cause)?);
        }
        let output = self.lower(sig.types, sig.output, Lower::Args(args), cause)?;
        let inputs = self.mk_list(&inputs)?;
        Ok(Some((inputs, output)))
    }

    /// A fn pointer's (or a closure signature's) inputs and output.
    pub(crate) fn fn_ptr_sig(&mut self, l: TyList) -> Result<(TyList, Ty), Stop> {
        let mut all = self.list(l);
        let output = all.pop().unwrap_or(self.common.unit);
        Ok((self.mk_list(&all)?, output))
    }

    /// `try_overloaded_call_traits`: `Fn`, then `FnMut`, then `FnOnce` (rustc's order), each
    /// with a fresh `Args`; the first whose `callee: Tr<Args>` may hold is the call. Its method
    /// (`call`, `call_mut`, `call_once`, all `extern "rust-call"` in core's
    /// library/core/src/ops/function.rs) takes `Args` as one tuple at the type level, so the
    /// written arguments are checked against the tuple's fields (`TupleArgumentsFlag::
    /// TupleArguments`), and the output is `<T as FnOnce<Args>>::Output`. `Fn` borrows the
    /// callee, `FnMut` borrows it mutably, `FnOnce` takes it by value. `None` when no trait may
    /// hold (the next autoderef step is tried).
    fn overloaded_call(&mut self, e: ExprId, callee: ExprId, t: Ty, n: usize) -> Result<Option<(TyList, Ty)>, Stop> {
        let tries = [
            (LangItem::Fn, "call", Some(Mutbl::Not)),
            (LangItem::FnMut, "call_mut", Some(Mutbl::Mut)),
            (LangItem::FnOnce, "call_once", None),
        ];
        // A world without the three traits cannot decide the call: the one finding naming the
        // first it lacks, and the call is checked no further.
        let mut traits = [None; 3];
        for (slot, (item, ..)) in traits.iter_mut().zip(tries) {
            let Some(tr) = self.lang(item)? else {
                self.missing_lang(item, At::Expr(e));
                return Ok(Some((self.err_list(n)?, self.common.err)));
            };
            *slot = Some(tr);
        }
        let Some(once) = traits[2] else { return Ok(None) };
        for ((_, method, borrow), tr) in tries.into_iter().zip(traits) {
            let Some(tr) = tr else { continue };
            let tuple = self.fresh()?;
            let args = self.mk_list(&[t, tuple])?;
            if !self.ty.may_hold(Goal::Trait { trait_: tr, args }, &mut self.reads)? {
                continue;
            }
            self.register_trait(tr, args, Cause::Expr(e))?;
            let output = self.project(once, args, Name::of("Output"), Cause::Expr(e))?;
            if let Some(m) = self.reads.assoc_item(tr, Name::of(method))? {
                self.callees.push((e, m.item, args));
            }
            if let Some(mutbl) = borrow {
                let borrowed = self.mk(TyView::Ref(mutbl, t))?;
                self.adjust.push((callee, AdjustKind::Borrow { mutbl, two_phase: false, raw: false }, borrowed));
            }
            self.select()?;
            let tuple = self.structurally_resolve(tuple, At::Expr(callee))?;
            return match self.view(tuple) {
                // The count is checked here, as E0057 (the caller passes no count check on).
                TyView::Tuple(inputs) => {
                    if self.ty.list_len(inputs) as usize != n {
                        self.diag(DiagCode::FnTraitArgCount, At::Expr(e), None, None, None);
                        return Ok(Some((self.err_list(n)?, self.common.err)));
                    }
                    Ok(Some((inputs, output)))
                }
                TyView::Error => Ok(Some((self.err_list(n)?, self.common.err))),
                _ => {
                    self.diag(DiagCode::CallArgsNotTuple, At::Expr(callee), None, Some(tuple), None);
                    Ok(Some((self.err_list(n)?, self.common.err)))
                }
            };
        }
        Ok(None)
    }

    pub(crate) fn expr_call(&mut self, f: Frame, e: ExprId, callee: ExprId, args: List) -> Result<Flow, Stop> {
        let arg_ids = self.body.expr_list(args)?;
        match f.state {
            0 => {
                self.resume(f, 1)?;
                self.push_expr(callee, Expectation::None, Then::Nothing)?;
                Ok(Flow::Wait)
            }
            1 => {
                let callee_ty = self.structurally_resolve(self.ret, At::Expr(callee))?;
                let (steps, limit) = self.autoderef(callee_ty, false)?;
                for (i, step) in steps.iter().enumerate() {
                    let sig = match self.view(step.ty) {
                        TyView::FnDef(k, fargs) => match self.fn_def_sig(k, fargs, Cause::Expr(e))? {
                            Some(sig) => {
                                self.callees.push((e, k, fargs));
                                Some(sig)
                            }
                            None => None,
                        },
                        TyView::FnPtr(l, _) => Some(self.fn_ptr_sig(l)?),
                        TyView::Closure(_, sig) | TyView::ClosureOf { sig, .. } => match self.view(sig) {
                            TyView::FnPtr(l, _) => {
                                let (inputs, output) = self.fn_ptr_sig(l)?;
                                // A closure is called through its `Fn*` impl (the Reference,
                                // "Call expressions": a closure operand's call is
                                // `Fn::call(&f, (args..))`, its arguments one `Args` tuple), so a
                                // wrong count is the Fn-trait call's E0057, not a fn item's E0061.
                                if self.ty.list_len(inputs) as usize != arg_ids.len() {
                                    self.adjust_autoderef(callee, &steps, i);
                                    self.diag(DiagCode::FnTraitArgCount, At::Expr(e), None, None, None);
                                    return self.start_args(f, e, arg_ids, inputs, output, 2, false);
                                }
                                Some((inputs, output))
                            }
                            _ => None,
                        },
                        TyView::Error => {
                            let errs = self.err_list(arg_ids.len())?;
                            return self.start_args(f, e, arg_ids, errs, self.common.err, 2, false);
                        }
                        _ if self.is_ty_var(step.ty) => None,
                        _ => {
                            let before = self.adjust.len();
                            self.adjust_autoderef(callee, &steps, i);
                            match self.overloaded_call(e, callee, step.ty, arg_ids.len())? {
                                Some((inputs, output)) => {
                                    return self.start_args(f, e, arg_ids, inputs, output, 2, false);
                                }
                                None => {
                                    self.adjust.truncate(before);
                                    None
                                }
                            }
                        }
                    };
                    if let Some((inputs, output)) = sig {
                        self.adjust_autoderef(callee, &steps, i);
                        // The call's expected type decides what it can of the formal inputs
                        // before an argument is typed (rules/expect_inputs.rs).
                        self.expected_inputs(f.expect, inputs, output)?;
                        return self.start_args(f, e, arg_ids, inputs, output, 2, true);
                    }
                }
                let code = if limit { DiagCode::RecursionLimit } else { DiagCode::NotCallable };
                self.diag(code, At::Expr(callee), None, Some(callee_ty), None);
                let errs = self.err_list(arg_ids.len())?;
                self.start_args(f, e, arg_ids, errs, self.common.err, 2, false)
            }
            _ => {
                if self.next_arg(f, arg_ids, 2)? { Ok(Flow::Wait) } else { Ok(Flow::Done(f.u)) }
            }
        }
    }
}
