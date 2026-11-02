//! Type inference for expressions.

use std::{iter::repeat_with, mem};

use blaze_hash::FixedState;
use blaze_mem::Frame;
use either::Either;
use hir_def::{
    AdtId, FieldId, TupleFieldId, TupleId, VariantId,
    expr_store::path::{GenericArgs as HirGenericArgs, Path},
    hir::{
        ArithOp, Array, AsmOperand, AsmOptions, BinaryOp, BindingAnnotation, Expr, ExprId,
        ExprOrPatIdPacked, InlineAsmKind, LabelId, LoopSource, Pat, PatId, RecordLitField,
        RecordSpread, Statement, Statements, UnaryOp, UnsafeBinderCastKind,
    },
    resolver::{TypeNs, ValueNs},
    signatures::VariantFields,
    type_ref::TypeRefId,
};
use hir_def::{
    FunctionId,
    hir::{ClosureKind, CoroutineKind},
    lang_item::LangItemTarget,
};
use hir_expand::{mod_path::ModPath, name::Name};
use rustc_ast_ir::Mutability;
use rustc_type_ir::{
    FallibleTypeFolder, InferTy, Interner, TypeFoldable, TypeFolder, TypeVisitable,
    TypeVisitableExt, TypeVisitor, VisitorResult,
    inherent::{IntoKind, Ty as _},
    try_visit,
};
use smallvec::SmallVec;
use tracing::debug;

use crate::{
    Adjust, Adjustment, CallableDefId, Rawness, Span,
    consteval::literal_ty,
    db::HirDatabase,
    infer::{AllowTwoPhase, BreakableKind, coerce::CoerceMany, find_continuable, pat::PatOrigin},
    lower::lower_mutability,
    method_resolution::{self, CandidateId, MethodCallee, MethodError},
    next_solver::{
        ClauseKind, FnSig, IsRigid, StoredTy, Ty, TyKind, TypeError,
        infer::{
            BoundRegionConversionTime, InferOk,
            resolve::ReplaceInferWithError,
            traits::{Obligation, ObligationCause, ObligationCauseCode},
        },
        obligation_ctxt::ObligationCtxt,
    },
};

use super::{
    BreakableContext, CallArguments, Diverges, Expectation, InferenceContext, InferenceDiagnostic,
    ReturnKind, Unreachable, UnreachableAt, cast::CastCheck, find_breakable,
};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExprIsRead {
    Yes,
    No,
}

/// A frame list of foldable values, folded in place: `fudge_inference_if_ok` folds what its
/// closure returns, and `TypeFoldable for Vec<T>` collects a new list in the global allocator.
#[derive(Clone, Debug)]
struct FrameList<'f, T>(Vec<T, &'f Frame>);

impl<I: Interner, T: TypeVisitable<I>> TypeVisitable<I> for FrameList<'_, T> {
    fn visit_with<V: TypeVisitor<I>>(&self, visitor: &mut V) -> V::Result {
        for item in &self.0 {
            try_visit!(item.visit_with(visitor));
        }
        V::Result::output()
    }
}

impl<I: Interner, T: TypeFoldable<I>> TypeFoldable<I> for FrameList<'_, T> {
    fn try_fold_with<F: FallibleTypeFolder<I>>(mut self, folder: &mut F) -> Result<Self, F::Error> {
        for item in &mut self.0 {
            *item = item.clone().try_fold_with(folder)?; // clone: an interned `Ty`, a copied ID
        }
        Ok(self)
    }

    fn fold_with<F: TypeFolder<I>>(mut self, folder: &mut F) -> Self {
        for item in &mut self.0 {
            *item = item.clone().fold_with(folder); // clone: an interned `Ty`, a copied ID
        }
        self
    }
}

impl<'db, 'f> InferenceContext<'db, 'f> {
    pub(crate) fn infer_expr(
        &mut self,
        tgt_expr: ExprId,
        expected: &Expectation<'db>,
        is_read: ExprIsRead,
    ) -> Ty<'db> {
        let ty = self.infer_expr_inner(tgt_expr, expected, is_read);
        if let Some(expected_ty) = expected.only_has_type(&mut self.table) {
            _ = self.demand_eqtype(tgt_expr.into(), expected_ty, ty);
        }
        ty
    }

    pub(crate) fn infer_expr_suptype_coerce_never(
        &mut self,
        expr: ExprId,
        expected: &Expectation<'db>,
        is_read: ExprIsRead,
    ) -> Ty<'db> {
        let ty = self.infer_expr_inner(expr, expected, is_read);
        if ty.is_never() {
            if let Some(adjustments) = self.result.expr_adjustments.get(&expr) {
                return if let [Adjustment { kind: Adjust::NeverToAny, target }] = adjustments {
                    target.as_ref()
                } else {
                    self.err_ty()
                };
            }

            if let Some(target) = expected.only_has_type(&mut self.table) {
                self.coerce(expr, ty, target, AllowTwoPhase::No, ExprIsRead::Yes)
                    .expect("never-to-any coercion should always succeed")
            } else {
                ty
            }
        } else {
            if let Some(expected_ty) = expected.only_has_type(&mut self.table) {
                _ = self.demand_suptype(expr.into(), expected_ty, ty);
            }
            ty
        }
    }

    pub(crate) fn infer_expr_no_expect(
        &mut self,
        tgt_expr: ExprId,
        is_read: ExprIsRead,
    ) -> Ty<'db> {
        self.infer_expr_inner(tgt_expr, &Expectation::None, is_read)
    }

    /// Infer type of expression with possibly implicit coerce to the expected type.
    /// Return the type after possible coercion.
    pub(super) fn infer_expr_coerce(
        &mut self,
        expr: ExprId,
        expected: &Expectation<'db>,
        is_read: ExprIsRead,
    ) -> Ty<'db> {
        let ty = self.infer_expr_inner(expr, expected, is_read);
        if let Some(target) = expected.only_has_type(&mut self.table) {
            match self.coerce(expr, ty, target, AllowTwoPhase::No, is_read) {
                Ok(res) => res,
                Err(_) => {
                    self.emit_type_mismatch(expr.into(), target, ty);
                    target
                }
            }
        } else {
            ty
        }
    }

    /// Whether this expression constitutes a read of value of the type that
    /// it evaluates to.
    ///
    /// This is used to determine if we should consider the block to diverge
    /// if the expression evaluates to `!`, and if we should insert a `NeverToAny`
    /// coercion for values of type `!`.
    ///
    /// This function generally returns `false` if the expression is a place
    /// expression and the *parent* expression is the scrutinee of a match or
    /// the pointee of an `&` addr-of expression, since both of those parent
    /// expressions take a *place* and not a value.
    pub(super) fn expr_guaranteed_to_constitute_read_for_never(
        &mut self,
        expr: ExprId,
        is_read: ExprIsRead,
    ) -> bool {
        // rustc queries parent hir node of `expr` here and determine whether
        // the current `expr` is read of value per its parent.
        // But since we don't have hir node, we cannot follow such "bottom-up"
        // method.
        // So, we pass down such readness from the parent expression through the
        // recursive `infer_expr*` calls in a "top-down" manner.
        // rustc does the place expr check first, but since we are feeding
        // readness of the `expr` as a given value, we just can short-circuit
        // the place expr check if it's true(see codes and comments below)
        is_read == ExprIsRead::Yes
            // We only care about place exprs. Anything else returns an immediate
            // which would constitute a read. We don't care about distinguishing
            // "syntactic" place exprs since if the base of a field projection is
            // not a place then it would've been UB to read from it anyways since
            // that constitutes a read.
            || !self.is_syntactic_place_expr(expr)
    }

    /// Whether this pattern constitutes a read of value of the scrutinee that
    /// it is matching against. This is used to determine whether we should
    /// perform `NeverToAny` coercions.
    fn pat_guaranteed_to_constitute_read_for_never(&self, pat: PatId) -> bool {
        match self.store.pat(pat) {
            // Does not constitute a read.
            Pat::Wild | Pat::Rest => false,

            // This is unnecessarily restrictive when the pattern that doesn't
            // constitute a read is unreachable.
            //
            // For example `match *never_ptr { value => {}, _ => {} }` or
            // `match *never_ptr { _ if false => {}, value => {} }`.
            //
            // It is however fine to be restrictive here; only returning `true`
            // can lead to unsoundness.
            Pat::Or(subpats) => {
                subpats.iter().all(|pat| self.pat_guaranteed_to_constitute_read_for_never(*pat))
            }

            // All of these constitute a read, or match on something that isn't `!`,
            // which would require a `NeverToAny` coercion.
            Pat::Bind { .. }
            | Pat::TupleStruct { .. }
            | Pat::Path(_)
            | Pat::Tuple { .. }
            | Pat::Box { .. }
            | Pat::Deref { .. }
            | Pat::Ref { .. }
            | Pat::Lit(_)
            | Pat::Range { .. }
            | Pat::Slice { .. }
            | Pat::Record { .. }
            | Pat::NotNull
            | Pat::Missing => true,
            Pat::Expr(_) => unreachable!(
                "we don't call pat_guaranteed_to_constitute_read_for_never() with assignments"
            ),
        }
    }

    /// Checks if the pattern contains any `ref` or `ref mut` bindings, and if
    /// yes whether it contains mutable or just immutables ones.
    //
    // FIXME(tschottdorf): this is problematic as the HIR is being scraped, but
    // ref bindings are be implicit after #42640 (default match binding modes). See issue #44848.
    fn contains_explicit_ref_binding(&self, pat: PatId) -> bool {
        if let Pat::Bind { id, .. } = self.store.pat(pat)
            && matches!(self.store[id].mode, BindingAnnotation::Ref | BindingAnnotation::RefMut)
        {
            return true;
        }

        let mut result = false;
        self.store.walk_pats_shallow(pat, |pat| result |= self.contains_explicit_ref_binding(pat));
        result
    }

    fn is_syntactic_place_expr(&mut self, expr: ExprId) -> bool {
        match self.store.expr(expr) {
            // Lang item paths cannot currently be local variables or statics.
            Expr::Path(Path::LangItem(_, _)) => false,
            Expr::Path(Path::Normal(path)) => path.type_anchor.is_none(),
            Expr::Path(path) => {
                let guard = self.resolver.update_to_inner_scope(self.db, self.store_owner, expr);
                let is_place = self
                    .resolver
                    .resolve_path_in_value_ns_fully(
                        self.db,
                        path,
                        self.store.expr_path_hygiene(expr),
                    )
                    .is_none_or(|res| {
                        matches!(res, ValueNs::LocalBinding(_) | ValueNs::StaticId(_))
                    });
                self.resolver.reset_to_guard(guard);
                is_place
            }
            Expr::Underscore => true,
            Expr::UnaryOp { op: UnaryOp::Deref, .. } => true,
            Expr::Field { .. } | Expr::Index { .. } => true,
            // rustc's `is_place_expr`: an unsafe binder cast keeps its operand's place-ness.
            Expr::UnsafeBinderCast { expr, .. } => self.is_syntactic_place_expr(expr),
            Expr::Call { .. }
            | Expr::MethodCall { .. }
            | Expr::Tuple { .. }
            | Expr::If { .. }
            | Expr::Match { .. }
            | Expr::Closure { .. }
            | Expr::Block { .. }
            | Expr::Array(..)
            | Expr::Break { .. }
            | Expr::Continue { .. }
            | Expr::Return { .. }
            | Expr::Become { .. }
            | Expr::Let { .. }
            | Expr::Loop { .. }
            | Expr::InlineAsm(..)
            | Expr::OffsetOf(..)
            | Expr::Literal(..)
            | Expr::Const(..)
            | Expr::UnaryOp { .. }
            | Expr::BinaryOp { .. }
            | Expr::Assignment { .. }
            | Expr::Yield { .. }
            | Expr::Cast { .. }
            | Expr::Await { .. }
            | Expr::Ref { .. }
            | Expr::RecordLit { .. }
            | Expr::Yeet { .. }
            | Expr::Missing
            | Expr::IncludeBytes => false,
        }
    }

    pub(crate) fn check_lhs_assignable(&mut self, lhs: ExprId, assign: ExprId, op: Option<ArithOp>) {
        if self.is_syntactic_place_expr(lhs) {
            return;
        }

        self.push_diagnostic(InferenceDiagnostic::InvalidLhsOfAssignment { lhs, assign, op });
    }

    fn infer_expr_coerce_never(
        &mut self,
        expr: ExprId,
        expected: &Expectation<'db>,
        is_read: ExprIsRead,
    ) -> Ty<'db> {
        let ty = self.infer_expr_inner(expr, expected, is_read);
        // While we don't allow *arbitrary* coercions here, we *do* allow
        // coercions from `!` to `expected`.
        if ty.is_never() {
            if let Some(adjustments) = self.result.expr_adjustments.get(&expr) {
                return if let [Adjustment { kind: Adjust::NeverToAny, target }] = adjustments {
                    target.as_ref()
                } else {
                    self.err_ty()
                };
            }

            if let Some(target) = expected.only_has_type(&mut self.table) {
                self.coerce(expr, ty, target, AllowTwoPhase::No, ExprIsRead::Yes)
                    .expect("never-to-any coercion should always succeed")
            } else {
                ty
            }
        } else {
            if let Some(expected_ty) = expected.only_has_type(&mut self.table) {
                _ = self.demand_eqtype(expr.into(), expected_ty, ty);
            }
            ty
        }
    }

    pub(super) fn infer_expr_inner(
        &mut self,
        tgt_expr: ExprId,
        expected: &Expectation<'db>,
        is_read: ExprIsRead,
    ) -> Ty<'db> {
        // Every expression is inferred and ids are dense, so one in 64 bounds how long a
        // cancelled revision runs on, without two dependent loads per expression.
        if tgt_expr.into_raw().into_u32() % 64 == 0 {
            blaze_query::db::unwind_if_cancelled(self.db);
        }
        // An expression is inferred within the one it is in: rustc's 5,204-arm `else if`.
        crate::next_solver::ensure_sufficient_stack(|| {
            self.infer_expr_inner_here(tgt_expr, expected, is_read)
        })
    }

    fn infer_expr_inner_here(
        &mut self,
        tgt_expr: ExprId,
        expected: &Expectation<'db>,
        is_read: ExprIsRead,
    ) -> Ty<'db> {
        let expr = self.store.expr(tgt_expr);
        // Expressions after siblings that diverged; not inside `.await`'s desugaring, nor of a
        // `try` block's generated `Try::from_output(())`.
        if !matches!(expr, Expr::Await { .. }) && !self.is_try_block_output_of_unit(tgt_expr) {
            self.warn_if_unreachable(UnreachableAt::Expr(tgt_expr));
        }
        // Whether a past expression diverged doesn't affect the typing of this one.
        let old_diverges = mem::replace(&mut self.diverges, Diverges::Maybe);
        let ty = match expr {
            Expr::Missing => self.err_ty(),
            Expr::If { condition, then_branch, else_branch } => {
                let expected = &expected.adjust_for_branches(&mut self.table, tgt_expr.into());
                self.infer_expr_coerce_never(
                    condition,
                    &Expectation::HasType(self.types.types.bool),
                    ExprIsRead::Yes,
                );
                self.warn_if_unreachable(UnreachableAt::IfBlock(tgt_expr));

                let condition_diverges = mem::replace(&mut self.diverges, Diverges::Maybe);

                let then_ty = self.infer_expr_inner(then_branch, expected, ExprIsRead::Yes);
                let then_diverges = mem::replace(&mut self.diverges, Diverges::Maybe);
                let mut coercion_sites = [then_branch, tgt_expr];
                if let Some(else_branch) = else_branch {
                    coercion_sites[1] = else_branch;
                }
                let mut coerce = CoerceMany::with_coercion_sites(
                    expected.coercion_target_type(&mut self.table, then_branch.into()),
                    &coercion_sites,
                );
                coerce.coerce(
                    self,
                    &ObligationCause::new(then_branch),
                    then_branch,
                    then_ty,
                    ExprIsRead::Yes,
                );
                match else_branch {
                    Some(else_branch) => {
                        let else_ty = self.infer_expr_inner(else_branch, expected, ExprIsRead::Yes);
                        let else_diverges = mem::replace(&mut self.diverges, Diverges::Maybe);
                        coerce.coerce(
                            self,
                            &ObligationCause::with_code(
                                else_branch,
                                ObligationCauseCode::IfExpression,
                            ),
                            else_branch,
                            else_ty,
                            ExprIsRead::Yes,
                        );
                        self.diverges = condition_diverges | then_diverges & else_diverges;
                    }
                    None => {
                        coerce.coerce_forced_unit(
                            self,
                            tgt_expr,
                            &ObligationCause::with_code(
                                tgt_expr,
                                ObligationCauseCode::IfExpressionWithNoElse,
                            ),
                            true,
                            ExprIsRead::Yes,
                        );
                        self.diverges = condition_diverges;
                    }
                }

                coerce.complete(self)
            }
            Expr::Let { pat, expr } => {
                // For a `let` statement, `check_stmt` warns of the statement instead.
                self.warn_if_unreachable(UnreachableAt::LetInit(tgt_expr));
                self.infer_let(
                    self.table.next_ty_var(tgt_expr.into()),
                    Some(expr),
                    pat,
                    PatOrigin::LetExpr,
                );
                self.types.types.bool
            }
            Expr::Block { statements, tail, label, id: _, unsafe_: _ } => {
                self.infer_block(tgt_expr, statements, tail, label, expected)
            }
            Expr::Const(id) => {
                // rustc checks an inline const's body with a `FnCtxt` of its own.
                let prev_diverges = mem::replace(&mut self.diverges, Diverges::Maybe);
                let ty = self
                    .with_breakable_ctx(BreakableKind::Border, None, None, |this| {
                        this.infer_expr(id, expected, ExprIsRead::Yes)
                    })
                    .1;
                self.diverges = prev_diverges;
                ty
            }
            Expr::Loop { body, label, source } => {
                let coerce = match source {
                    // you can only use break with a value from a normal `loop { }`
                    LoopSource::Loop => {
                        Some(expected.coercion_target_type(&mut self.table, body.into()))
                    }
                    LoopSource::While | LoopSource::ForLoop => None,
                };
                let (breaks, ()) =
                    self.with_breakable_ctx(BreakableKind::Loop, coerce, label, |this| {
                        this.infer_expr_suptype_coerce_never(
                            body,
                            &Expectation::HasType(this.types.types.unit),
                            ExprIsRead::Yes,
                        );
                    });

                if breaks.may_break {
                    self.diverges = Diverges::Maybe;
                } else {
                    self.diverges |= Diverges::always(tgt_expr);
                }
                breaks.coerce.map(|c| c.complete(self)).unwrap_or(self.types.types.unit)
            }
            Expr::Closure { body, args, ret_type, arg_types, closure_kind, capture_by: _ } => self
                .infer_closure(
                    body,
                    args,
                    ret_type,
                    arg_types,
                    closure_kind,
                    tgt_expr,
                    expected,
                ),
            Expr::Call { callee, args, .. } => self.infer_call(tgt_expr, callee, args, expected),
            Expr::MethodCall { receiver, args, method_name, generic_args } => self
                .infer_method_call(
                    tgt_expr,
                    receiver,
                    args,
                    method_name,
                    generic_args,
                    expected,
                ),
            Expr::Match { expr, arms } => {
                let mut scrutinee_is_read = true;
                let mut contains_ref_bindings = false;
                for arm in arms {
                    scrutinee_is_read &= self.pat_guaranteed_to_constitute_read_for_never(arm.pat);
                    contains_ref_bindings |= self.contains_explicit_ref_binding(arm.pat);
                }
                let scrutinee_is_read =
                    if scrutinee_is_read { ExprIsRead::Yes } else { ExprIsRead::No };
                let input_ty = self.demand_scrutinee_type(
                    expr,
                    contains_ref_bindings,
                    arms.is_empty(),
                    scrutinee_is_read,
                );

                if arms.is_empty() {
                    self.diverges |= Diverges::always(tgt_expr);
                    self.types.types.never
                } else {
                    // rustc's `warn_arms_when_scrutinee_diverges`.
                    for index in 0..arms.len() {
                        let index = index as u32;
                        self.warn_if_unreachable(UnreachableAt::Arm { match_expr: tgt_expr, index });
                    }
                    let matchee_diverges = mem::replace(&mut self.diverges, Diverges::Maybe);
                    let mut all_arms_diverge = Diverges::WarnedAlways;
                    for arm in arms.iter() {
                        self.infer_top_pat(arm.pat, input_ty, PatOrigin::MatchArm);
                    }

                    let expected = expected.adjust_for_branches(&mut self.table, tgt_expr.into());
                    let result_ty = match &expected {
                        // We don't coerce to `()` so that if the match expression is a
                        // statement it's branches can have any consistent type.
                        Expectation::HasType(ty) if *ty != self.types.types.unit => *ty,
                        _ => self.table.next_ty_var(expr.into()),
                    };
                    let mut coerce = CoerceMany::with_capacity(result_ty, arms.len(), self.frame);
                    let mut prior_arms = 0;

                    for arm in arms.iter() {
                        self.diverges = Diverges::Maybe;
                        if let Some(guard_expr) = arm.guard {
                            self.infer_expr_coerce_never(
                                guard_expr,
                                &Expectation::HasType(self.types.types.bool),
                                ExprIsRead::Yes,
                            );
                        }
                        // Not reset after the guard: a guard that diverges makes its arm's
                        // body unreachable, and the arm diverge.

                        let arm_ty = self.infer_expr_inner(arm.expr, &expected, ExprIsRead::Yes);
                        all_arms_diverge &= self.diverges;
                        // An arm of type `!` does not count as a prior arm: it does not shape
                        // the type later arms must agree with.
                        let code = match prior_arms {
                            0 => ObligationCauseCode::BlockTailExpression,
                            prior_arms => ObligationCauseCode::MatchExpressionArm { prior_arms },
                        };
                        coerce.coerce(
                            self,
                            &ObligationCause::with_code(arm.expr, code),
                            arm.expr,
                            arm_ty,
                            ExprIsRead::Yes,
                        );
                        if !arm_ty.is_never() {
                            prior_arms += 1;
                        }
                    }

                    if let Diverges::Always { .. } = all_arms_diverge
                        && !self.is_desugared_match(expr)
                    {
                        all_arms_diverge = Diverges::Always {
                            span: tgt_expr.into(),
                            custom_note: Some(Unreachable::ALL_ARMS_DIVERGE),
                        };
                    }
                    self.diverges = matchee_diverges | all_arms_diverge;

                    coerce.complete(self)
                }
            }
            Expr::Path(p) => self.infer_expr_path(p, tgt_expr.into(), tgt_expr),
            Expr::Continue { label } => {
                if label.is_none() && self.in_labeled_block() {
                    self.push_diagnostic(InferenceDiagnostic::UnlabeledJumpInLabeledBlock {
                        expr: tgt_expr,
                        is_break: false,
                    });
                } else if find_continuable(&self.breakables, label).is_none() {
                    self.push_diagnostic(InferenceDiagnostic::BreakOutsideOfLoop {
                        expr: tgt_expr,
                        is_break: false,
                        bad_value_break: false,
                    });
                };
                self.types.types.never
            }
            Expr::Break { expr, label } => {
                let breakable_idx = find_breakable(&self.breakables, label);
                // rustc reports only E0695 for it, and types it as a jump to the loop.
                let in_labeled_block = label.is_none() && self.in_labeled_block();
                if in_labeled_block {
                    self.push_diagnostic(InferenceDiagnostic::UnlabeledJumpInLabeledBlock {
                        expr: tgt_expr,
                        is_break: true,
                    });
                }
                let val_ty = if let Some(expr) = expr {
                    let opt_coerce_to = match breakable_idx {
                        Some(breakable_idx) => match &self.breakables[breakable_idx].coerce {
                            Some(coerce) => coerce.expected_ty(),
                            None => {
                                if !in_labeled_block {
                                    self.push_diagnostic(InferenceDiagnostic::BreakOutsideOfLoop {
                                        expr: tgt_expr,
                                        is_break: true,
                                        bad_value_break: true,
                                    });
                                }
                                self.err_ty()
                            }
                        },
                        None => self.err_ty(),
                    };
                    self.infer_expr_inner(
                        expr,
                        &Expectation::HasType(opt_coerce_to),
                        ExprIsRead::Yes,
                    )
                } else {
                    self.types.types.unit
                };

                match breakable_idx {
                    Some(breakable_idx) => {
                        let breakable = &mut self.breakables[breakable_idx];

                        // If we encountered a `break`, then (no surprise) it may be possible to break from the
                        // loop... unless the value being returned from the loop diverges itself, e.g.
                        // `break return 5` or `break loop {}`.
                        breakable.may_break |= !self.diverges.is_always();

                        if let Some(mut coerce) = breakable.coerce.take() {
                            let expr = expr.unwrap_or(tgt_expr);
                            coerce.coerce(
                                self,
                                &ObligationCause::new(expr),
                                expr,
                                val_ty,
                                ExprIsRead::Yes,
                            );
                            self.breakables[breakable_idx].coerce = Some(coerce);
                        }
                    }
                    None if in_labeled_block => {}
                    None => {
                        self.push_diagnostic(InferenceDiagnostic::BreakOutsideOfLoop {
                            expr: tgt_expr,
                            is_break: true,
                            bad_value_break: false,
                        });
                    }
                }
                self.types.types.never
            }
            Expr::Return { expr } => self.infer_expr_return(tgt_expr, expr),
            Expr::Become { expr } => self.infer_expr_become(tgt_expr, expr),
            Expr::Yield { expr } => {
                if self.coroutine_desugaring == Some(CoroutineKind::Async) {
                    // rustc lowers it to `{ expr; <error> }`.
                    if let Some(expr) = expr {
                        self.infer_expr_no_expect(expr, ExprIsRead::Yes);
                    }
                    self.push_diagnostic(InferenceDiagnostic::AsyncCoroutinesNotSupported {
                        expr: tgt_expr,
                    });
                    self.types.types.error
                } else if let Some((resume_ty, yield_ty)) = self.resume_yield_tys {
                    if let Some(expr) = expr {
                        self.infer_expr_coerce(
                            expr,
                            &Expectation::has_type(yield_ty),
                            ExprIsRead::Yes,
                        );
                    } else {
                        let unit = self.types.types.unit;
                        let _ = self.coerce(
                            tgt_expr,
                            unit,
                            yield_ty,
                            AllowTwoPhase::No,
                            ExprIsRead::Yes,
                        );
                    }
                    resume_ty
                } else {
                    self.push_diagnostic(InferenceDiagnostic::YieldOutsideCoroutine {
                        expr: tgt_expr,
                    });
                    self.types.types.error
                }
            }
            Expr::Yeet { expr } => {
                if let Some(expr) = expr {
                    self.infer_expr_no_expect(expr, ExprIsRead::Yes);
                }
                self.types.types.never
            }
            Expr::RecordLit { path, fields, spread, .. } => {
                self.infer_record_expr(tgt_expr, expected, path, fields, spread)
            }
            Expr::Field { expr, name } => self.infer_field_access(tgt_expr, expr, name, expected),
            Expr::Await { expr } => self.infer_await_expr(tgt_expr, expr),
            Expr::Cast { expr, type_ref } => {
                let cast_ty = self.make_ty(type_ref);
                let expr_ty =
                    self.infer_expr(expr, &Expectation::Castable(cast_ty), ExprIsRead::Yes);
                // As rustc's `CastCheck::new`, a cast to an unsized type is reported here and is
                // of the error type, so nothing that uses its value reports it again.
                let resolved = self.table.resolve_vars_with_obligations(cast_ty);
                if !resolved.has_infer_types() && !self.table.type_is_sized_modulo_regions(resolved) {
                    self.push_diagnostic(InferenceDiagnostic::CastToUnsized {
                        expr: tgt_expr,
                        cast_ty: resolved.store(),
                    });
                    self.err_ty()
                } else {
                    self.deferred_cast_checks.push(CastCheck::new(tgt_expr, expr, expr_ty, cast_ty));
                    cast_ty
                }
            }
            Expr::Ref { expr, rawness, mutability } => self.infer_ref_expr(
                rawness,
                lower_mutability(mutability),
                expr,
                expected,
                tgt_expr,
            ),
            Expr::UnaryOp { expr, op } => self.infer_unop_expr(op, expr, expected, tgt_expr),
            Expr::BinaryOp { lhs, rhs, op } => match op {
                Some(BinaryOp::Assignment { op: Some(op) }) => {
                    self.infer_assign_op_expr(tgt_expr, op, lhs, rhs, expected)
                }
                Some(op) => self.infer_binop_expr(tgt_expr, op, lhs, rhs, expected),
                None => self.err_ty(),
            },
            Expr::Assignment { target, value } => {
                // In ordinary (non-destructuring) assignments, the type of
                // `lhs` must be inferred first so that the ADT fields
                // instantiations in RHS can be coerced to it. Note that this
                // cannot happen in destructuring assignments because of how
                // they are desugared.
                let lhs_ty = match self.store.pat(target) {
                    // LHS of assignment doesn't constitute reads.
                    Pat::Expr(expr) => {
                        Some(self.infer_expr(expr, &Expectation::none(), ExprIsRead::No))
                    }
                    _ => None,
                };
                let is_destructuring_assignment = lhs_ty.is_none();

                if let Some(lhs_ty) = lhs_ty {
                    self.write_pat_ty(target, lhs_ty);
                    self.infer_expr_coerce(value, &Expectation::has_type(lhs_ty), ExprIsRead::Yes);
                    if let Pat::Expr(lhs) = self.store.pat(target) {
                        self.check_lhs_assignable(lhs, tgt_expr, None);
                    }
                } else {
                    // Let the target pattern constrain the RHS coercion target. In particular,
                    // this allows a diverging RHS to coerce to the pattern's type.
                    let expected_ty = self.table.next_ty_var(target.into());
                    let rhs_ty = self.infer_expr_coerce(
                        value,
                        &Expectation::has_type(expected_ty),
                        ExprIsRead::Yes,
                    );
                    self.infer_top_pat(target, rhs_ty, PatOrigin::DestructuringAssignment);
                }
                if is_destructuring_assignment && self.diverges.is_always() {
                    // Ordinary assignments always return `()`, even when they diverge.
                    // However, rustc lowers destructuring assignments into blocks, and blocks return `!` if they have no tail
                    // expression and they diverge. Therefore, we have to do the same here, even though we don't lower destructuring
                    // assignments into blocks.
                    self.table.new_maybe_never_var(value.into())
                } else {
                    self.types.types.unit
                }
            }
            Expr::Index { base, index } => {
                let base_t = self.infer_expr_no_expect(base, ExprIsRead::Yes);
                let idx_t = self.infer_expr_no_expect(index, ExprIsRead::Yes);

                let base_t = self.structurally_resolve_type(base.into(), base_t);
                match self.lookup_indexing(tgt_expr, base, index, base_t, idx_t) {
                    Some((trait_index_ty, trait_element_ty)) => {
                        // two-phase not needed because index_ty is never mutable
                        self.demand_coerce(
                            index,
                            idx_t,
                            trait_index_ty,
                            AllowTwoPhase::No,
                            ExprIsRead::Yes,
                        );
                        self.table.select_obligations_where_possible();
                        trait_element_ty
                    }
                    None => {
                        let at_error = self.ty_at_error(base_t);
                        self.push_diagnostic(InferenceDiagnostic::CannotIndexInto {
                            expr: tgt_expr,
                            found: base_t.store(),
                            at_error,
                        });
                        self.types.types.error
                    }
                }
            }
            Expr::Tuple { exprs, .. } => {
                let mut tys = Vec::with_capacity_in(exprs.len(), self.frame);
                match expected
                    .only_has_type(&mut self.table)
                    .map(|t| self.table.try_structurally_resolve_type(tgt_expr.into(), t).kind())
                {
                    Some(TyKind::Tuple(substs)) => tys.extend(
                        substs
                            .iter()
                            .chain(repeat_with(|| self.table.next_ty_var(Span::Dummy)))
                            .take(exprs.len()),
                    ),
                    _ => tys.extend(exprs.iter().map(|&expr| self.table.next_ty_var(expr.into()))),
                }

                for (expr, ty) in exprs.iter().zip(tys.iter_mut()) {
                    *ty =
                        self.infer_expr_coerce(*expr, &Expectation::has_type(*ty), ExprIsRead::Yes);
                }

                Ty::new_tup(self.interner(), &tys)
            }
            Expr::Array(Array::ElementList { elements }) => {
                self.infer_array_elements_expr(elements, expected, tgt_expr)
            }
            Expr::Array(Array::Repeat { initializer, repeat }) => {
                self.infer_array_repeat_expr(initializer, repeat, expected, tgt_expr)
            }
            Expr::Literal(lit) => literal_ty(
                self.interner(),
                lit,
                |_| {
                    let expected_ty = expected.to_option(&self.table);
                    tracing::debug!(?expected_ty);
                    let opt_ty = match expected_ty.as_ref().map(|it| it.kind()) {
                        Some(TyKind::Int(_) | TyKind::Uint(_)) => expected_ty,
                        Some(TyKind::Char) => Some(self.types.types.u8),
                        Some(TyKind::RawPtr(..) | TyKind::FnDef(..) | TyKind::FnPtr(..)) => {
                            Some(self.types.types.usize)
                        }
                        _ => None,
                    };
                    opt_ty.unwrap_or_else(|| self.table.next_int_var())
                },
                |_| {
                    let expected_ty = expected.to_option(&self.table);
                    let opt_ty = match expected_ty.as_ref().map(|it| it.kind()) {
                        Some(TyKind::Int(_) | TyKind::Uint(_)) => expected_ty,
                        Some(TyKind::Char) => Some(self.types.types.u8),
                        Some(TyKind::RawPtr(..) | TyKind::FnDef(..) | TyKind::FnPtr(..)) => {
                            Some(self.types.types.usize)
                        }
                        _ => None,
                    };
                    opt_ty.unwrap_or_else(|| self.table.next_int_var())
                },
                |_| {
                    let opt_ty = expected
                        .to_option(&self.table)
                        .filter(|ty| matches!(ty.kind(), TyKind::Float(_)));
                    opt_ty.unwrap_or_else(|| self.table.next_float_var())
                },
            ),
            Expr::Underscore => {
                // Underscore expression is an error, we render a specialized diagnostic
                // to let the user know what type is expected though.
                let expected = expected.to_option(&self.table).unwrap_or_else(|| self.err_ty());
                self.push_diagnostic(InferenceDiagnostic::TypedHole {
                    expr: tgt_expr,
                    expected: expected.store(),
                });
                expected
            }
            Expr::OffsetOf(offset) => {
                // MIR lowering needs the type whose fields these are.
                let container = self.make_ty(offset.container);
                self.result.type_of_type_placeholder.insert(offset.container, container.store());
                self.types.types.usize
            }
            Expr::InlineAsm(asm) => {
                let check_expr_asm_operand = |this: &mut Self, expr, is_input: bool| {
                    let ty = this.infer_expr_no_expect(expr, ExprIsRead::Yes);

                    // If this is an input value, we require its type to be fully resolved
                    // at this point. This allows us to provide helpful coercions which help
                    // pass the type candidate list in a later pass.
                    //
                    // We don't require output types to be resolved at this point, which
                    // allows them to be inferred based on how they are used later in the
                    // function.
                    if is_input {
                        let ty = this.structurally_resolve_type(expr.into(), ty);
                        match ty.kind() {
                            TyKind::FnDef(def, parameters) => {
                                let parameters = parameters.no_bound_vars().unwrap();
                                let fnptr_ty = Ty::new_fn_ptr(
                                    this.interner(),
                                    this.interner()
                                        .fn_sig(def)
                                        .instantiate(this.interner(), parameters)
                                        .skip_norm_wip(),
                                );
                                _ = this.coerce(
                                    expr,
                                    ty,
                                    fnptr_ty,
                                    AllowTwoPhase::No,
                                    ExprIsRead::Yes,
                                );
                            }
                            TyKind::Ref(_, base_ty, mutbl) => {
                                let ptr_ty = Ty::new_ptr(this.interner(), base_ty, mutbl);
                                _ = this.coerce(
                                    expr,
                                    ty,
                                    ptr_ty,
                                    AllowTwoPhase::No,
                                    ExprIsRead::Yes,
                                );
                            }
                            _ => {}
                        }
                    }
                };

                let mut diverge = asm.options.contains(AsmOptions::NORETURN);
                asm.operands.iter().enumerate().for_each(|(index, (_, operand))| match *operand {
                    AsmOperand::In { expr, .. } => check_expr_asm_operand(self, expr, true),
                    AsmOperand::Out { expr: Some(expr), .. } | AsmOperand::InOut { expr, .. } => {
                        check_expr_asm_operand(self, expr, false)
                    }
                    AsmOperand::Out { expr: None, .. } => (),
                    AsmOperand::SplitInOut { in_expr, out_expr, .. } => {
                        check_expr_asm_operand(self, in_expr, true);
                        if let Some(out_expr) = out_expr {
                            check_expr_asm_operand(self, out_expr, false);
                        }
                    }
                    AsmOperand::Label(expr) => {
                        let previous_diverges = self.diverges;
                        // The label blocks should have unit return value or diverge.
                        let ty = self.infer_expr_inner(
                            expr,
                            &Expectation::HasType(self.types.types.unit),
                            ExprIsRead::No,
                        );
                        if !ty.is_never() {
                            _ = self.demand_suptype(expr.into(), self.types.types.unit, ty);
                            diverge = false;
                        }
                        // We need this to avoid false unreachable warning when a label diverges.
                        self.diverges = previous_diverges;
                    }
                    AsmOperand::Const(expr) => {
                        self.infer_expr(expr, &Expectation::None, ExprIsRead::No);
                    }
                    // FIXME: `sym` should report for things that are not functions or statics.
                    AsmOperand::Sym(path) => {
                        // rustc type checks a `sym` as a path to a value, with the generic
                        // arguments it spells out.
                        if let Some((ValueNs::FunctionId(_), ty)) =
                            self.infer_path(self.store.path_at(path), tgt_expr.into())
                        {
                            self.result.asm_sym_tys.insert((tgt_expr, index as u32), ty.store());
                        }
                    }
                });
                if diverge || asm.kind == InlineAsmKind::NakedAsm {
                    self.types.types.never
                } else {
                    self.types.types.unit
                }
            }
            Expr::UnsafeBinderCast { kind, expr, ty } => {
                self.infer_unsafe_binder_cast(tgt_expr, kind, expr, ty, expected, is_read)
            }
            Expr::IncludeBytes => {
                let len = self.table.next_const_var(Span::Dummy);
                let arr = Ty::new_array_with_const_len(self.interner(), self.types.types.u8, len);
                Ty::new_ref(self.interner(), self.types.regions.statik, arr, Mutability::Not)
            }
        };
        // rustc's `ExprKind::Err` is of the error type, which unifies with what it meets: a
        // fresh variable in its place would leave its uses ambiguous (E0284).
        let ty = if matches!(expr, Expr::Missing) { ty } else { self.insert_type_vars_shallow(ty) };
        // Non-block expressions whose children diverged.
        match expr {
            Expr::Block { .. }
            | Expr::If { .. }
            | Expr::Let { .. }
            | Expr::Loop { .. }
            | Expr::Match { .. } => {}
            // A cast from `!` is sometimes needed to appease type checking.
            Expr::Cast { .. } => {}
            // rustc lowers a destructuring assignment to a block, and `.await` to a `match`.
            Expr::Assignment { target, .. } if !matches!(self.store.pat(target), Pat::Expr(_)) => {}
            Expr::Await { .. } => {}
            Expr::Call { .. } if self.is_try_block_output(tgt_expr) => {}
            Expr::Call { .. } | Expr::MethodCall { .. } => {
                self.warn_if_unreachable(UnreachableAt::Call(tgt_expr))
            }
            _ => self.warn_if_unreachable(UnreachableAt::Expr(tgt_expr)),
        }
        self.write_expr_ty(tgt_expr, ty);
        if self.table.resolve_vars_with_obligations(ty).is_never()
            && self.expr_guaranteed_to_constitute_read_for_never(tgt_expr, is_read)
        {
            // Any expression that produces a value of type `!` must have diverged
            self.diverges |= Diverges::always(tgt_expr);
        }
        self.diverges |= old_diverges;
        ty
    }

    fn infer_ref_expr(
        &mut self,
        rawness: Rawness,
        mutbl: Mutability,
        oprnd: ExprId,
        expected: &Expectation<'db>,
        expr: ExprId,
    ) -> Ty<'db> {
        let hint = expected.only_has_type(&mut self.table).map_or(Expectation::None, |ty| {
            match self.table.resolve_vars_with_obligations(ty).kind() {
                TyKind::Ref(_, ty, _) | TyKind::RawPtr(ty, _) => {
                    if self.is_syntactic_place_expr(oprnd) {
                        // Places may legitimately have unsized types.
                        // For example, dereferences of a wide pointer and
                        // the last field of a struct can be unsized.
                        Expectation::has_type(ty)
                    } else {
                        Expectation::rvalue_hint(self, ty)
                    }
                }
                _ => Expectation::None,
            }
        });
        let ty = self.infer_expr_inner(oprnd, &hint, ExprIsRead::No);

        match rawness {
            Rawness::RawPtr => Ty::new_ptr(self.interner(), ty, mutbl),
            Rawness::Ref => {
                // Note: at this point, we cannot say what the best lifetime
                // is to use for resulting pointer. We want to use the
                // shortest lifetime possible so as to avoid spurious borrowck
                // errors. Moreover, the longest lifetime will depend on the
                // precise details of the value whose address is being taken
                // (and how long it is valid), which we don't know yet until
                // type inference is complete.
                //
                // Therefore, here we simply generate a region variable. The
                // region inferencer will then select a suitable value.
                // Finally, borrowck will infer the value of the region again,
                // this time with enough precision to check that the value
                // whose address was taken can actually be made to live as long
                // as it needs to live.
                let region = self.table.next_region_var(expr.into());
                Ty::new_ref(self.interner(), region, ty, mutbl)
            }
        }
    }

    fn infer_await_expr(&mut self, expr: ExprId, awaitee: ExprId) -> Ty<'db> {
        let awaitee_ty = self.infer_expr_no_expect(awaitee, ExprIsRead::Yes);
        let (Some(into_future), Some(into_future_output)) =
            (self.lang_items.IntoFuture, self.lang_items.IntoFutureOutput)
        else {
            return self.types.types.error;
        };
        self.table.register_bound(awaitee_ty, into_future, ObligationCause::new(expr));
        self.table.try_structurally_resolve_type(
            expr.into(),
            Ty::new_projection(
                self.interner(),
                IsRigid::No,
                into_future_output.into(),
                [awaitee_ty],
            ),
        )
    }

    fn infer_record_expr(
        &mut self,
        expr: ExprId,
        expected: &Expectation<'db>,
        path: Path<'_>,
        fields: &[RecordLitField],
        base_expr: RecordSpread,
    ) -> Ty<'db> {
        // Find the relevant variant
        let (adt_ty, Some(variant)) = self.resolve_variant(expr.into(), path, false) else {
            if let Some(segment) = self.first_unresolved_segment(path, expr.into()) {
                self.push_diagnostic(InferenceDiagnostic::UnresolvedPath { id: expr.into(), segment });
            }
            for field in fields {
                self.infer_expr_no_expect(field.expr, ExprIsRead::Yes);
            }

            return self.types.types.error;
        };
        self.write_variant_resolution(expr.into(), variant);

        // The ADT's where clauses are what a closure in a field gets its signature from.
        if let TyKind::Adt(adt, args) = adt_ty.kind() {
            self.add_required_obligations_for_value_path(expr.into(), adt.def_id().into(), args);
        }

        // Prohibit struct expressions when non-exhaustive flag is set.
        if self.has_applicable_non_exhaustive(variant.into()) {
            self.push_diagnostic(InferenceDiagnostic::NonExhaustiveRecordExpr { expr });
        }

        self.check_record_expr_fields(adt_ty, expected, expr, variant, fields, base_expr);

        self.require_type_is_sized(adt_ty, expr.into(), ObligationCauseCode::Misc);
        adt_ty
    }

    fn check_record_expr_fields(
        &mut self,
        adt_ty: Ty<'db>,
        expected: &Expectation<'db>,
        expr: ExprId,
        variant: VariantId,
        hir_fields: &[RecordLitField],
        base_expr: RecordSpread,
    ) {
        let interner = self.interner();

        let adt_ty = self.table.try_structurally_resolve_type(expr.into(), adt_ty);
        let adt_ty_hint = expected.only_has_type(&mut self.table).and_then(|expected| {
            self.infcx()
                .fudge_inference_if_ok(|| {
                    let mut ocx = ObligationCtxt::new(self.infcx());
                    ocx.sup(&ObligationCause::new(expr), self.table.param_env, expected, adt_ty)?;
                    if !ocx.try_evaluate_obligations().is_empty() {
                        return Err(TypeError::Mismatch);
                    }
                    Ok(self.resolve_vars_if_possible(adt_ty))
                })
                .ok()
        });
        if let Some(adt_ty_hint) = adt_ty_hint {
            // re-link the variables that the fudging above can create.
            _ = self.demand_eqtype(expr.into(), adt_ty_hint, adt_ty);
        }

        let TyKind::Adt(adt, args) = adt_ty.kind() else {
            // An error type was reported where it was made; anything else is rustc's `span_bug!`.
            assert!(adt_ty.is_ty_error(), "non-ADT passed to check_struct_expr_fields: {adt_ty:?}");
            return;
        };
        let adt_id = adt.def_id();

        let variant_fields = variant.fields(self.db);
        let variant_field_tys = self.db.field_types(variant);
        let variant_field_vis = VariantFields::field_visibilities(self.db, variant);
        let mut remaining_fields = hashbrown::HashMap::with_capacity_and_hasher_in(
            variant_fields.len(),
            FixedState,
            self.frame,
        );
        remaining_fields.extend(variant_fields.fields().iter().map(|(i, field)| (&field.name, i)));

        let mut seen_fields =
            hashbrown::HashMap::with_capacity_and_hasher_in(hir_fields.len(), FixedState, self.frame);

        // Type-check each field.
        for field in hir_fields {
            let name = &field.name;
            let field_type = if let Some(i) = remaining_fields.remove(name) {
                seen_fields.insert(name, i);

                if !self.resolver.is_visible(self.db, variant_field_vis[i]) {
                    self.push_diagnostic(InferenceDiagnostic::NoSuchField {
                        field: field.expr.into(),
                        private: Some(i),
                        variant,
                    });
                }

                variant_field_tys[i].ty().instantiate(interner, args).skip_norm_wip()
            } else {
                if let Some(field_idx) = seen_fields.get(&name) {
                    self.push_diagnostic(InferenceDiagnostic::DuplicateField {
                        field: field.expr.into(),
                        variant,
                    });
                    variant_field_tys[*field_idx].ty().instantiate(interner, args).skip_norm_wip()
                } else {
                    self.push_diagnostic(InferenceDiagnostic::NoSuchField {
                        field: field.expr.into(),
                        private: None,
                        variant,
                    });
                    self.types.types.error
                }
            };

            // Check that the expected field type is WF. Otherwise, we emit no use-site error
            // in the case of coercions for non-WF fields, which leads to incorrect error
            // tainting. See issue #126272.
            self.table.register_wf_obligation(field_type.into(), ObligationCause::new(field.expr));

            // Make sure to give a type to the field even if there's
            // an error, so we can continue type-checking.
            self.infer_expr_coerce(field.expr, &Expectation::has_type(field_type), ExprIsRead::Yes);
        }

        // Make sure the programmer specified correct number of fields.
        if matches!(adt_id, AdtId::UnionId(_)) && hir_fields.len() != 1 {
            self.push_diagnostic(InferenceDiagnostic::UnionExprMustHaveExactlyOneField { expr });
        }

        match base_expr {
            RecordSpread::FieldDefaults => {
                let mut missing_mandatory_fields = Vec::new_in(self.frame);
                let mut missing_optional_fields = Vec::new_in(self.frame);
                for (field_idx, field) in variant_fields.fields().iter() {
                    if remaining_fields.remove(&field.name).is_some() {
                        if field.default_value.is_none() {
                            missing_mandatory_fields.push(field_idx);
                        } else {
                            missing_optional_fields.push(field_idx);
                        }
                    }
                }
                if !missing_mandatory_fields.is_empty() {
                    // FIXME: Emit an error: missing fields.
                }
            }
            RecordSpread::Expr(base_expr) => {
                // FIXME: We are currently creating two branches here in order to maintain
                // consistency. But they should be merged as much as possible.
                if self.features.type_changing_struct_update {
                    if matches!(adt_id, AdtId::StructId(_)) {
                        // Make some fresh generic parameters for our ADT type.
                        let fresh_args = self.table.fresh_args_for_item(expr.into(), adt_id.into());
                        // We do subtyping on the FRU fields first, so we can
                        // learn exactly what types we expect the base expr
                        // needs constrained to be compatible with the struct
                        // type we expect from the expectation value.
                        for (field_idx, field) in variant_fields.fields().iter() {
                            let fru_ty = variant_field_tys[field_idx]
                                .ty()
                                .instantiate(interner, fresh_args)
                                .skip_norm_wip();
                            if remaining_fields.remove(&field.name).is_some() {
                                let target_ty = variant_field_tys[field_idx]
                                    .ty()
                                    .instantiate(interner, args)
                                    .skip_norm_wip();
                                let cause = ObligationCause::new(expr);
                                match self.table.at(&cause).sup(target_ty, fru_ty) {
                                    Ok(InferOk { obligations, value: () }) => {
                                        self.table.register_predicates(obligations)
                                    }
                                    Err(_) => {
                                        unreachable!(
                                            "subtyping remaining fields of type changing FRU \
                                                failed: {target_ty:?} != {fru_ty:?}: {:?}",
                                            field.name,
                                        );
                                    }
                                }
                            }
                        }
                        // The use of fresh args that we have subtyped against
                        // our base ADT type's fields allows us to guide inference
                        // along so that, e.g.
                        // ```
                        // MyStruct<'a, F1, F2, const C: usize> {
                        //     f: F1,
                        //     // Other fields that reference `'a`, `F2`, and `C`
                        // }
                        //
                        // let x = MyStruct {
                        //    f: 1usize,
                        //    ..other_struct
                        // };
                        // ```
                        // will have the `other_struct` expression constrained to
                        // `MyStruct<'a, _, F2, C>`, as opposed to just `_`...
                        // This is important to allow coercions to happen in
                        // `other_struct` itself. See `coerce-in-base-expr.rs`.
                        let fresh_base_ty = Ty::new_adt(self.interner(), adt_id, fresh_args);
                        self.infer_expr_suptype_coerce_never(
                            base_expr,
                            &Expectation::has_type(self.resolve_vars_if_possible(fresh_base_ty)),
                            ExprIsRead::Yes,
                        );
                    } else {
                        // Check the base_expr, regardless of a bad expected adt_ty, so we can get
                        // type errors on that expression, too.
                        self.infer_expr_no_expect(base_expr, ExprIsRead::Yes);
                        self.push_diagnostic(
                            InferenceDiagnostic::FunctionalRecordUpdateOnNonStruct { base_expr },
                        );
                    }
                } else {
                    self.infer_expr_suptype_coerce_never(
                        base_expr,
                        &Expectation::has_type(adt_ty),
                        ExprIsRead::Yes,
                    );
                    if !matches!(adt_id, AdtId::StructId(_)) {
                        self.push_diagnostic(
                            InferenceDiagnostic::FunctionalRecordUpdateOnNonStruct { base_expr },
                        );
                    }
                }
            }
            RecordSpread::None => {
                if !matches!(adt_id, AdtId::UnionId(_))
                    && !remaining_fields.is_empty()
                    //~ non_exhaustive already reported, which will only happen for extern modules
                    && !self.has_applicable_non_exhaustive(adt_id.into())
                {
                    debug!(?remaining_fields);

                    // FIXME: Emit an error: missing fields.
                }
            }
        }
    }

    fn demand_scrutinee_type(
        &mut self,
        scrut: ExprId,
        contains_ref_bindings: bool,
        no_arms: bool,
        scrutinee_is_read: ExprIsRead,
    ) -> Ty<'db> {
        // Not entirely obvious: if matches may create ref bindings, we want to
        // use the *precise* type of the scrutinee, *not* some supertype, as
        // the "scrutinee type" (issue #23116).
        //
        // arielb1 [writes here in this comment thread][c] that there
        // is certainly *some* potential danger, e.g., for an example
        // like:
        //
        // [c]: https://github.com/rust-lang/rust/pull/43399#discussion_r130223956
        //
        // ```
        // let Foo(x) = f()[0];
        // ```
        //
        // Then if the pattern matches by reference, we want to match
        // `f()[0]` as a lexpr, so we can't allow it to be
        // coerced. But if the pattern matches by value, `f()[0]` is
        // still syntactically a lexpr, but we *do* want to allow
        // coercions.
        //
        // However, *likely* we are ok with allowing coercions to
        // happen if there are no explicit ref mut patterns - all
        // implicit ref mut patterns must occur behind a reference, so
        // they will have the "correct" variance and lifetime.
        //
        // This does mean that the following pattern would be legal:
        //
        // ```
        // struct Foo(Bar);
        // struct Bar(u32);
        // impl Deref for Foo {
        //     type Target = Bar;
        //     fn deref(&self) -> &Bar { &self.0 }
        // }
        // impl DerefMut for Foo {
        //     fn deref_mut(&mut self) -> &mut Bar { &mut self.0 }
        // }
        // fn foo(x: &mut Foo) {
        //     {
        //         let Bar(z): &mut Bar = x;
        //         *z = 42;
        //     }
        //     assert_eq!(foo.0.0, 42);
        // }
        // ```
        //
        // FIXME(tschottdorf): don't call contains_explicit_ref_binding, which
        // is problematic as the HIR is being scraped, but ref bindings may be
        // implicit after #42640. We need to make sure that pat_adjustments
        // (once introduced) is populated by the time we get here.
        //
        // See #44848.
        if contains_ref_bindings || no_arms {
            self.infer_expr_no_expect(scrut, scrutinee_is_read)
        } else {
            // ...but otherwise we want to use any supertype of the
            // scrutinee. This is sort of a workaround, see note (*) in
            // `check_pat` for some details.
            let scrut_ty = self.table.next_ty_var(scrut.into());
            self.infer_expr_coerce_never(scrut, &Expectation::HasType(scrut_ty), scrutinee_is_read);
            scrut_ty
        }
    }

    fn infer_expr_path(&mut self, path: Path<'_>, id: ExprOrPatIdPacked, scope_id: ExprId) -> Ty<'db> {
        let g = self.resolver.update_to_inner_scope(self.db, self.store_owner, scope_id);
        let ty = match self.infer_path(path, id) {
            Some((_, ty)) => ty,
            // A `Self` that resolves but has no constructor is reported already.
            None if self.diagnostics.any(|it| {
                matches!(it, InferenceDiagnostic::SelfCtorNotTupleOrUnit { expr, .. }
                    if ExprOrPatIdPacked::from(*expr) == id)
            }) =>
            {
                self.err_ty()
            }
            None => {
                if path.mod_path().is_some_and(|mod_path| mod_path.is_ident() || mod_path.is_self())
                {
                    self.push_diagnostic(InferenceDiagnostic::UnresolvedIdent { id });
                } else if let Some(segment) = self.first_unresolved_segment(path, id) {
                    self.push_diagnostic(InferenceDiagnostic::UnresolvedPath { id, segment });
                }
                self.err_ty()
            }
        };
        self.resolver.reset_to_guard(g);
        ty
    }

    /// The index of the first segment of `path` that names nothing, as rustc's late resolver
    /// finds it (`report_path_resolution_error`): a prefix that is no module or type, or a last
    /// segment that names nothing in its module in either namespace. `None` when a prefix is
    /// a type (an unknown associated item is E0599's, reported where it is looked up), the
    /// path is type-anchored or a lang item's, or its last segment names something of the
    /// other namespace (rustc's E0423/E0574, not this).
    pub(super) fn first_unresolved_segment(
        &self,
        path: Path<'_>,
        id: ExprOrPatIdPacked,
    ) -> Option<u32> {
        if path.type_anchor().is_some() {
            return None;
        }
        let mod_path = path.mod_path()?;
        let segments = mod_path.segments();
        let last = segments.len().checked_sub(1)?;
        for len in 1..=last {
            // alloc: error path: a path that failed to resolve.
            let prefix = ModPath::from_segments(mod_path.kind, segments[..len].iter().cloned());
            match self.resolver.resolve_path_in_type_ns(self.db, Path::BarePath(&prefix)) {
                None => return Some(len as u32 - 1),
                Some((TypeNs::ModuleId(_), None, _)) => {}
                Some(_) => return None,
            }
        }
        let hygiene = self.store.expr_or_pat_path_hygiene(id.unpack());
        let names_something = self.resolver.resolve_path_in_value_ns(self.db, path, hygiene).is_some()
            || self.resolver.resolve_path_in_type_ns(self.db, path).is_some();
        (!names_something).then_some(last as u32)
    }

    fn infer_unop_expr(
        &mut self,
        unop: UnaryOp,
        oprnd: ExprId,
        expected: &Expectation<'db>,
        expr: ExprId,
    ) -> Ty<'db> {
        let expected_inner = match unop {
            UnaryOp::Not | UnaryOp::Neg => expected,
            UnaryOp::Deref => &Expectation::None,
        };
        let mut oprnd_t = self.infer_expr_inner(oprnd, expected_inner, ExprIsRead::Yes);

        oprnd_t = self.structurally_resolve_type(oprnd.into(), oprnd_t);
        match unop {
            UnaryOp::Deref => {
                if let Some(ty) = self.lookup_derefing(expr, oprnd, oprnd_t) {
                    oprnd_t = ty;
                } else {
                    let at_error = self.ty_at_error(oprnd_t);
                    self.push_diagnostic(InferenceDiagnostic::CannotBeDereferenced {
                        expr,
                        found: oprnd_t.store(),
                        at_error,
                    });
                    oprnd_t = self.types.types.error;
                }
            }
            UnaryOp::Not => {
                let result = self.infer_user_unop(expr, oprnd_t, unop);
                // If it's builtin, we can reuse the type, this helps inference.
                if !(oprnd_t.is_integral() || oprnd_t.kind() == TyKind::Bool) {
                    oprnd_t = result;
                }
            }
            UnaryOp::Neg => {
                let result = self.infer_user_unop(expr, oprnd_t, unop);
                // If it's builtin, we can reuse the type, this helps inference.
                if !oprnd_t.is_numeric() {
                    oprnd_t = result;
                }
            }
        }
        oprnd_t
    }

    fn infer_array_repeat_expr(
        &mut self,
        element: ExprId,
        count: ExprId,
        expected: &Expectation<'db>,
        expr: ExprId,
    ) -> Ty<'db> {
        let interner = self.interner();
        let count_ct = self.create_anon_const(count, self.types.types.usize);
        let count = self.table.try_structurally_resolve_const(count.into(), count_ct);

        let uty = match expected {
            Expectation::HasType(uty) => {
                self.table.resolve_vars_with_obligations(*uty).builtin_index()
            }
            _ => None,
        };

        let t = match uty {
            Some(uty) => {
                self.infer_expr_coerce(element, &Expectation::has_type(uty), ExprIsRead::Yes);
                uty
            }
            None => {
                let ty = self.table.next_ty_var(element.into());
                self.infer_expr_suptype_coerce_never(
                    element,
                    &Expectation::has_type(ty),
                    ExprIsRead::Yes,
                );
                ty
            }
        };

        // We defer checking whether the element type is `Copy` as it is possible to have
        // an inference variable as a repeat count and it seems unlikely that `Copy` would
        // have inference side effects required for type checking to succeed.
        // FIXME: Do it here like rustc.
        // self.deferred_repeat_expr_checks.borrow_mut().push((element, element_ty, count));

        let ty = Ty::new_array_with_const_len(interner, t, count);
        self.table.register_wf_obligation(ty.into(), ObligationCause::new(expr));
        ty
    }

    fn infer_array_elements_expr(
        &mut self,
        args: &[ExprId],
        expected: &Expectation<'db>,
        expr: ExprId,
    ) -> Ty<'db> {
        let element_ty = if !args.is_empty() {
            let coerce_to = expected
                .to_option(&self.table)
                .and_then(|uty| {
                    self.table
                        .resolve_vars_with_obligations(uty)
                        .builtin_index()
                        // Avoid using the original type variable as the coerce_to type, as it may resolve
                        // during the first coercion instead of being the LUB type.
                        .filter(|t| !self.table.resolve_vars_with_obligations(*t).is_ty_var())
                })
                .unwrap_or_else(|| self.table.next_ty_var(expr.into()));
            let mut coerce = CoerceMany::with_coercion_sites(coerce_to, args);

            for &e in args {
                // FIXME: the element expectation should use
                // `try_structurally_resolve_and_adjust_for_branches` just like in `if` and `match`.
                // While that fixes nested coercion, it will break [some
                // code like this](https://github.com/rust-lang/rust/pull/140283#issuecomment-2958776528).
                // If we find a way to support recursive tuple coercion, this break can be avoided.
                let e_ty =
                    self.infer_expr_inner(e, &Expectation::has_type(coerce_to), ExprIsRead::Yes);
                let cause = ObligationCause::new(e);
                coerce.coerce(self, &cause, e, e_ty, ExprIsRead::Yes);
            }
            coerce.complete(self)
        } else {
            self.table.next_ty_var(expr.into())
        };
        let array_len = args.len() as u64;
        Ty::new_array(self.interner(), element_ty, array_len)
    }

    pub(super) fn infer_return(&mut self, expr: ExprId) {
        let ret_ty = self
            .return_coercion
            .as_mut()
            .expect("infer_return called outside function body")
            .expected_ty();
        let return_expr_ty =
            self.infer_expr_inner(expr, &Expectation::HasType(ret_ty), ExprIsRead::Yes);
        let mut coerce_many = self.return_coercion.take().unwrap();
        coerce_many.coerce(
            self,
            &ObligationCause::with_code(expr, ObligationCauseCode::ReturnValue),
            expr,
            return_expr_ty,
            ExprIsRead::Yes,
        );
        self.return_coercion = Some(coerce_many);
    }

    fn infer_expr_return(&mut self, ret: ExprId, expr: Option<ExprId>) -> Ty<'db> {
        match self.return_coercion {
            Some(_) => {
                if let Some(expr) = expr {
                    self.infer_return(expr);
                } else {
                    let mut coerce = self.return_coercion.take().unwrap();
                    coerce.coerce_forced_unit(
                        self,
                        ret,
                        &ObligationCause::with_code(ret, ObligationCauseCode::ReturnNoExpression),
                        true,
                        ExprIsRead::Yes,
                    );
                    self.return_coercion = Some(coerce);
                }
            }
            None => {
                self.push_diagnostic(InferenceDiagnostic::ReturnOutsideFunction {
                    expr: ret,
                    kind: ReturnKind::ReturnExpr,
                });
                if let Some(expr) = expr {
                    self.infer_expr_no_expect(expr, ExprIsRead::Yes);
                }
                // rustc's lowering makes it `ExprKind::Err`: nothing more is reported of it.
                return self.err_ty();
            }
        }
        self.types.types.never
    }

    fn infer_expr_become(&mut self, tgt_expr: ExprId, expr: ExprId) -> Ty<'db> {
        match &self.return_coercion {
            Some(return_coercion) => {
                let ret_ty = return_coercion.expected_ty();

                let call_expr_ty =
                    self.infer_expr_inner(expr, &Expectation::HasType(ret_ty), ExprIsRead::Yes);

                // NB: this should *not* coerce.
                //     tail calls don't support any coercions except lifetimes ones (like `&'static u8 -> &'a u8`).
                _ = self.demand_eqtype(expr.into(), call_expr_ty, ret_ty);
            }
            None => {
                self.push_diagnostic(InferenceDiagnostic::ReturnOutsideFunction {
                    expr: tgt_expr,
                    kind: ReturnKind::BecomeExpr,
                });
                self.infer_expr_no_expect(expr, ExprIsRead::Yes);
                return self.err_ty();
            }
        }

        self.types.types.never
    }

    fn infer_block(
        &mut self,
        expr: ExprId,
        statements: Statements<'_>,
        tail: Option<ExprId>,
        label: Option<LabelId>,
        expected: &Expectation<'db>,
    ) -> Ty<'db> {
        let prev_diverges = self.diverges;
        let coerce_ty = expected.coercion_target_type(&mut self.table, expr.into());
        let g = self.resolver.update_to_inner_scope(self.db, self.store_owner, expr);

        let (ctxt, tail_expr_ty) =
            self.with_breakable_ctx(BreakableKind::Block, Some(coerce_ty), label, |this| {
                for (index, stmt) in statements.iter().enumerate() {
                    if matches!(stmt, Statement::Item(_)) {
                        continue;
                    }
                    // rustc's `check_stmt`: the statement, and the outer divergence hidden.
                    this.warn_if_unreachable(UnreachableAt::Stmt { block: expr, index: index as u32 });
                    let old_diverges = mem::replace(&mut this.diverges, Diverges::Maybe);
                    match stmt {
                        Statement::Let { pat, type_ref, initializer, else_branch, .. } => {
                            let decl_ty = type_ref
                                .as_ref()
                                .map(|&tr| this.make_ty(tr))
                                .unwrap_or_else(|| this.table.next_ty_var(pat.into()));

                            this.infer_let(
                                decl_ty,
                                initializer,
                                pat,
                                PatOrigin::LetStmt { has_else: else_branch.is_some() },
                            );
                            if let Some(expr) = else_branch {
                                let previous_diverges = this.diverges;
                                this.infer_expr_coerce(
                                    expr,
                                    &Expectation::HasType(this.types.types.never),
                                    ExprIsRead::Yes,
                                );
                                this.diverges = previous_diverges;
                            }
                        }
                        Statement::Expr { expr, has_semi } => {
                            if has_semi {
                                this.infer_expr(expr, &Expectation::none(), ExprIsRead::Yes);
                            } else {
                                this.infer_expr_coerce(
                                    expr,
                                    &Expectation::HasType(this.types.types.unit),
                                    ExprIsRead::Yes,
                                );
                            }
                        }
                        Statement::Item(_) => (),
                    }
                    this.diverges |= old_diverges;
                }

                // check the tail expression **without** holding the
                // `enclosing_breakables` lock below.
                tail.map(|expr| (expr, this.infer_expr_inner(expr, expected, ExprIsRead::Yes)))
            });

        let mut coerce = ctxt.coerce.unwrap();
        if let Some((tail_expr, tail_expr_ty)) = tail_expr_ty {
            let cause =
                ObligationCause::with_code(tail_expr, ObligationCauseCode::BlockTailExpression);
            coerce.coerce_inner(
                self,
                &cause,
                tail_expr,
                tail_expr_ty,
                false,
                false,
                ExprIsRead::Yes,
            );
        } else {
            // Subtle: if there is no explicit tail expression,
            // that is typically equivalent to a tail expression
            // of `()` -- except if the block diverges. In that
            // case, there is no value supplied from the tail
            // expression (assuming there are no other breaks,
            // this implies that the type of the block will be
            // `!`).
            //
            // #41425 -- label the implicit `()` as being the
            // "found type" here, rather than the "expected type".
            if !self.diverges.is_always() {
                coerce.coerce_forced_unit(
                    self,
                    expr,
                    &ObligationCause::new(expr),
                    false,
                    ExprIsRead::Yes,
                );
            }
        }

        if ctxt.may_break {
            // If we can break from the block, then the block's exit is always reachable
            // (... as long as the entry is reachable) - regardless of the tail of the block.
            self.diverges = prev_diverges;
        }

        self.resolver.reset_to_guard(g);

        coerce.complete(self)
    }

    fn infer_let(
        &mut self,
        decl_ty: Ty<'db>,
        initializer: Option<ExprId>,
        pat: PatId,
        pat_origin: PatOrigin,
    ) {
        if let Some(expr) = initializer {
            // If we have a subpattern that performs a read, we want to consider this
            // to diverge for compatibility to support something like `let x: () = *never_ptr;`.
            let target_is_read = if self.pat_guaranteed_to_constitute_read_for_never(pat) {
                ExprIsRead::Yes
            } else {
                ExprIsRead::No
            };
            if self.contains_explicit_ref_binding(pat) {
                self.infer_expr(expr, &Expectation::has_type(decl_ty), target_is_read)
            } else {
                self.infer_expr_coerce(expr, &Expectation::has_type(decl_ty), target_is_read)
            };
        };

        self.infer_top_pat(pat, decl_ty, pat_origin);
    }

    fn lookup_field(
        &mut self,
        field_expr: ExprId,
        receiver_ty: Ty<'db>,
        name: &Name,
    ) -> Option<(Ty<'db>, Either<FieldId, TupleFieldId>, Vec<Adjustment, &'f Frame>, bool)> {
        let interner = self.interner();
        let frame = self.frame;
        let mut autoderef = self.table.autoderef_with_tracking(receiver_ty, field_expr.into());
        let mut private_field = None;
        let res = autoderef.by_ref().find_map(|(derefed_ty, _)| {
            let (field_id, parameters) = match derefed_ty.kind() {
                TyKind::Tuple(substs) => {
                    return name.as_tuple_index().and_then(|idx| {
                        substs.as_slice().get(idx).copied().map(|ty| {
                            (
                                Either::Right(TupleFieldId {
                                    tuple: TupleId(self.tuple_field_accesses_rev.insert(substs)),
                                    index: idx as u32,
                                }),
                                ty,
                            )
                        })
                    });
                }
                TyKind::Adt(adt, parameters) => match adt.def_id() {
                    hir_def::AdtId::StructId(s) => {
                        let local_id = s.fields(self.db).field(name)?;
                        let field = FieldId { parent: s.into(), local_id };
                        (field, parameters)
                    }
                    hir_def::AdtId::UnionId(u) => {
                        let local_id = u.fields(self.db).field(name)?;
                        let field = FieldId { parent: u.into(), local_id };
                        (field, parameters)
                    }
                    hir_def::AdtId::EnumId(_) => return None,
                },
                _ => return None,
            };
            let is_visible = VariantFields::field_visibilities(self.db, field_id.parent)
                [field_id.local_id]
                .is_visible_from(self.db, self.resolver.module());
            if !is_visible {
                if private_field.is_none() {
                    private_field = Some((field_id, parameters));
                }
                return None;
            }
            let ty = self.db.field_types(field_id.parent)[field_id.local_id]
                .ty()
                .instantiate(interner, parameters)
                .skip_norm_wip();
            Some((Either::Left(field_id), ty))
        });

        Some(match res {
            Some((field_id, ty)) => {
                let adjustments =
                    self.table.register_infer_ok(autoderef.adjust_steps_as_infer_ok(frame));
                let ty = self.process_remote_user_written_ty(ty);

                (ty, field_id, adjustments, true)
            }
            None => {
                let (field_id, subst) = private_field?;
                let adjustments =
                    self.table.register_infer_ok(autoderef.adjust_steps_as_infer_ok(frame));
                let ty = self.db.field_types(field_id.parent)[field_id.local_id]
                    .ty()
                    .instantiate(self.interner(), subst)
                    .skip_norm_wip();
                let ty = self.process_remote_user_written_ty(ty);

                (ty, Either::Left(field_id), adjustments, false)
            }
        })
    }

    /// rustc's `check_expr_unsafe_binder_cast`. `wrap_binder!` checks its operand against the
    /// binder's contents and has the binder's type; `unwrap_binder!` has the contents of its
    /// operand's binder. Each binder is instantiated with fresh region variables.
    fn infer_unsafe_binder_cast(
        &mut self,
        tgt_expr: ExprId,
        kind: UnsafeBinderCastKind,
        operand: ExprId,
        type_ref: Option<TypeRefId>,
        expected: &Expectation<'db>,
        is_read: ExprIsRead,
    ) -> Ty<'db> {
        let ascribed = type_ref.map(|type_ref| self.make_ty(type_ref));
        match kind {
            UnsafeBinderCastKind::Wrap => {
                let binder_ty = match (ascribed, expected.only_has_type(&mut self.table)) {
                    (Some(ascribed), Some(expected)) => {
                        _ = self.demand_eqtype(operand.into(), expected, ascribed);
                        expected
                    }
                    (Some(ty), None) | (None, Some(ty)) => ty,
                    (None, None) => self.table.next_ty_var(operand.into()),
                };
                let binder_ty = self.structurally_resolve_type(operand.into(), binder_ty);
                let contents = self.unsafe_binder_contents(tgt_expr, kind, operand, binder_ty);
                self.infer_expr_suptype_coerce_never(
                    operand,
                    &Expectation::HasType(contents),
                    is_read,
                );
                binder_ty
            }
            UnsafeBinderCastKind::Unwrap => {
                let hint = ascribed.unwrap_or_else(|| self.table.next_ty_var(operand.into()));
                let binder_ty = self.infer_expr_suptype_coerce_never(
                    operand,
                    &Expectation::HasType(hint),
                    is_read,
                );
                let binder_ty = self.structurally_resolve_type(operand.into(), binder_ty);
                self.unsafe_binder_contents(tgt_expr, kind, operand, binder_ty)
            }
        }
    }

    /// What `binder_ty`, the binder of the cast `tgt_expr`, holds; an error, reported unless
    /// `binder_ty` is one, when it is no unsafe binder.
    fn unsafe_binder_contents(
        &mut self,
        tgt_expr: ExprId,
        kind: UnsafeBinderCastKind,
        operand: ExprId,
        binder_ty: Ty<'db>,
    ) -> Ty<'db> {
        match binder_ty.kind() {
            TyKind::UnsafeBinder(inner) => self.infcx().instantiate_binder_with_fresh_vars(
                operand.into(),
                BoundRegionConversionTime::HigherRankedType,
                inner.into(),
            ),
            TyKind::Error(_) => binder_ty,
            _ => {
                self.push_diagnostic(InferenceDiagnostic::UnsafeBinderCastOfNonBinder {
                    expr: tgt_expr,
                    kind,
                    found: binder_ty.store(),
                });
                self.err_ty()
            }
        }
    }

    fn infer_field_access(
        &mut self,
        tgt_expr: ExprId,
        receiver: ExprId,
        name: &Name,
        expected: &Expectation<'db>,
    ) -> Ty<'db> {
        // Field projections don't constitute reads.
        let receiver_ty = self.infer_expr_inner(receiver, &Expectation::none(), ExprIsRead::No);
        let receiver_ty = self.structurally_resolve_type(receiver.into(), receiver_ty);

        if name.is_missing() {
            // Bail out early, don't even try to look up field. Also, we don't issue an unresolved
            // field diagnostic because this is a syntax error rather than a semantic error.
            return self.err_ty();
        }

        match self.lookup_field(tgt_expr, receiver_ty, name) {
            Some((ty, field_id, adjustments, is_public)) => {
                self.write_expr_adj(receiver, &adjustments);
                self.result.field_resolutions.insert(tgt_expr, field_id);
                if !is_public && let Either::Left(field) = field_id {
                    // FIXME: Merge this diagnostic into UnresolvedField?
                    self.push_diagnostic(InferenceDiagnostic::PrivateField {
                        expr: tgt_expr,
                        field,
                    });
                }
                ty
            }
            None => {
                // No field: probe for a method of the name, as rustc's
                // `method_exists_for_diagnostic`, which also lets IDE features work while
                // people are typing.
                let resolved =
                    self.lookup_method_for_diagnostic(receiver_ty, *name, receiver, tgt_expr);
                self.push_diagnostic(InferenceDiagnostic::UnresolvedField {
                    expr: tgt_expr,
                    receiver: receiver_ty.store(),
                    name: *name,
                    method_with_same_name_exists: resolved.is_ok(),
                });
                match resolved {
                    Ok((func, _is_visible)) => {
                        self.check_method_call(tgt_expr, &[], func.sig, expected)
                    }
                    Err(_) => self.err_ty(),
                }
            }
        }
    }

    fn instantiate_erroneous_method(&mut self, def_id: FunctionId) -> MethodCallee<'db> {
        // FIXME: Using fresh infer vars for the method args isn't optimal,
        // we can do better by going thorough the full probe/confirm machinery.
        let args = self.table.fresh_args_for_item(Span::Dummy, def_id.into());
        let sig = self
            .db
            .callable_item_signature(def_id.into())
            .instantiate(self.interner(), args)
            .skip_norm_wip();
        let sig = self.infcx().instantiate_binder_with_fresh_vars(
            Span::Dummy,
            BoundRegionConversionTime::FnCall,
            sig,
        );
        MethodCallee { def_id, args, sig }
    }

    fn infer_method_call_as_call(
        &mut self,
        tgt_expr: ExprId,
        args: &[ExprId],
        callee_ty: Ty<'db>,
        param_tys: &[Ty<'db>],
        ret_ty: Ty<'db>,
        indices_to_skip: &[u32],
        is_varargs: bool,
        expected: &Expectation<'db>,
    ) -> Ty<'db> {
        if let TyKind::FnDef(def_id, args) = callee_ty.kind() {
            let args = args.no_bound_vars().unwrap();
            let def_id = match def_id.0 {
                CallableDefId::FunctionId(it) => it.into(),
                CallableDefId::StructId(it) => it.into(),
                CallableDefId::EnumVariantId(it) => it.loc(self.db).parent.into(),
            };
            self.add_required_obligations_for_value_path(tgt_expr.into(), def_id, args);
        }

        self.check_call_arguments(
            tgt_expr,
            param_tys,
            ret_ty,
            expected,
            args,
            indices_to_skip,
            is_varargs,
            TupleArgumentsFlag::DontTupleArguments,
        );
        ret_ty
    }

    fn infer_method_call(
        &mut self,
        tgt_expr: ExprId,
        receiver: ExprId,
        args: &[ExprId],
        method_name: &Name,
        generic_args: Option<HirGenericArgs<'_>>,
        expected: &Expectation<'db>,
    ) -> Ty<'db> {
        let receiver_ty = self.infer_expr_inner(receiver, &Expectation::none(), ExprIsRead::Yes);
        let receiver_ty = self.table.try_structurally_resolve_type(receiver.into(), receiver_ty);

        let resolved = self.lookup_method_including_private(
            receiver_ty,
            *method_name,
            generic_args,
            receiver,
            tgt_expr,
        );
        match resolved {
            Ok((func, visible)) => {
                if !visible {
                    self.push_diagnostic(InferenceDiagnostic::PrivateAssocItem {
                        id: tgt_expr.into(),
                        item: func.def_id.into(),
                    })
                }
                self.check_method_call(tgt_expr, args, func.sig, expected)
            }
            // Failed to resolve, report diagnostic and try to resolve as call to field access or
            // assoc function
            Err(_) => {
                let field_with_same_name_exists =
                    match self.lookup_field(tgt_expr, receiver_ty, method_name) {
                        Some((ty, field_id, adjustments, _public)) => {
                            self.write_expr_adj(receiver, &adjustments);
                            self.result.field_resolutions.insert(tgt_expr, field_id);
                            Some(ty)
                        }
                        None => None,
                    };

                let assoc_func_with_same_name =
                    self.with_method_resolution(tgt_expr.into(), receiver.into(), |ctx| {
                        if !matches!(
                            receiver_ty.kind(),
                            TyKind::Infer(InferTy::TyVar(_)) | TyKind::Error(_)
                        ) {
                            ctx.probe_for_name(
                                method_resolution::Mode::Path,
                                *method_name,
                                receiver_ty,
                            )
                        } else {
                            Err(MethodError::ErrorReported)
                        }
                    });
                let assoc_func_with_same_name = match assoc_func_with_same_name {
                    Ok(method_resolution::Pick {
                        item: CandidateId::FunctionId(def_id), ..
                    })
                    | Err(MethodError::PrivateMatch(method_resolution::Pick {
                        item: CandidateId::FunctionId(def_id),
                        ..
                    })) => Some(self.instantiate_erroneous_method(def_id)),
                    _ => None,
                };

                self.push_diagnostic(InferenceDiagnostic::UnresolvedMethodCall {
                    expr: tgt_expr,
                    receiver: receiver_ty.store(),
                    name: *method_name,
                    field_with_same_name: field_with_same_name_exists.map(|it| it.store()),
                    assoc_func_with_same_name: assoc_func_with_same_name.map(|it| it.def_id),
                });

                let recovered = match assoc_func_with_same_name {
                    Some(it) => Some((
                        Ty::new_fn_def(
                            self.interner(),
                            CallableDefId::FunctionId(it.def_id).into(),
                            it.args,
                        ),
                        it.sig,
                        true,
                    )),
                    None => field_with_same_name_exists.and_then(|field_ty| {
                        let callable_sig = field_ty.callable_sig(self.interner())?;
                        let callable_sig = self.infcx().instantiate_binder_with_fresh_vars(
                            tgt_expr.into(),
                            BoundRegionConversionTime::FnCall,
                            callable_sig,
                        );
                        Some((field_ty, callable_sig, false))
                    }),
                };
                match recovered {
                    Some((callee_ty, sig, strip_first)) => self.infer_method_call_as_call(
                        tgt_expr,
                        args,
                        callee_ty,
                        sig.inputs_and_output.inputs().get(strip_first as usize..).unwrap_or(&[]),
                        sig.output(),
                        &[],
                        true,
                        expected,
                    ),
                    None => {
                        for &arg in args.iter() {
                            self.infer_expr_no_expect(arg, ExprIsRead::Yes);
                        }
                        self.err_ty()
                    }
                }
            }
        }
    }

    fn check_method_call(
        &mut self,
        tgt_expr: ExprId,
        args: &[ExprId],
        sig: FnSig<'db>,
        expected: &Expectation<'db>,
    ) -> Ty<'db> {
        let param_tys = if !sig.inputs_and_output.inputs().is_empty() {
            &sig.inputs_and_output.inputs()[1..]
        } else {
            &[]
        };
        let ret_ty = sig.output();

        self.check_call_arguments(
            tgt_expr,
            param_tys,
            ret_ty,
            expected,
            args,
            &[],
            sig.c_variadic(),
            TupleArgumentsFlag::DontTupleArguments,
        );
        ret_ty
    }

    /// Generic function that factors out common logic from function calls,
    /// method calls and overloaded operators.
    pub(super) fn check_call_arguments(
        &mut self,
        call_expr: ExprId,
        // Types (as defined in the *signature* of the target function)
        formal_input_tys: &[Ty<'db>],
        formal_output: Ty<'db>,
        // Expected output from the parent expression or statement
        expectation: &Expectation<'db>,
        // The expressions for each provided argument
        provided_args: &[ExprId],
        skip_indices: &[u32],
        // Whether the function is variadic, for example when imported from C
        c_variadic: bool,
        // Whether the arguments have been bundled in a tuple (ex: closures)
        tuple_arguments: TupleArgumentsFlag,
    ) {
        let frame = self.frame;
        let formal_input_tys = {
            let mut generalized = Vec::with_capacity_in(formal_input_tys.len(), frame);
            for &ty in formal_input_tys {
                let generalized_ty = self.table.next_ty_var(call_expr.into());
                let _ = self.demand_eqtype(call_expr.into(), ty, generalized_ty);
                generalized.push(generalized_ty);
            }
            generalized
        };

        // First, let's unify the formal method signature with the expectation eagerly.
        // We use this to guide coercion inference; it's output is "fudged" which means
        // any remaining type variables are assigned to new, unrelated variables. This
        // is because the inference guidance here is only speculative.
        let formal_output = self.table.resolve_vars_with_obligations(formal_output);
        let expected_input_tys: Option<Vec<Ty<'db>, &Frame>> = expectation
            .only_has_type(&mut self.table)
            .and_then(|expected_output| {
                self.table
                    .infer_ctxt
                    .fudge_inference_if_ok(|| {
                        let mut ocx = ObligationCtxt::new(&self.table.infer_ctxt);

                        // Attempt to apply a subtyping relationship between the formal
                        // return type (likely containing type variables if the function
                        // is polymorphic) and the expected return type.
                        // No argument expectations are produced if unification fails.
                        let origin = ObligationCause::new(call_expr);
                        ocx.sup(&origin, self.table.param_env, expected_output, formal_output)?;

                        for &ty in &formal_input_tys {
                            ocx.register_obligation(Obligation::new(
                                self.interner(),
                                ObligationCause::new(call_expr),
                                self.table.param_env,
                                ClauseKind::WellFormed(ty.into()),
                            ));
                        }

                        if !ocx.try_evaluate_obligations().is_empty() {
                            return Err(TypeError::Mismatch);
                        }

                        // Record all the argument types, with the args
                        // produced from the above subtyping unification.
                        Ok(Some(FrameList(formal_input_tys.clone()))) // clone: a frame list the fudge hands back
                    })
                    .ok()
            })
            .unwrap_or_default()
            .map(|list| list.0);

        // If the arguments should be wrapped in a tuple (ex: closures), unwrap them here
        let (formal_input_tys, expected_input_tys) = if tuple_arguments
            == TupleArgumentsFlag::TupleArguments
        {
            let tuple_type = self.structurally_resolve_type(call_expr.into(), formal_input_tys[0]);
            match tuple_type.kind() {
                // We expected a tuple and got a tuple
                TyKind::Tuple(arg_types) => {
                    // Argument length differs. The mismatch is reported below by the
                    // shared `MismatchedArgCount` push (with `is_fn_trait_call = true`,
                    // which the diagnostic surface renders as E0057).
                    let expected_input_tys = match expected_input_tys {
                        Some(expected_input_tys) => match expected_input_tys.first() {
                            Some(ty) => match ty.kind() {
                                TyKind::Tuple(tys) => {
                                    let mut list = Vec::with_capacity_in(tys.len(), frame);
                                    list.extend_from_slice(tys.as_slice());
                                    Some(list)
                                }
                                _ => None,
                            },
                            None => None,
                        },
                        None => None,
                    };
                    let mut formal = Vec::with_capacity_in(arg_types.len(), frame);
                    formal.extend_from_slice(arg_types.as_slice());
                    (formal, expected_input_tys)
                }
                _ => {
                    // Otherwise, there's a mismatch, so clear out what we're expecting, and set
                    // our input types to err_args so we don't blow up the error messages
                    if !tuple_type.is_ty_error() {
                        self.push_diagnostic(InferenceDiagnostic::FnTraitArgsNotTuple { call_expr });
                    }
                    let mut formal = Vec::with_capacity_in(provided_args.len(), frame);
                    formal.resize(provided_args.len(), self.types.types.error);
                    (formal, None)
                }
            }
        } else {
            (formal_input_tys, expected_input_tys)
        };

        // If there are no external expectations at the call site, just use the types from the function defn
        let expected_input_tys: &[Ty<'db>] = match &expected_input_tys {
            Some(expected_input_tys) => {
                assert_eq!(expected_input_tys.len(), formal_input_tys.len());
                expected_input_tys
            }
            None => &formal_input_tys,
        };

        let minimum_input_count = expected_input_tys.len();
        let provided_arg_count = provided_args.len() - skip_indices.len();

        // Keep track of whether we *could possibly* be satisfied, i.e. whether we're on the happy path
        // if the wrong number of arguments were supplied, we CAN'T be satisfied,
        // and if we're c_variadic, the supplied arguments must be >= the minimum count from the function
        // otherwise, they need to be identical, because rust doesn't currently support variadic functions
        let args_count_matches = if c_variadic {
            provided_arg_count >= minimum_input_count
        } else {
            provided_arg_count == minimum_input_count
        };

        // We introduce a helper function to demand that a given argument satisfy a given input
        // This is more complicated than just checking type equality, as arguments could be coerced
        // This version writes those types back so further type checking uses the narrowed types
        let demand_compatible = |this: &mut InferenceContext<'db, 'f>, idx: usize, provided_arg: ExprId| {
            let formal_input_ty: Ty<'db> = formal_input_tys[idx];
            let expected_input_ty: Ty<'db> = expected_input_tys[idx];

            // We're on the happy path here, so we'll do a more involved check and write back types
            // To check compatibility, we'll do 3 things:
            // 1. Unify the provided argument with the expected type
            let expectation = Expectation::rvalue_hint(this, expected_input_ty);

            let checked_ty = this.infer_expr_inner(provided_arg, &expectation, ExprIsRead::Yes);

            // 2. Coerce to the most detailed type that could be coerced
            //    to, which is `expected_ty` if `rvalue_hint` returns an
            //    `ExpectHasType(expected_ty)`, or the `formal_ty` otherwise.
            let coerced_ty = expectation.only_has_type(&mut this.table).unwrap_or(formal_input_ty);

            // Cause selection errors caused by resolving a single argument to point at the
            // argument and not the call. This lets us customize the span pointed to in the
            // fulfillment error to be more accurate.
            let coerced_ty = this.table.resolve_vars_with_obligations(coerced_ty);

            let coerce_error = this
                .coerce(provided_arg, checked_ty, coerced_ty, AllowTwoPhase::Yes, ExprIsRead::Yes)
                .err();
            if coerce_error.is_some() {
                return Err((coerce_error, coerced_ty, checked_ty));
            }

            // 3. Check if the formal type is actually equal to the checked one
            //    and register any such obligations for future type checks.
            let formal_ty_error = this
                .table
                .infer_ctxt
                .at(&ObligationCause::new(provided_arg), this.table.param_env)
                .eq(formal_input_ty, coerced_ty);

            // If neither check failed, the types are compatible
            match formal_ty_error {
                Ok(InferOk { obligations, value: () }) => {
                    this.table.register_predicates(obligations);
                    Ok(())
                }
                Err(err) => Err((Some(err), coerced_ty, checked_ty)),
            }
        };

        // rustc's `compatibility_diagonal`, by position among the arguments not taken as legacy
        // const generics: those below `minimum_input_count` that did not fit their own input
        // (the rest are `Incompatible(None)`, never checked against one).
        let mut incompatible = SmallVec::<[u32; 2]>::new();

        // Check the arguments.
        // We do this in a pretty awful way: first we type-check any arguments
        // that are not closures, then we type-check the closures. This is so
        // that we have more information about the types of arguments when we
        // type-check the functions. This isn't really the right way to do this.
        for check_closures in [false, true] {
            // More awful hacks: before we check argument types, try to do
            // an "opportunistic" trait resolution of any trait bounds on
            // the call. This helps coercions.
            if check_closures {
                self.table.select_obligations_where_possible();
            }

            let mut skip_indices = skip_indices.iter().copied().peekable();
            let mut position = 0;
            // Check each argument, to satisfy the input it was provided for
            // Visually, we're traveling down the diagonal of the compatibility matrix
            for (idx, arg) in provided_args.iter().enumerate() {
                if skip_indices.peek() == Some(&(idx as u32)) {
                    skip_indices.next();
                    continue;
                }
                let idx = position;
                position += 1;
                // Only in the first pass: a closure can't diverge, but an argument before it
                // can (`f(panic!(), || {})`).
                if !check_closures {
                    self.warn_if_unreachable(UnreachableAt::Expr(*arg));
                }

                // For this check, we do *not* want to treat async coroutine closures (async blocks)
                // as proper closures. Doing so would regress type inference when feeding
                // the return value of an argument-position async block to an argument-position
                // closure wrapped in a block.
                // See <https://github.com/rust-lang/rust/issues/112225>.
                let is_closure = if let Expr::Closure { closure_kind, .. } = self.store.expr(*arg) {
                    !matches!(closure_kind, ClosureKind::OldCoroutine(_))
                } else {
                    false
                };
                if is_closure != check_closures {
                    continue;
                }

                if idx >= minimum_input_count {
                    // Make sure we've checked this expr at least once.
                    self.infer_expr_no_expect(*arg, ExprIsRead::Yes);
                    continue;
                }

                if let Err((_error, expected, found)) = demand_compatible(self, idx, *arg) {
                    incompatible.push(idx as u32);
                    if args_count_matches {
                        // Don't report type mismatches if there is a mismatch in args count.
                        self.emit_type_mismatch((*arg).into(), expected, found);
                    }
                }
            }
        }

        // rustc's `call_appears_satisfied`.
        if !args_count_matches || !incompatible.is_empty() {
            let args = self.call_arguments(
                call_expr,
                provided_args,
                skip_indices,
                &formal_input_tys,
                &expected_input_tys,
                &incompatible,
                c_variadic,
            );
            self.push_diagnostic(if args_count_matches {
                InferenceDiagnostic::IncorrectArguments { call_expr, args }
            } else {
                InferenceDiagnostic::MismatchedArgCount {
                    call_expr,
                    expected: expected_input_tys.len() + skip_indices.len(),
                    found: provided_args.len(),
                    is_fn_trait_call: tuple_arguments == TupleArgumentsFlag::TupleArguments,
                    args,
                }
            });
        }
    }

    /// rustc's `report_arg_errors` inputs of a call whose arguments do not fit: the callee, the
    /// inputs and the provided types as found, and `ArgMatrix::new`'s compatibility, its
    /// diagonal as `check_argument_types` found it (`incompatible` the positions that failed).
    fn call_arguments(
        &mut self,
        call_expr: ExprId,
        provided_args: &[ExprId],
        skip_indices: &[u32],
        formal_input_tys: &[Ty<'db>],
        expected_input_tys: &[Ty<'db>],
        incompatible: &[u32],
        c_variadic: bool,
    ) -> Box<CallArguments> {
        let inputs = formal_input_tys.len();
        // alloc: error path, one per call whose arguments do not fit
        let provided: Vec<Ty<'db>> = provided_args
            .iter()
            .enumerate()
            .filter(|&(idx, _)| !skip_indices.contains(&(idx as u32)))
            .take(if c_variadic { inputs } else { usize::MAX })
            .map(|(_, &arg)| {
                let ty = self.expr_ty_after_adjustments(arg);
                self.table.resolve_vars_if_possible(ty)
            })
            .collect(); // alloc: error path
        let inputs_tys: Vec<(Ty<'db>, Ty<'db>)> = formal_input_tys
            .iter()
            .zip(expected_input_tys)
            .map(|(&formal, &expected)| {
                (self.table.resolve_vars_if_possible(formal), self.table.resolve_vars_if_possible(expected))
            })
            .collect(); // alloc: error path

        let mut compatible_ends = Vec::with_capacity(provided.len());
        let mut compatible = Vec::new(); // alloc: error path
        for (provided_idx, &arg_ty) in provided.iter().enumerate() {
            for (expected_idx, &(formal, expected)) in inputs_tys.iter().enumerate() {
                let fits = if provided_idx == expected_idx {
                    !incompatible.contains(&(provided_idx as u32))
                } else {
                    self.check_compatible(arg_ty, formal, expected)
                };
                if fits {
                    compatible.push(expected_idx as u32);
                }
            }
            compatible_ends.push(compatible.len() as u32);
        }

        let (callee, method) = match self.store.expr(call_expr) {
            Expr::Call { callee, .. } => (self.expr_ty_after_adjustments(callee), None),
            Expr::MethodCall { receiver, .. } => (
                self.expr_ty_after_adjustments(receiver),
                self.result.method_resolution(call_expr).map(|(func, _)| func),
            ),
            _ => (self.types.types.error, None),
        };
        // alloc: error path, the diagnostic's own record
        Box::new(CallArguments {
            callee: self.ty_as_found(callee),
            method,
            is_method: matches!(self.store.expr(call_expr), Expr::MethodCall { .. }),
            c_variadic,
            // alloc: error path
            formal_inputs: inputs_tys.iter().map(|&(formal, _)| self.ty_as_found(formal)).collect(),
            // alloc: error path
            expected_inputs: inputs_tys.iter().map(|&(_, expected)| self.ty_as_found(expected)).collect(),
            // alloc: error path
            provided: provided.iter().map(|&ty| self.ty_as_found(ty)).collect(),
            compatible_ends: compatible_ends.into_boxed_slice(),
            compatible: compatible.into_boxed_slice(),
        })
    }

    /// rustc's `ArgMatchingCtxt::check_compatible` off the diagonal: whether an argument of
    /// type `arg_ty` would fit the input `(formal, expected)`, inference left as it was.
    fn check_compatible(&mut self, arg_ty: Ty<'db>, formal: Ty<'db>, expected: Ty<'db>) -> bool {
        // An error type fits nothing, so the heuristics don't pass errors into every input. An
        // error region is no error: a type inference recorded keeps its regions as errors.
        if formal.references_non_lt_error() || expected.references_non_lt_error() {
            return false;
        }
        let expectation = Expectation::rvalue_hint(self, expected);
        let coerced_ty = expectation.only_has_type(&mut self.table).unwrap_or(formal);
        if !self.may_coerce(arg_ty, coerced_ty) {
            return false;
        }
        let infcx = &self.table.infer_ctxt;
        let cause = ObligationCause::dummy();
        let subtyping_error = infcx
            .probe(|_| infcx.sup_raw(&cause, self.table.param_env, formal, coerced_ty).is_err());
        !subtyping_error && !coerced_ty.references_non_lt_error() && !arg_ty.references_non_lt_error()
    }

    pub(super) fn with_breakable_ctx<T>(
        &mut self,
        kind: BreakableKind,
        ty: Option<Ty<'db>>,
        label: Option<LabelId>,
        cb: impl FnOnce(&mut Self) -> T,
    ) -> (BreakableContext<'db, 'f>, T) {
        let frame = self.frame;
        self.breakables.push({
            BreakableContext {
                kind,
                may_break: false,
                coerce: ty.map(|ty| CoerceMany::new(ty, frame)),
                label,
            }
        });
        let res = cb(self);
        let ctx = self.breakables.pop().expect("breakable stack broken");
        (ctx, res)
    }

    /// rustc's `require_label_in_labeled_block`: whether the innermost of the labeled blocks,
    /// loops and closures around an unlabeled `break`/`continue` is a labeled block, E0695. An
    /// unlabeled block, or a `try` block (whose label is desugared), is not one.
    fn in_labeled_block(&self) -> bool {
        self.breakables
            .iter()
            .rev()
            .find_map(|ctx| match ctx.kind {
                BreakableKind::Block => {
                    ctx.label.filter(|&label| !self.store[label].name.is_generated()).map(|_| true)
                }
                BreakableKind::Loop | BreakableKind::Border => Some(false),
            })
            .unwrap_or(false)
    }

    /// `ty` as inference has it at an error (`types_at_error`, for one type).
    pub(super) fn ty_at_error(&self, ty: Ty<'db>) -> Option<StoredTy> {
        self.types_at_error(ty, self.types.types.unit).map(|it| it.0)
    }

    /// `ty` as an error reports it: an undecided numeric literal's type fresh (`ty_at_error`),
    /// any other variable an error.
    fn ty_as_found(&self, ty: Ty<'db>) -> StoredTy {
        self.ty_at_error(ty).unwrap_or_else(|| {
            let ty = self.table.resolve_vars_if_possible(ty);
            ty.fold_with(&mut ReplaceInferWithError::new(self.interner())).store()
        })
    }

    /// rustc's `warn_if_unreachable`: `what` is unreachable when what came before it in its
    /// parent diverged. Once per divergence (`WarnedAlways`). The `#[automatically_derived]`
    /// rule is applied when the body is done (`in_automatically_derived_impl`).
    pub(super) fn warn_if_unreachable(&mut self, what: UnreachableAt) {
        let Diverges::Always { span, custom_note } = self.diverges else { return };
        self.diverges = Diverges::WarnedAlways;
        self.unreachable.push(Unreachable { what, after: span, note: custom_note });
    }

    /// The lang item the callee of `call` names, when hir-def wrote it as a lang path: only
    /// its desugarings (`?`, `for`, `try` blocks, `format_args!`) do.
    fn lang_callee(&self, call: ExprId) -> Option<LangItemTarget> {
        let Expr::Call { callee, .. } = self.store.expr(call) else { return None };
        match self.store.expr(callee) {
            Expr::Path(Path::LangItem(target, _)) => Some(target),
            _ => None,
        }
    }

    /// A `try` block's `Try::from_output(..)` of its tail: rustc's call with a `TryBlock`
    /// desugaring span.
    fn is_try_block_output(&self, call: ExprId) -> bool {
        let Some(from_output) = self.lang_items.TryTraitFromOutput else { return false };
        self.lang_callee(call) == Some(LangItemTarget::FunctionId(from_output))
    }

    /// A `try` block's `Try::from_output(())` where the block has no tail: hir-def allocates
    /// that `()` after the callee's path, and a tail that is written before it.
    fn is_try_block_output_of_unit(&self, call: ExprId) -> bool {
        let Expr::Call { callee, args: &[arg] } = self.store.expr(call) else { return false };
        arg > callee
            && matches!(self.store.expr(arg), Expr::Tuple { exprs: [] })
            && self.is_try_block_output(call)
    }

    /// Whether a `match` of `scrutinee` is a desugaring (rustc's `MatchSource` other than
    /// `Normal`): `?` matches on `Try::branch(..)`, a `for` loop on `IntoIterator::into_iter(..)`
    /// and `Iterator::next(..)`.
    fn is_desugared_match(&self, scrutinee: ExprId) -> bool {
        self.lang_callee(scrutinee).is_some()
    }
}

/// Controls whether the arguments are tupled. This is used for the call
/// operator.
///
/// Tupling means that all call-side arguments are packed into a tuple and
/// passed as a single parameter. For example, if tupling is enabled, this
/// function:
/// ```
/// fn f(x: (isize, isize)) {}
/// ```
/// Can be called as:
/// ```ignore UNSOLVED (can this be done in user code?)
/// # fn f(x: (isize, isize)) {}
/// f(1, 2);
/// ```
/// Instead of:
/// ```
/// # fn f(x: (isize, isize)) {}
/// f((1, 2));
/// ```
#[derive(Copy, Clone, Eq, PartialEq)]
pub(super) enum TupleArgumentsFlag {
    DontTupleArguments,
    TupleArguments,
}
