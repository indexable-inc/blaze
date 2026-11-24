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
use crate::diag::{About, At, DiagCode};
use crate::error::Stop;
use crate::ids::{ExprId, ItemKey, LangItem, List, Name};
use crate::machine::{Expectation, Flow, Frame, Lower, Machine, Then};
use crate::ty::{Cause, Goal, Ty, TyList, TyView, TypeCtx};
use crate::world::World;

impl<'a, W: World, C: TypeCtx> Machine<'_, '_, '_, 'a, W, C> {
    /// A fn item's inputs and output, instantiated with `args`.
    /// `cause` is the place that instantiates it: a projection in the signature that fails is
    /// reported there.
    pub(crate) fn fn_def_sig(&mut self, k: ItemKey, args: TyList, cause: Cause) -> Result<Option<(TyList, Ty)>, Stop> {
        let Some(sig) = self.reads.fn_sig(k)? else { return Ok(None) };
        let first_row = self.projected.len();
        let mut inputs: Vec<Ty, Alloc<'a>> = Vec::with_capacity_in(sig.inputs.len(), self.alloc);
        for &t in sig.inputs {
            inputs.push(self.lower(sig.types, t, Lower::Args(args), cause)?);
        }
        if k == self.body.owner && self.inferred_output_ty.is_some() {
            self.diag(DiagCode::InferredErrorSetCycle, At::of(cause), About::found(self.common.err));
            let inputs = self.mk_list(&inputs)?;
            return Ok(Some((inputs, self.common.err)));
        }
        // A trait fn's `impl Trait` at a concrete `Self` is the impl's type (rules/rpitit_use.rs).
        self.rpitit = self.rpitit_rows(k, sig.types, sig.output, args)?;
        let output = self.lower(sig.types, sig.output, Lower::Args(args), cause);
        self.rpitit.clear();
        let output = output?;
        if self.ty.list_len(args) != 0
            && let Cause::Expr(call) = cause
            && matches!(self.body.expr(call)?, crate::body::Expr::Call { .. } | crate::body::Expr::MethodCall { .. })
        {
            self.record_lifetime_impls(call, k, args, first_row)?;
        }
        let inputs = self.mk_list(&inputs)?;
        Ok(Some((inputs, output)))
    }

    /// Every trait bound of the callee, instantiated at this call (`[Self, trait args..]`), for
    /// the same selection that resolves projections (src/selected.rs, "Bound rows"): any
    /// subject, any trait, any arity. A bound proves a lifetime relation whether or not its
    /// trait names a lifetime (`T: Marker` at `T = &'x u8` with `impl Marker for &'static u8`
    /// makes `'x: 'static`), and `T: Deserialize<'de>` consumes `'de` though the callee
    /// returns plain `T`. Which bounds are kept is decided at writeback, where the instance is
    /// resolved (writeback/selected_rows.rs); where the caller's own where-clause may answer a
    /// ground row instead of an impl is decided there too. A clause whose written arguments
    /// hold a projection is not noted ([`crate::selected::holds_alias`]).
    fn record_lifetime_impls(&mut self, call: ExprId, item: ItemKey, args: TyList, first: usize) -> Result<(), Stop> {
        let preds = self.reads.predicates(item)?;
        for &clause in preds.clauses {
            let (trait_, name, formal) = match clause {
                crate::world::PClause::Trait { trait_, args: formal } => (trait_, None, formal),
                crate::world::PClause::Projection { trait_, args: formal, name, .. } => (trait_, Some(name), formal),
                _ => continue,
            };
            let actual = self.lower_list(preds.types, formal, args, Cause::Expr(call))?;
            // One row per trait reference (or projection) at a call: a projection row of the
            // same trait at the same arguments (`T: Tr` beside `T::Out`) is this bound's row too,
            // since a projection is proven through its trait reference's source.
            let dup = match name {
                None => self.projected[first..].iter().any(|&(e, tr, _, a)| e == call && tr == trait_ && a == actual),
                Some(n) => self.projected[first..].iter().any(|&(e, tr, m, a)| e == call && tr == trait_ && m == Some(n) && a == actual),
            };
            if !dup {
                self.projected.push((call, trait_, name, actual));
            }
        }
        Ok(())
    }

    /// A fn pointer's (or a closure signature's) inputs and output.
    pub(crate) fn fn_ptr_sig(&mut self, l: TyList) -> Result<(TyList, Ty), Stop> {
        let mut all = self.list(l);
        let output = all.pop().unwrap_or(self.common.unit);
        Ok((self.mk_list(&all)?, output))
    }

    /// Probe each callable trait with a fresh argument tuple. The selected method determines
    /// the receiver adjustment and the result projection. For an async call the expression
    /// produces the call future. Awaiting that future yields the declared output.
    fn overloaded_call(&mut self, e: ExprId, callee: ExprId, t: Ty, n: usize) -> Result<Option<(TyList, Ty)>, Stop> {
        let mut missing = None;
        for method in super::closure::CALL_METHODS {
            let Some(tr) = self.lang(method.trait_)? else {
                if missing.is_none() && matches!(method.trait_, LangItem::Fn | LangItem::FnMut | LangItem::FnOnce) {
                    missing = Some(method.trait_);
                }
                continue;
            };
            let tuple = self.fresh()?;
            let args = self.mk_list(&[t, tuple])?;
            if !self.ty.may_hold(Goal::Trait { trait_: tr, args }, &mut self.reads)? {
                continue;
            }
            self.register_trait(tr, args, Cause::Expr(e))?;
            let Some(owner) = self.need_lang(method.owner, At::Expr(e))? else {
                return Ok(Some((self.err_list(n)?, self.common.err)));
            };
            let output = self.project(owner, args, Name::of(method.result), Cause::Expr(e))?;
            if let Some(m) = self.reads.assoc_item(tr, Name::of(method.method))? {
                self.callees.push((e, m.item, args));
            }
            if let Some(mutbl) = method.borrow {
                let borrowed = self.mk(TyView::Ref(mutbl, t))?;
                self.adjust.push((callee, AdjustKind::Borrow { mutbl, two_phase: false, raw: false }, borrowed));
            }
            self.select()?;
            let tuple = self.structurally_resolve(tuple, At::Expr(callee))?;
            return match self.view(tuple) {
                // The count is checked here, as E0057 (the caller passes no count check on).
                TyView::Tuple(inputs) => {
                    if self.ty.list_len(inputs) as usize != n {
                        self.diag(DiagCode::FnTraitArgCount, At::Expr(e), About::Count { expected: self.ty.list_len(inputs), got: n as u32, found: Some(t) });
                        return Ok(Some((self.err_list(n)?, self.common.err)));
                    }
                    Ok(Some((inputs, output)))
                }
                TyView::Error => Ok(Some((self.err_list(n)?, self.common.err))),
                _ => {
                    self.diag(DiagCode::CallArgsNotTuple, At::Expr(callee), About::found(tuple));
                    Ok(Some((self.err_list(n)?, self.common.err)))
                }
            };
        }
        if let Some(item) = missing {
            self.missing_lang(item, At::Expr(e));
            return Ok(Some((self.err_list(n)?, self.common.err)));
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
                            Some((inputs, output)) => {
                                self.callees.push((e, k, fargs));
                                // A C-variadic `k`: one fresh input per argument past the
                                // declared ones (rules/c_variadic.rs).
                                let inputs = self.c_variadic_inputs(k, inputs, arg_ids.len())?;
                                // `#[rustc_legacy_const_generics]`: the const parameters' types at
                                // the listed positions (rules/legacy_const.rs).
                                let inputs = self.legacy_const_inputs(callee, k, inputs, arg_ids.len())?;
                                Some((inputs, output))
                            }
                            None => None,
                        },
                        // Calling through `unsafe extern "C" fn(A, ...)` needs the variable
                        // arguments' promotions and the variadic call ABI on a fn pointer
                        // callee (rules/c_variadic.rs reads them from a fn item's signature).
                        TyView::FnPtr(_, h) if h.c_variadic => {
                            return Err(crate::error::CheckError::Unported("a call through a C-variadic fn pointer").into());
                        }
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
                                    let count = About::Count { expected: self.ty.list_len(inputs), got: arg_ids.len() as u32, found: Some(callee_ty) };
                                    self.diag(DiagCode::FnTraitArgCount, At::Expr(e), count);
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
                        self.expected_inputs(f.expect, inputs, output, arg_ids)?;
                        return self.start_args(f, e, arg_ids, inputs, output, 2, true);
                    }
                }
                let code = if limit { DiagCode::RecursionLimit } else { DiagCode::NotCallable };
                self.diag(code, At::Expr(callee), About::found(callee_ty));
                let errs = self.err_list(arg_ids.len())?;
                self.start_args(f, e, arg_ids, errs, self.common.err, 2, false)
            }
            _ => {
                if self.next_arg(f, arg_ids, 2)? {
                    return Ok(Flow::Wait);
                }
                // Every argument is typed: the `...` ones are held to C's promotions (E0617).
                self.check_c_variadic_args(e, arg_ids)?;
                // The arguments at legacy const generic positions give the callee's constants.
                self.bind_legacy_consts(e, callee, arg_ids)?;
                Ok(Flow::Done(f.u))
            }
        }
    }
}
