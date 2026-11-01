//! `deref_nullptr` (deny): `*` of a pointer that is null by how it is written (`ptr::null()`,
//! `0 as *const T`, casts of them), unless its pointee is zero-sized or the place is only
//! borrowed raw (`&raw const *ptr::null::<T>()`, `ptr::addr_of!`).
//!
//! Follows rustc's `compiler/rustc_lint/src/builtin.rs` (`DerefNullPtr`) and
//! `compiler/rustc_lint/src/lints.rs` (`BuiltinDerefNullptr`) (MIT or Apache-2.0).

use blaze_hash::HashSet;
use hir_def::hir::{
    Expr, ExprId, UnaryOp,
    type_ref::{Rawness, TypeRef},
};
use hir_ty::next_solver::TyKind;
use rustc_type_ir::inherent::IntoKind as _;

use super::{is_zero, layout, span};
use crate::{
    lints::bodies::{BodyCx, ExprKind, ExprKinds, Hooks, LatePass, LintSet, lint_set, types::util},
    lower::{Found, Phase},
    model::Diag,
};

#[derive(Default)]
pub(crate) struct DerefNullPtr {
    /// The places of the body being walked that are borrowed raw (`&raw const <place>`): the
    /// parents rustc's `parent_hir_node` finds, seen before the places in them.
    raw_borrowed: HashSet<ExprId>,
}

impl LatePass for DerefNullPtr {
    const LINTS: LintSet = lint_set(["deref_nullptr"]);
    const HOOKS: Hooks = Hooks::BODY;
    const EXPRS: ExprKinds = ExprKinds::of(&[ExprKind::Ref, ExprKind::UnaryOp]);

    fn check_body(&mut self, _b: &BodyCx<'_, '_, '_>, _out: &mut Vec<Found>) {
        self.raw_borrowed.clear();
    }

    fn check_expr(&mut self, b: &BodyCx<'_, '_, '_>, expr: ExprId, out: &mut Vec<Found>) {
        match b.store().expr(expr) {
            Expr::Ref { expr: place, rawness: Rawness::RawPtr, .. } => {
                self.raw_borrowed.insert(place);
            }
            Expr::UnaryOp { expr: pointer, op: UnaryOp::Deref } => {
                if is_null_ptr(b, pointer) && !self.raw_borrowed.contains(&expr) {
                    let (Some(span), Some(at)) = (span(b, expr), b.expr_node(expr)) else { return };
                    // clone: a lint's span, for a lint found
                    let mut diag = Diag::lint("deref_nullptr", "dereferencing a null pointer", span.clone());
                    diag.span_label(span, "this code causes undefined behavior when executed");
                    out.push(b.lint(diag, Phase::Late, at));
                }
            }
            _ => {}
        }
    }
}

/// rustc's `is_null_ptr`: `expr` is a raw pointer to a type that is not zero-sized, made by
/// `ptr::null()` or `ptr::null_mut()`, or a cast to a raw pointer of `0` or of such a pointer.
fn is_null_ptr(b: &BodyCx<'_, '_, '_>, expr: ExprId) -> bool {
    let TyKind::RawPtr(pointee, _) = util::ty(b, expr).kind() else { return false };
    if layout(b, pointee).is_some_and(|it| it.size == 0) {
        return false;
    }
    match b.store().expr(expr) {
        Expr::Cast { expr: inner, type_ref } => {
            matches!(b.store().type_ref(type_ref), TypeRef::RawPtr(..)) && (is_zero(b, inner) || is_null_ptr(b, inner))
        }
        Expr::Call { callee, .. } => {
            super::callee_item(b, callee).is_some_and(|it| it == util::items().ptr_null || it == util::items().ptr_null_mut)
        }
        _ => false,
    }
}
