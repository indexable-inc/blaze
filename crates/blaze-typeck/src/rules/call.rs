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
//! last after a selection (`Machine::next_arg`). Omitted for now: overloaded calls through the
//! `Fn*` traits on other types, and the expected-input deduction from the expected output
//! (`expected_inputs_for_expected_output`), which only sharpens hints.

use blaze_own::Vec;
use core::alloc::Allocator;

use crate::diag::{At, DiagCode};
use crate::error::Stop;
use crate::ids::{ExprId, ItemKey, List};
use crate::machine::{Expectation, Flow, Frame, Lower, Machine, Then};
use crate::ty::{Ty, TyList, TyView, TypeCtx};
use crate::world::World;

impl<W: World, C: TypeCtx, A: Allocator + Clone> Machine<'_, '_, '_, W, C, A> {
    /// A fn item's inputs and output, instantiated with `args`.
    pub(crate) fn fn_def_sig(&mut self, k: ItemKey, args: TyList) -> Result<Option<(TyList, Ty)>, Stop> {
        let Some(sig) = self.reads.fn_sig(k)? else { return Ok(None) };
        let mut inputs: Vec<Ty, A> = Vec::with_capacity_in(sig.inputs.len(), self.alloc.clone());
        for &t in sig.inputs {
            inputs.push(self.lower(sig.types, t, Lower::Args(args))?);
        }
        let output = self.lower(sig.types, sig.output, Lower::Args(args))?;
        let inputs = self.mk_list(&inputs)?;
        Ok(Some((inputs, output)))
    }

    /// A fn pointer's (or a closure signature's) inputs and output.
    pub(crate) fn fn_ptr_sig(&mut self, l: TyList) -> Result<(TyList, Ty), Stop> {
        let mut all = self.list(l);
        let output = all.pop().unwrap_or(self.common.unit);
        Ok((self.mk_list(&all)?, output))
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
                        TyView::FnDef(k, fargs) => match self.fn_def_sig(k, fargs)? {
                            Some(sig) => {
                                self.callees.push((e, k, fargs));
                                Some(sig)
                            }
                            None => None,
                        },
                        TyView::FnPtr(l) => Some(self.fn_ptr_sig(l)?),
                        TyView::Closure(_, sig) => match self.view(sig) {
                            TyView::FnPtr(l) => Some(self.fn_ptr_sig(l)?),
                            _ => None,
                        },
                        TyView::Error => {
                            let errs = self.err_list(arg_ids.len())?;
                            return self.start_args(f, e, arg_ids, errs, self.common.err, 2, false);
                        }
                        _ => None,
                    };
                    if let Some((inputs, output)) = sig {
                        self.adjust_autoderef(callee, &steps, i);
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
