//! MIR lowering for places

use hir_def::{FunctionId, hir::UnsafeBinderCastKind};

use super::*;
use crate::{
    mir::{MutBorrowKind, Operand, OperandKind},
    next_solver::{Region, RegionExt},
};

impl<'db> MirLowerCtx<'_, '_, 'db> {
    /// A temporary is dropped before the ones that were made while computing its value.
    fn declare_last(&mut self, local: LocalId) {
        let scope = self.drop_scopes.iter_mut().rev().find(|it| it.locals.holds(local));
        if let Some(scope) = scope
            && let Some((&last, before)) = scope.locals.split_last()
            && (last != local || before.contains(&local))
        {
            let locals = scope.locals.edit();
            locals.retain(|&it| it != local);
            locals.push(local);
        }
    }

    fn lower_expr_to_some_place_without_adjust(
        &mut self,
        expr_id: ExprId,
        prev_block: BasicBlockId,
    ) -> Result<'db, Option<(Place<'db>, BasicBlockId)>> {
        let ty = self.expr_ty_without_adjust(expr_id);
        let place: Place<'db> = self.temp(ty, prev_block, expr_id.into())?.into();
        self.unschedule(place.local);
        let lowered = self.lower_expr_to_place_without_adjust(expr_id, place, prev_block);
        self.reschedule(place.local);
        let Some(current) = lowered? else {
            return Ok(None);
        };
        self.declare_last(place.local);
        Ok(Some((place, current)))
    }

    pub(super) fn lower_expr_to_some_place_with_adjust(
        &mut self,
        expr_id: ExprId,
        prev_block: BasicBlockId,
        adjustments: &[Adjustment],
    ) -> Result<'db, Option<(Place<'db>, BasicBlockId)>> {
        let ty = adjustments
            .last()
            .map(|it| it.target.as_ref())
            .unwrap_or_else(|| self.expr_ty_without_adjust(expr_id));
        let place: Place<'db> = self.temp(ty, prev_block, expr_id.into())?.into();
        self.unschedule(place.local);
        let lowered = self.lower_expr_to_place_with_adjust(expr_id, place, prev_block, adjustments);
        self.reschedule(place.local);
        let Some(current) = lowered? else {
            return Ok(None);
        };
        self.declare_last(place.local);
        Ok(Some((place, current)))
    }

    /// A place expression inside another goes as deep as the source does: the stack grows
    /// for it as for `lower_expr_to_place_without_adjust`.
    pub(super) fn lower_expr_as_place_with_adjust(
        &mut self,
        current: BasicBlockId,
        expr_id: ExprId,
        upgrade_rvalue: bool,
        adjustments: &[Adjustment],
    ) -> Result<'db, Option<(Place<'db>, BasicBlockId)>> {
        crate::next_solver::ensure_sufficient_stack(|| {
            self.lower_expr_as_place_with_adjust_here(current, expr_id, upgrade_rvalue, adjustments)
        })
    }

    fn lower_expr_as_place_with_adjust_here(
        &mut self,
        current: BasicBlockId,
        expr_id: ExprId,
        upgrade_rvalue: bool,
        adjustments: &[Adjustment],
    ) -> Result<'db, Option<(Place<'db>, BasicBlockId)>> {
        let try_rvalue = |this: &mut MirLowerCtx<'_, '_, 'db>| {
            if !upgrade_rvalue {
                return Err(MirLowerError::MutatingRvalue);
            }
            this.lower_expr_to_some_place_with_adjust(expr_id, current, adjustments)
        };
        if let Some((last, rest)) = adjustments.split_last() {
            match last.kind {
                Adjust::Deref(None) => {
                    let Some(mut it) = self.lower_expr_as_place_with_adjust(
                        current,
                        expr_id,
                        upgrade_rvalue,
                        rest,
                    )?
                    else {
                        return Ok(None);
                    };
                    it.0 = it.0.project(ProjectionElem::Deref);
                    Ok(Some(it))
                }
                Adjust::Deref(Some(od)) => {
                    let Some((r, current)) = self.lower_expr_as_place_with_adjust(
                        current,
                        expr_id,
                        upgrade_rvalue,
                        rest,
                    )?
                    else {
                        return Ok(None);
                    };
                    self.lower_overloaded_deref(
                        current,
                        r,
                        rest.last()
                            .map(|it| it.target.as_ref())
                            .unwrap_or_else(|| self.expr_ty_without_adjust(expr_id)),
                        last.target.as_ref(),
                        expr_id.into(),
                        match od.0 {
                            Mutability::Mut => true,
                            Mutability::Not => false,
                        },
                    )
                }
                Adjust::NeverToAny | Adjust::Borrow(_) | Adjust::Pointer(_) => try_rvalue(self),
            }
        } else {
            self.lower_expr_as_place_without_adjust(current, expr_id, upgrade_rvalue)
        }
    }

    pub(super) fn lower_expr_as_place(
        &mut self,
        current: BasicBlockId,
        expr_id: ExprId,
        upgrade_rvalue: bool,
    ) -> Result<'db, Option<(Place<'db>, BasicBlockId)>> {
        match self.infer.expr_adjustments.get(&expr_id) {
            Some(a) => self.lower_expr_as_place_with_adjust(current, expr_id, upgrade_rvalue, a),
            None => self.lower_expr_as_place_without_adjust(current, expr_id, upgrade_rvalue),
        }
    }

    pub(super) fn lower_expr_as_place_without_adjust(
        &mut self,
        current: BasicBlockId,
        expr_id: ExprId,
        upgrade_rvalue: bool,
    ) -> Result<'db, Option<(Place<'db>, BasicBlockId)>> {
        let try_rvalue = |this: &mut MirLowerCtx<'_, '_, 'db>| {
            if !upgrade_rvalue {
                return Err(MirLowerError::MutatingRvalue);
            }
            this.lower_expr_to_some_place_without_adjust(expr_id, current)
        };
        match self.store.expr(expr_id) {
            Expr::Path(p) => {
                let resolver_guard =
                    self.resolver.update_to_inner_scope(self.db, self.store_owner, expr_id);
                let hygiene = self.store.expr_path_hygiene(expr_id);
                let resolved = self.resolver.resolve_path_in_value_ns_fully(self.db, p, hygiene);
                self.resolver.reset_to_guard(resolver_guard);
                let Some(pr) = resolved else {
                    return try_rvalue(self);
                };
                match pr {
                    ValueNs::LocalBinding(pat_id) => match self.guard_refs.get(&pat_id) {
                        Some(&reference) => Ok(Some((
                            Place::from(reference).project(ProjectionElem::Deref),
                            current,
                        ))),
                        None => Ok(Some((self.binding_local(pat_id)?.into(), current))),
                    },
                    ValueNs::StaticId(s) => {
                        let ty = self.expr_ty_without_adjust(expr_id);
                        let ref_ty = Ty::new_ref(
                            self.interner(),
                            Region::new_static(self.interner()),
                            ty,
                            Mutability::Not,
                        );
                        let temp = self.temp(ref_ty, current, expr_id.into())?.into();
                        self.push_assignment(
                            current,
                            temp,
                            Operand { kind: OperandKind::Static(s), span: None }.into(),
                            expr_id.into(),
                        );
                        Ok(Some((temp.project(ProjectionElem::Deref), current)))
                    }
                    _ => try_rvalue(self),
                }
            }
            Expr::UnaryOp { expr, op: hir_def::hir::UnaryOp::Deref } => {
                let is_builtin = match self.expr_ty_without_adjust(expr).kind() {
                    TyKind::Ref(..) | TyKind::RawPtr(..) => true,
                    TyKind::Adt(id, _) => id.is_box(),
                    _ => false,
                };
                if !is_builtin {
                    let Some((mut p, current)) = self.lower_expr_as_place(current, expr, true)?
                    else {
                        return Ok(None);
                    };
                    // Inference borrows the operand itself when the deref is a mutable one, so
                    // the place is that reference and not the value it points to.
                    let borrowed = self.infer.expr_adjustments.get(&expr).is_some_and(|it| {
                        matches!(it.last(), Some(Adjustment { kind: Adjust::Borrow(_), .. }))
                    });
                    if borrowed {
                        p = p.project(ProjectionElem::Deref);
                    }
                    return self.lower_overloaded_deref(
                        current,
                        p,
                        self.expr_ty_without_adjust(expr),
                        self.expr_ty_without_adjust(expr_id),
                        expr_id.into(),
                        'b: {
                            if let Some((f, _)) = self.infer.method_resolution(expr_id)
                                && let Some(deref_fn) = self.lang_items().DerefMut_deref_mut
                            {
                                break 'b deref_fn == f;
                            }
                            false
                        },
                    );
                }
                let Some((mut r, current)) = self.lower_expr_as_place(current, expr, true)? else {
                    return Ok(None);
                };
                r = r.project(ProjectionElem::Deref);
                Ok(Some((r, current)))
            }
            Expr::UnaryOp { .. } => try_rvalue(self),
            Expr::Field { expr, .. } => {
                let Some((mut r, current)) = self.lower_expr_as_place(current, expr, true)? else {
                    return Ok(None);
                };
                self.push_field_projection(&mut r, expr_id)?;
                Ok(Some((r, current)))
            }
            Expr::Index { base, index } => {
                let base_ty = self.expr_ty_after_adjustments(base);
                if !is_builtin_index(self.infer, base, index) {
                    let Some(index_fn) = self.infer.method_resolution(expr_id) else {
                        return Err(MirLowerError::UnresolvedMethod(
                            // alloc: error path
                            "[overloaded index]".to_owned(),
                        ));
                    };
                    let Some((base_place, current)) =
                        self.lower_expr_as_place(current, base, true)?
                    else {
                        return Ok(None);
                    };
                    let Some((index_operand, current)) =
                        self.lower_expr_to_some_operand(index, current)?
                    else {
                        return Ok(None);
                    };
                    return self.lower_overloaded_index(
                        current,
                        base_place,
                        base_ty,
                        self.expr_ty_without_adjust(expr_id),
                        index_operand,
                        expr_id.into(),
                        index_fn,
                    );
                }
                let adjusts = self
                    .infer
                    .expr_adjustments
                    .get(&base)
                    .and_then(|it| it.split_last())
                    .map(|it| it.1)
                    .unwrap_or(&[]);
                // rustc builds a builtin index only of an array or slice place, which the
                // adjustments of a well-typed base reach: `ys[i]` of `ys: &[i32]` derefs it.
                // Without them, inference found no `Index` impl and reported it.
                let indexed = adjusts
                    .last()
                    .map_or_else(|| self.infer.expr_ty(base), |it| it.target.as_ref());
                if !matches!(indexed.kind(), TyKind::Array(..) | TyKind::Slice(..)) {
                    return Err(MirLowerError::TypeError(
                        "a builtin index of a base that is not an array or slice",
                    ));
                }
                let Some((mut p_base, current)) =
                    self.lower_expr_as_place_with_adjust(current, base, true, adjusts)?
                else {
                    return Ok(None);
                };
                let l_index =
                    self.temp(self.expr_ty_after_adjustments(index), current, expr_id.into())?;
                let Some(current) = self.lower_expr_to_place(index, l_index.into(), current)?
                else {
                    return Ok(None);
                };
                p_base = p_base.project(ProjectionElem::Index(l_index));
                Ok(Some((p_base, current)))
            }
            // rustc's `UnwrapUnsafeBinder` place: a temporary would be borrowed or written in
            // place of the binder's value.
            Expr::UnsafeBinderCast { kind: UnsafeBinderCastKind::Unwrap, .. } => {
                let what = "`unwrap_binder!` as a place: MIR has no `UnwrapUnsafeBinder` projection";
                Err(MirLowerError::NotSupported(what.to_owned())) // alloc: error path
            }
            _ => try_rvalue(self),
        }
    }

    fn lower_overloaded_index(
        &mut self,
        current: BasicBlockId,
        place: Place<'db>,
        base_ty: Ty<'db>,
        result_ty: Ty<'db>,
        index_operand: Operand,
        span: MirSpan,
        index_fn: (FunctionId, GenericArgs<'db>),
    ) -> Result<'db, Option<(Place<'db>, BasicBlockId)>> {
        let mutability = match base_ty.as_reference() {
            Some((_, _, mutability)) => mutability,
            None => Mutability::Not,
        };
        let result_ref =
            Ty::new_ref(self.interner(), Region::error(self.interner()), result_ty, mutability);
        let mut result = self.temp(result_ref, current, span)?.into();
        let index_fn_op = Operand::const_zst(Ty::new_fn_def(
            self.interner(),
            CallableDefId::FunctionId(index_fn.0).into(),
            index_fn.1,
        ));
        let Some(current) = self.lower_call(
            index_fn_op,
            // alloc: the call's arguments, kept in the body
            Box::new([
                Operand { kind: OperandKind::Copy(place.store()), span: None },
                index_operand,
            ]),
            result,
            current,
            false,
            span,
        )?
        else {
            return Ok(None);
        };
        result = result.project(ProjectionElem::Deref);
        Ok(Some((result, current)))
    }

    fn lower_overloaded_deref(
        &mut self,
        current: BasicBlockId,
        place: Place<'db>,
        source_ty: Ty<'db>,
        target_ty: Ty<'db>,
        span: MirSpan,
        mutability: bool,
    ) -> Result<'db, Option<(Place<'db>, BasicBlockId)>> {
        let lang_items = self.lang_items();
        let (mutability, deref_fn, borrow_kind) = if !mutability {
            (Mutability::Not, lang_items.Deref_deref, BorrowKind::Shared)
        } else {
            (
                Mutability::Mut,
                lang_items.DerefMut_deref_mut,
                BorrowKind::Mut { kind: MutBorrowKind::Default },
            )
        };
        let error_region = Region::error(self.interner());
        let ty_ref = Ty::new_ref(self.interner(), error_region, source_ty, mutability);
        let target_ty_ref = Ty::new_ref(self.interner(), error_region, target_ty, mutability);
        let ref_place = self.temp(ty_ref, current, span)?.into();
        self.push_assignment(current, ref_place, Rvalue::Ref(borrow_kind, place.store()), span);
        let deref_fn = deref_fn.ok_or(MirLowerError::LangItemNotFound)?;
        let deref_fn_op = Operand::const_zst(Ty::new_fn_def(
            self.interner(),
            CallableDefId::FunctionId(deref_fn).into(),
            GenericArgs::new_from_slice(&[source_ty.into()]),
        ));
        let mut result = self.temp(target_ty_ref, current, span)?.into();
        let Some(current) = self.lower_call(
            deref_fn_op,
            // alloc: the call's arguments, kept in the body
            Box::new([Operand { kind: OperandKind::Copy(ref_place.store()), span: None }]),
            result,
            current,
            false,
            span,
        )?
        else {
            return Ok(None);
        };
        result = result.project(ProjectionElem::Deref);
        Ok(Some((result, current)))
    }
}
