//! Explicit tail calls (`become <call_expr>`, RFC 3407): evaluates the callee and arguments,
//! runs all active scope drops BEFORE transferring control, reuses incoming caller-owned
//! `Pass::Ref` parameter buffers for outgoing `Pass::Ref` arguments (e.g. `evm2::interpreter::dispatch`'s
//! `RawStack<'_>`), and emits real `Op::TailCall` / `Op::TailCallIndirect` for O(1) stack execution.
//! Falls back cleanly to a normal call + return when tail-call-ineligible (e.g. coroutines,
//! `byval`, stacked arguments, unmatched `Pass::Ref` arguments, or return-value adjustments).

use core::alloc::Allocator;

use blaze_lir::builder::Variadic;
use blaze_lir::func::{Conv, Ext, ParamKind, RefKind, SlotId};
use blaze_lir::{Flags, Ty, Value};
use blaze_own::Vec;
use blaze_typeck::{CtorKind, Def, Expr, ExprId, PNode};

use super::{Callee, Pass, Repr, Task, TrapKind, V, Walk, malformed, pointer_abi, unsupported};
use crate::{Abi, BuildError, Instance};

impl<A: Allocator + Copy> Walk<'_, '_, '_, A> {
    /// Evaluates `become value` (RFC 3407 explicit tail call): schedules `Task::BecomeCall` for
    /// direct fn, fn pointer, and concrete method calls when outside a coroutine and with no
    /// return-value adjustments; otherwise falls back cleanly to a normal call + return.
    pub(super) fn eval_become(&mut self, value: ExprId) -> Result<(), BuildError> {
        let mut call_e = value;
        for _ in 0..16 {
            if let Expr::Paren(inner) = self.expr(call_e)? {
                call_e = inner;
            } else {
                break;
            }
        }
        if self.co.is_some() || !self.adjust_rows(value).is_empty() || !self.adjust_rows(call_e).is_empty() {
            self.work.push(Task::Return(true));
            self.work.push(Task::Eval(value));
            return Ok(());
        }
        match self.expr(call_e)? {
            Expr::Call { callee, args } => {
                let list = self.body.expr_lists.get(args.range()).ok_or(malformed("a call's arguments past `expr_lists`"))?;
                let t = self.adjusted_ty(callee)?;
                match self.node_at(callee, t)? {
                    PNode::FnPtr(_, header) if pointer_abi(header.abi) != Some(Abi::RustCall) => {
                        self.work.push(Task::BecomeCall(call_e));
                        for &a in list.iter().rev() {
                            if let Repr::Agg(_) = self.adjusted_repr(a)? {
                                self.work.push(Task::ArgCopy(a));
                            }
                            self.work.push(Task::Eval(a));
                        }
                        self.work.push(Task::Eval(callee));
                        return Ok(());
                    }
                    PNode::FnDef(item, fn_args)
                        if !matches!(self.expr_def(callee), Some(Def::Ctor { kind: CtorKind::Tuple, .. }))
                            && self.adts.abi(item) != Some(Abi::RustCall)
                            && self.adts.intrinsic(item).is_none()
                            && self.dyn_slot(call_e, Instance { item, args: fn_args })?.is_none() =>
                    {
                        let listed = self.legacy_listed(callee, item, list.len())?;
                        self.work.push(Task::BecomeCall(call_e));
                        for (k, &a) in list.iter().enumerate().rev() {
                            if listed.iter().any(|&x| usize::try_from(x).ok() == Some(k)) {
                                continue;
                            }
                            if let Repr::Agg(_) = self.adjusted_repr(a)? {
                                self.work.push(Task::ArgCopy(a));
                            }
                            self.work.push(Task::Eval(a));
                        }
                        self.work.push(Task::Discard);
                        self.work.push(Task::Eval(callee));
                        return Ok(());
                    }
                    _ => {}
                }
            }
            Expr::MethodCall { receiver, args, .. } => {
                let instance = self.method_callee(call_e)?;
                if self.adts.abi(instance.item) != Some(Abi::RustCall) && self.dyn_slot(call_e, instance)?.is_none() {
                    let list = self.body.expr_lists.get(args.range()).ok_or(malformed("a call's arguments past `expr_lists`"))?;
                    self.work.push(Task::BecomeCall(call_e));
                    for &a in list.iter().rev() {
                        if let Repr::Agg(_) = self.adjusted_repr(a)? {
                            self.work.push(Task::ArgCopy(a));
                        }
                        self.work.push(Task::Eval(a));
                    }
                    if let Repr::Agg(_) = self.adjusted_repr(receiver)? {
                        self.work.push(Task::ArgCopy(receiver));
                    }
                    self.work.push(Task::Eval(receiver));
                    return Ok(());
                }
            }
            _ => {}
        }
        self.work.push(Task::Return(true));
        self.work.push(Task::Eval(value));
        Ok(())
    }

    /// Completes `Task::BecomeCall(e)` by resolving the callee and arguments and emitting a real
    /// `Op::TailCall` / `Op::TailCallIndirect` (or falling back to a normal call + return).
    pub(super) fn become_call(&mut self, e: ExprId) -> Result<(), BuildError> {
        let mut list = Vec::new_in(self.alloc);
        let (is_ptr, abi, instance_opt, declared) = match self.expr(e)? {
            Expr::Call { callee, args } => {
                let written = self.body.expr_lists.get(args.range()).ok_or(malformed("a call's arguments past `expr_lists`"))?;
                let t = self.adjusted_ty(callee)?;
                match self.node_at(callee, t)? {
                    PNode::FnDef(item, fn_args) => {
                        let listed = self.legacy_listed(callee, item, written.len())?;
                        for (k, &a) in written.iter().enumerate() {
                            if !listed.iter().any(|&x| usize::try_from(x).ok() == Some(k)) {
                                list.push(a);
                            }
                        }
                        let instance = Instance { item, args: fn_args };
                        let abi = self.adts.abi(item).ok_or(BuildError::NoAbi { item })?;
                        let declared = self.adts.c_variadic(item).and_then(|d| usize::try_from(d).ok());
                        if declared.is_some() {
                            Self::c_variadic_abi(e, abi, self.adts.nounwind(item))?;
                        }
                        (false, abi, Some(instance), declared)
                    }
                    PNode::FnPtr(sig_list, header) => {
                        let abi = pointer_abi(header.abi)
                            .ok_or_else(|| unsupported(e, "a fn pointer outside the Rust, rust-call and C-family calling conventions"))?;
                        for &a in written {
                            list.push(a);
                        }
                        let declared = if header.c_variadic {
                            let len = self.ground.view().list(sig_list).map_err(|_| malformed("a fn pointer past the ground table"))?.len();
                            Some(len.saturating_sub(1))
                        } else {
                            None
                        };
                        (true, abi, None, declared)
                    }
                    _ => return Err(malformed("a `become` call task on an unexpected callee")),
                }
            }
            Expr::MethodCall { receiver, args, .. } => {
                list.push(receiver);
                let args_slice = self.body.expr_lists.get(args.range()).ok_or(malformed("a call's arguments past `expr_lists`"))?;
                for &a in args_slice {
                    list.push(a);
                }
                let instance = self.method_callee(e)?;
                let item = instance.item;
                let abi = self.adts.abi(item).ok_or(BuildError::NoAbi { item })?;
                let declared = self.adts.c_variadic(item).and_then(|d| usize::try_from(d).ok());
                if declared.is_some() {
                    Self::c_variadic_abi(e, abi, self.adts.nounwind(item))?;
                }
                (false, abi, Some(instance), declared)
            }
            _ => return Err(malformed("a `become` call task on another expression")),
        };

        let item_opt = instance_opt.map(|i| i.item);
        let n = list.len();
        let mut reprs = Vec::with_capacity_in(n + 1, self.alloc);
        let mut passes = Vec::with_capacity_in(n + 1, self.alloc);
        for (k, &a) in list.iter().enumerate() {
            let t = self.adjusted_ty(a)?;
            let g = self.ground_ty(t).map_err(|error| Self::ty_err(a, error))?;
            let repr = self.repr_at(a, t)?;
            reprs.push(repr);
            passes.push(match declared {
                Some(d) if k >= d => self.unnamed_c_pass(a, g, repr)?,
                _ => self.pass(g, repr, abi, item_opt)?,
            });
        }
        let ty = self.expr_ty(e)?;
        let g = self.ground_ty(ty).map_err(|error| Self::ty_err(e, error))?;
        let rrepr = self.repr_at(e, ty)?;
        let rpass = self.pass(g, rrepr, abi, item_opt)?;

        let (lir_callee, callee_nounwind) = if is_ptr {
            let at = self.vals.len().checked_sub(reprs.len() + 1).ok_or(malformed("a call's pointer missing from the value stack (a build bug)"))?;
            let p = self.remove_val(at);
            let p = self.scalar(p, Ty::PTR)?;
            (Callee::Pointer(p), false)
        } else {
            let instance = instance_opt.ok_or(malformed("a direct `become` call without an instance"))?;
            let loc = self.caller_arg(Some(e), instance)?;
            let nounwind = self.adts.nounwind(instance.item) == Some(true);
            let place = self.ref_place(instance);
            let reference = self.b.reference(RefKind::Func, place);
            if let Some(loc) = loc {
                if declared.is_some() {
                    return Err(malformed("a C-variadic fn that is `#[track_caller]` (E0737)"));
                }
                reprs.push(Repr::Ptr);
                passes.push(Pass::Scalar { ty: Ty::PTR, ext: Ext::None, agg: false });
                self.push_val(V::Scalar(loc), None);
            }
            (Callee::Direct(reference), nounwind)
        };

        self.emit_tail_call(e, lir_callee, &reprs, &passes, declared, callee_nounwind, rrepr, rpass)
    }

    /// Emits `Op::TailCall` / `Op::TailCallIndirect` when tail-call-eligible, or falls back
    /// cleanly to a normal call + return.
    #[expect(clippy::too_many_arguments, reason = "call site, callee, argument reprs/passes, variadic/nounwind flags, and return repr/pass")]
    fn emit_tail_call(
        &mut self,
        e: ExprId,
        callee: Callee,
        reprs: &[Repr],
        passes: &[Pass],
        declared: Option<usize>,
        callee_nounwind: bool,
        rrepr: Repr,
        rpass: Pass,
    ) -> Result<(), BuildError> {
        let mut can_tail = declared.is_none()
            && self.co.is_none()
            && !passes.iter().any(|p| matches!(p, Pass::Memory { .. }));

        let sig_pair = can_tail.then(|| self.sig_of(passes, rpass));
        if let Some((params, rets)) = sig_pair.as_ref() {
            let (_, own_rets) = self.sig_of(&[], self.ret_pass);
            let need_sret = matches!(rpass, Pass::Ref | Pass::Memory { .. });
            let have_sret = matches!(self.ret_pass, Pass::Ref | Pass::Memory { .. }) && self.sret.is_some();
            if rets.as_slice() != own_rets.as_slice() || need_sret != have_sret {
                can_tail = false;
            }
            if can_tail {
                let max_gpr = if cfg!(target_arch = "x86_64") { 6u32 } else { 8u32 };
                let mut gpr = 0u32;
                let mut fpr = 0u32;
                for p in params.iter() {
                    if p.kind == ParamKind::Byval {
                        can_tail = false;
                        break;
                    }
                    if p.kind == ParamKind::Sret {
                        if cfg!(target_arch = "x86_64") {
                            gpr += 1;
                        }
                        continue;
                    }
                    if p.ty.is_float() {
                        fpr += 1;
                    } else if p.ty == Ty::I128 {
                        if !cfg!(target_arch = "x86_64") {
                            gpr = (gpr + 1) & !1;
                        }
                        gpr += 2;
                    } else {
                        gpr += 1;
                    }
                }
                if gpr > max_gpr || fpr > 8 {
                    can_tail = false;
                }
            }
        }
        let mut ref_match: Vec<Option<Value>, A> = Vec::with_capacity_in(passes.len(), self.alloc);
        if can_tail {
            let mut used: Vec<bool, A> = Vec::with_capacity_in(self.ref_params.len(), self.alloc);
            for _ in 0..self.ref_params.len() {
                used.push(false);
            }
            for (&p, &repr) in passes.iter().zip(reprs.iter()) {
                if p == Pass::Ref {
                    let (need_size, need_align) = self.size_align(repr);
                    let mut chosen = None;
                    for (used_slot, &(ptr, in_size, in_align)) in used.iter_mut().zip(self.ref_params.iter()) {
                        if !*used_slot && in_size >= need_size && in_align >= need_align {
                            *used_slot = true;
                            chosen = Some(ptr);
                            break;
                        }
                    }
                    if chosen.is_none() {
                        can_tail = false;
                        break;
                    }
                    ref_match.push(chosen);
                } else {
                    ref_match.push(None);
                }
            }
        }

        let Some((params, rets)) = sig_pair.filter(|_| can_tail) else {
            self.emit_call_from(callee, reprs, passes, declared, callee_nounwind, rrepr, rpass)?;
            if let PNode::Never = self.node_at(e, self.expr_ty(e)?)? {
                self.pop_val();
                self.unreachable_of(TrapKind::AfterDivergingCall, Some(e));
                self.dead_block();
                self.push_val(V::None, None);
                return Ok(());
            }
            let v = self.pop()?;
            self.ret_value(v)?;
            self.dead_block();
            self.push_val(V::None, None);
            return Ok(());
        };

        let n = reprs.len();
        let base = self.vals.len().checked_sub(n).ok_or(malformed("a `become` call's arguments missing from the value stack (a build bug)"))?;
        let mut values = Vec::with_capacity_in(params.len(), self.alloc);
        if matches!(rpass, Pass::Ref | Pass::Memory { .. }) {
            let sret = self.sret.ok_or(malformed("an aggregate `become` result without its pointer"))?;
            values.push(sret);
        }
        let mut arg_temps: Vec<SlotId, A> = Vec::new_in(self.alloc);
        let mut ref_copies: Vec<(Repr, (Value, i32), Value), A> = Vec::new_in(self.alloc);
        for i in 0..n {
            let v = *self.vals.get(base + i).ok_or(malformed("a `become` argument value missing"))?;
            let &pass = passes.get(i).ok_or(malformed("a `become` argument pass missing"))?;
            let &repr = reprs.get(i).ok_or(malformed("a `become` argument repr missing"))?;
            if pass == Pass::Ref {
                let dst_ptr = ref_match
                    .get(i)
                    .copied()
                    .flatten()
                    .ok_or(malformed("a `Pass::Ref` tail-call arg without a matched buffer"))?;
                let mut src = self.place(v, repr)?;
                if self.ref_params.iter().any(|&(rp, _, _)| rp == src.0) {
                    let tmp = self.temp(repr);
                    self.copy(repr, src, tmp)?;
                    src = tmp;
                }
                if let Some(slot) = self.whole_slot(src) {
                    arg_temps.push(slot);
                }
                values.push(dst_ptr);
                ref_copies.push((repr, src, dst_ptr));
            } else {
                if let V::Place(p, off) = v
                    && let Some(slot) = self.whole_slot((p, off))
                {
                    arg_temps.push(slot);
                }
                let _ = self.pass_arg_into(v, repr, pass, |x| values.push(x))?;
            }
        }
        // Drop all active locals and parameters BEFORE transferring control (RFC 3407).
        self.drop_from(0)?;
        self.truncate_vals(base);
        for (repr, src, dst_ptr) in ref_copies {
            self.copy(repr, src, (dst_ptr, 0))?;
        }
        for slot in arg_temps {
            self.end_temp_slot(slot);
        }
        let sig = self.b.sig(Conv::C, Variadic::No, &params, &rets);
        match callee {
            Callee::Direct(reference) => self.b.tail_call(Flags::NONE, reference, sig, &values),
            Callee::Pointer(p) => self.b.tail_call_indirect(Flags::NONE, p, sig, &values),
        }
        self.dead_block();
        self.push_val(V::None, None);
        Ok(())
    }
}
