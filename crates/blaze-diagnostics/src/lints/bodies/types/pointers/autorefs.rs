//! `dangerous_implicit_autorefs` (deny): a reference taken implicitly (an autoref, or the
//! `&` of an overloaded deref) of a place that dereferences a raw pointer, through field and
//! index projections: `(*ptr)[..16]`, `(*ptr).field.len()`, `&raw const (*ptr).field` through
//! a `Deref` impl, a call of a method marked `#[rustc_no_implicit_autorefs]`.
//!
//! Follows rustc's `compiler/rustc_lint/src/autorefs.rs` and `compiler/rustc_lint/src/lints.rs`
//! (`ImplicitUnsafeAutorefsDiag`) (MIT or Apache-2.0).

use hir_def::{
    hir::{Expr, ExprId, UnaryOp, type_ref::Rawness},
    signatures::FunctionSignature,
};
use hir_ty::{
    Adjust, Adjustment, AutoBorrow, OverloadedDeref, PointerCast, infer::AutoBorrowMutability, next_solver::TyKind,
};
use rustc_type_ir::{Mutability, UintTy, inherent::IntoKind};

use super::{fn_def_span, has_attr, source_callsite, span, type_info};
use crate::{
    lints::bodies::{BodyCx, ExprKinds, ExprKind, LatePass, LintSet, lint_set, types::util},
    lower::{Found, Phase},
    model::{Applicability, Diag},
};

#[derive(Default)]
pub(crate) struct ImplicitAutorefs;

impl LatePass for ImplicitAutorefs {
    const LINTS: LintSet = lint_set(["dangerous_implicit_autorefs"]);
    const EXPRS: ExprKinds = ExprKinds::of(&[ExprKind::Ref, ExprKind::Index, ExprKind::MethodCall, ExprKind::Field]);

    fn check_expr(&mut self, b: &BodyCx<'_, '_, '_>, expr: ExprId, out: &mut Vec<Found>) {
        if let Some(found) = check(b, expr) {
            out.push(found);
        }
    }
}

fn check(b: &BodyCx<'_, '_, '_>, expr: ExprId) -> Option<Found> {
    let store = b.store();
    let mut is_coming_from_deref = false;
    let inner = match store.expr(expr) {
        Expr::Ref { expr: inner, rawness: Rawness::RawPtr, .. } => match store.expr(inner) {
            Expr::UnaryOp { expr: inner, op: UnaryOp::Deref } => {
                is_coming_from_deref = true;
                inner
            }
            _ => return None,
        },
        Expr::Index { base, .. } => base,
        Expr::MethodCall { receiver, .. } => receiver,
        Expr::Field { expr: inner, .. } => inner,
        _ => return None,
    };

    let mut adjustments = b.infer.expr_adjustment(inner)?;
    if is_builtin_index(b, expr) {
        adjustments = without_index_borrow(adjustments);
    }
    let adjustments = peel_derefs_adjustments(adjustments);
    let [adjustment] = adjustments else { return None };
    let (borrow_mutbl, through_overloaded_deref) = has_implicit_borrow(adjustment)?;
    let Expr::UnaryOp { expr: dereferenced, op: UnaryOp::Deref } = store.expr(peel_place_mappers(b, inner)) else {
        return None;
    };
    if !util::ty(b, dereferenced).is_raw_ptr() {
        return None;
    }
    let method = match store.expr(expr) {
        Expr::MethodCall { .. } => util::method_fn(b, expr),
        _ => None,
    };
    if method.is_some_and(|it| !has_attr(b.db(), it, "rustc_no_implicit_autorefs")) {
        return None;
    }

    let at = b.expr_node(expr)?;
    let raw_ptr_span = span(b, dereferenced)?;
    let inner_span = span(b, inner)?;
    let raw_ptr_ty = b.cx.ty(&type_info(b, dereferenced)?.original);
    let mut diag = Diag::lint(
        "dangerous_implicit_autorefs",
        "implicit autoref creates a reference to the dereference of a raw pointer",
        source_callsite(span(b, expr)?),
    );
    // alloc: the lint's text, for a lint found
    diag.span_label(raw_ptr_span, format!("this raw pointer has type `{raw_ptr_ty}`"));
    diag.note("creating a reference requires the pointer target to be valid and imposes aliasing requirements");
    if through_overloaded_deref {
        diag.note("references are created through calls to explicit `Deref(Mut)::deref(_mut)` implementations");
    } else {
        let autoref_ty = b.cx.ty(&type_info(b, inner)?.adjusted());
        diag.span_note(
            // clone: a lint's span, for a lint found
            inner_span.clone(),
            // alloc: the lint's text, for a lint found
            format!("autoref is being applied to this expression, resulting in: `{autoref_ty}`"),
        );
    }
    if let Some(method) = method {
        // alloc: the lint's text, for a lint found
        let name = FunctionSignature::of(b.db(), method).name.as_str().to_owned();
        // alloc: the lint's text, for a lint found
        diag.span_note(fn_def_span(b, method), format!("method calls to `{name}` require a reference"));
    }
    let mutbl = match borrow_mutbl {
        Mutability::Not => "&",
        Mutability::Mut => "&mut ",
    };
    let deref = if is_coming_from_deref { "*" } else { "" };
    diag.multipart_suggestion(
        "try using a raw pointer method instead; or if this reference is intentional, make it explicit",
        // alloc: the lint's text, for a lint found
        vec![(inner_span.shrink_to_lo(), format!("({mutbl}{deref}")), (inner_span.shrink_to_hi(), ")".to_owned())],
        Applicability::MaybeIncorrect,
    );
    Some(b.lint(diag, Phase::Late, at))
}

/// rustc's `peel_place_mappers`: through field accesses and indexing to the place they
/// project from.
fn peel_place_mappers(b: &BodyCx<'_, '_, '_>, mut expr: ExprId) -> ExprId {
    loop {
        match b.store().expr(expr) {
            Expr::Index { base, .. } => expr = base,
            Expr::Field { expr: inner, .. } => expr = inner,
            _ => return expr,
        }
    }
}

/// Whether `expr` is an index of an array or slice by a `usize`, which rustc's writeback
/// (`fix_index_builtin_expr`) makes the builtin index rather than a call of `Index::index`.
fn is_builtin_index(b: &BodyCx<'_, '_, '_>, expr: ExprId) -> bool {
    let Expr::Index { base, index } = b.store().expr(expr) else { return false };
    let TyKind::Ref(_, base, _) = util::ty_adjusted(b, base).kind() else { return false };
    matches!(base.kind(), TyKind::Array(..) | TyKind::Slice(_))
        && matches!(util::ty_adjusted(b, index).kind(), TyKind::Uint(UintTy::Usize))
}

/// The adjustments of a builtin index's base once rustc's writeback drops the borrow
/// `Index::index` took, and the unsizing of an array after it.
fn without_index_borrow(adjustments: &[Adjustment]) -> &[Adjustment] {
    match adjustments {
        [rest @ .., _, Adjustment { kind: Adjust::Pointer(PointerCast::Unsize), .. }] | [rest @ .., _] => rest,
        [] => adjustments,
    }
}

/// rustc's `peel_derefs_adjustments`: the adjustments from the last of the leading derefs on,
/// that one kept.
fn peel_derefs_adjustments(mut adjustments: &[Adjustment]) -> &[Adjustment] {
    while let [Adjustment { kind: Adjust::Deref(_), .. }, rest @ ..] = adjustments
        && !rest.is_empty()
    {
        adjustments = rest;
    }
    adjustments
}

/// rustc's `has_implicit_borrow`: the mutability of the reference the adjustment takes, and
/// whether it is taken by an overloaded deref.
fn has_implicit_borrow(adjustment: &Adjustment) -> Option<(Mutability, bool)> {
    match &adjustment.kind {
        Adjust::Deref(Some(OverloadedDeref(mutbl))) => Some((*mutbl, true)),
        Adjust::Borrow(AutoBorrow::Ref(mutbl)) => Some((
            match mutbl {
                AutoBorrowMutability::Mut { .. } => Mutability::Mut,
                AutoBorrowMutability::Not => Mutability::Not,
            },
            false,
        )),
        Adjust::NeverToAny | Adjust::Pointer(_) | Adjust::Deref(None) | Adjust::Borrow(AutoBorrow::RawPtr(_)) => None,
    }
}
