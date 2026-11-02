//! `become f(args)`: rustc's tail call (`rustc_mir_build`'s `expr/stmt.rs`, `ExprKind::Become`,
//! and `builder/scope.rs`, `break_for_tail_call`; MIT or Apache-2.0). The callee and the
//! arguments are evaluated in a scope of their own, as for a call. Then what every other scope
//! holds is dropped, innermost first, and the block ends in `TailCall`. The arguments are moved
//! into the call, so leaving does not drop them; a drop that unwinds drops them first, in the
//! order they were written, and then goes on as it would have.

use super::*;

impl<'a, 'f, 'db> MirLowerCtx<'a, 'f, 'db> {
    /// `become call`, where `call` is the call `become_expr` makes. Nothing comes after it.
    pub(super) fn lower_become(
        &mut self,
        call: ExprId,
        become_expr: ExprId,
        current: BasicBlockId,
    ) -> Result<'db, Option<BasicBlockId>> {
        let scope = self.push_drop_scope();
        let Some((func, args, current)) = self.become_operands(call, current)? else {
            scope.pop_assume_dropped(self);
            return Ok(None);
        };
        let span = MirSpan::from(become_expr);
        let current = self.drop_for_tail_call(&args, current, span);
        self.set_terminator(
            current,
            TerminatorKind::TailCall { func, args: args.into_boxed_slice(), fn_span: call.into() },
            span,
        );
        scope.pop_assume_dropped(self);
        Ok(None)
    }

    /// The callee and the arguments of `call`, evaluated from `current` on in the order
    /// `lower_call_and_args` evaluates them, the callee first when it is a value.
    fn become_operands(
        &mut self,
        call: ExprId,
        mut current: BasicBlockId,
    ) -> Result<'db, Option<(Operand, Vec<Operand>, BasicBlockId)>> {
        let store = self.store;
        let mut exprs = Vec::new_in(self.frame);
        let func = match store.expr(call) {
            Expr::Call { callee, args, .. } => {
                if let Some((func_id, generic_args)) = self.infer.method_resolution(call) {
                    let ty = Ty::new_fn_def(
                        self.interner(),
                        CallableDefId::FunctionId(func_id).into(),
                        generic_args,
                    );
                    exprs.push(callee);
                    exprs.extend(args.iter().copied());
                    Operand::from_bytes(Box::default(), ty)
                } else {
                    let callee_ty = self.expr_ty_after_adjustments(callee);
                    match callee_ty.kind() {
                        TyKind::FnDef(def, _) => {
                            let CallableDefId::FunctionId(func) = def.0 else {
                                return Err(MirLowerError::TypeError(
                                    "`become` of a struct's or variant's constructor",
                                ));
                            };
                            // `#[rustc_legacy_const_generics]`: those are generic arguments.
                            let signature = FunctionSignature::of(self.db, func);
                            let consts = signature
                                .legacy_const_generics_indices(self.db, func)
                                .filter(|_| args.len() > signature.params.len())
                                .unwrap_or_default();
                            exprs.extend(
                                args.iter()
                                    .enumerate()
                                    .filter(|(idx, _)| !consts.contains(&(*idx as u32)))
                                    .map(|(_, &arg)| arg),
                            );
                            Operand::from_bytes(Box::default(), callee_ty)
                        }
                        TyKind::FnPtr(..) => {
                            let Some((func, c)) =
                                self.lower_expr_to_some_operand(callee, current)?
                            else {
                                return Ok(None);
                            };
                            current = c;
                            exprs.extend(args.iter().copied());
                            func
                        }
                        TyKind::Error(_) => {
                            return Err(MirLowerError::MissingFunctionDefinition(
                                self.owner, call,
                            ));
                        }
                        _ => return Err(MirLowerError::TypeError("`become` of a call on a bad type")),
                    }
                }
            }
            Expr::MethodCall { receiver, args, method_name, .. } => {
                let (func_id, generic_args) =
                    self.infer.method_resolution(call).ok_or_else(|| {
                        MirLowerError::UnresolvedMethod(
                            // alloc: error path
                            method_name.display(self.db, self.edition()).to_string(),
                        )
                    })?;
                exprs.push(receiver);
                exprs.extend(args.iter().copied());
                Operand::from_fn(self.db, func_id, generic_args)
            }
            _ => return Err(MirLowerError::TypeError("`become` of something that is not a call")),
        };
        let declared = self.declared_locals();
        let mut staged = Vec::with_capacity_in(exprs.len(), self.frame);
        for &arg in exprs.iter() {
            let Some((temp, c)) = self.lower_call_operand(arg, current)? else {
                return Ok(None);
            };
            current = c;
            // What unwinds from the next argument drops this one first.
            self.declare_operands_last(call, declared, std::iter::once(&temp));
            staged.push(temp);
        }
        self.declare_operands_last(call, declared, staged.iter());
        // alloc: the arguments, kept in the body at their final size
        let mut args = Vec::with_capacity(staged.len());
        args.extend(staged.drain(..));
        Ok(Some((func, args, current)))
    }

    /// rustc's `break_for_tail_call`: drops what every scope but the call's own holds, the
    /// innermost first and each the last declared first, ending the storage of each, from
    /// `current` on. What unwinds out of one of those drops drops the arguments, then what it
    /// would drop at that scope's end.
    fn drop_for_tail_call(
        &mut self,
        args: &[Operand],
        mut current: BasicBlockId,
        span: MirSpan,
    ) -> BasicBlockId {
        let mut moved = Vec::with_capacity_in(args.len(), self.frame);
        for arg in args {
            if let OperandKind::Copy(place) | OperandKind::Move(place) = &arg.kind
                && place.projection.as_slice().is_empty()
                && !self.is_copy(self.local(place.local).ty.as_ref())
            {
                moved.push(place.local);
            }
        }
        let mut order = mem::replace(&mut self.order_buf, Vec::new_in(self.frame));
        let mut written = mem::replace(&mut self.written_buf, Vec::new_in(self.frame));
        let call_scope = self.drop_scopes.len() - 1;
        for index in (0..call_scope).rev() {
            order.clear();
            written.clear();
            self.drop_scopes[index].in_order_into(&self.operand_locals, &mut order);
            written.push(current);
            for (at, &local) in order.iter().enumerate().rev() {
                if !self.is_copy(self.local(local).ty.as_ref()) {
                    let after = self.unwind_from_drop(&order, at);
                    let unwind = self.unwind_through_arguments(&moved, after);
                    let prev = mem::replace(&mut current, self.new_basic_block());
                    written.push(current);
                    self.set_terminator(
                        prev,
                        TerminatorKind::Drop { place: Place::from(local).store(), target: current, unwind },
                        span,
                    );
                }
                self.push_statement(current, StatementKind::StorageDead(local).with_span(span));
            }
            self.drop_scopes[index].exits.extend_from_slice(&written);
        }
        self.order_buf = order;
        self.written_buf = written;
        current
    }

    /// The way out that drops `moved`, the first first, then goes on to `after`: rustc's
    /// entry point of a tail call's drop, the arguments' drops added over the next unwind node.
    fn unwind_through_arguments(
        &mut self,
        moved: &[LocalId],
        after: Option<BasicBlockId>,
    ) -> Option<BasicBlockId> {
        let mut at = after;
        for &local in moved.iter().rev() {
            at = Some(self.unwind_node(at, local, true));
        }
        at
    }
}
