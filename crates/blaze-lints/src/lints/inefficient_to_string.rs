//! `inefficient_to_string`: `x.to_string()` where `x` is a reference to a `&str`, a `String` or
//! a `Cow<str>`, which before Rust 1.82 went through `ToString`'s blanket impl rather than the
//! specialization of what it points to.
//!
//! Follows clippy's `clippy_lints/src/methods/inefficient_to_string.rs` (MIT or Apache-2.0),
//! written from what is known of its source rather than read from a checkout: no copy of
//! clippy's source was at hand. Its name, message, help, span, suggestion and applicability are
//! clippy's, and as clippy it says nothing when the crate keeps to Rust 1.82 or later
//! (`msrvs::SPECIALIZED_TO_STRING_FOR_REFS`), where the specialization is reached.

use hir::HirDisplay as _;
use syntax::view::{self as ast, AstNode as _};

use super::{
    implicit_clone::{COW, STRING, TO_STRING, is_std_adt, of_trait, peel_refs},
    utils,
};
use crate::{check::Cx, diag::Applicability, msrvs};

const LINT: super::Implemented = super::Implemented::InefficientToString;

pub(super) fn check<'t>(cx: &mut Cx<'_, 't>, node: ast::Node<'t>) {
    if !cx.on(LINT) || cx.conf().meets(msrvs::SPECIALIZED_TO_STRING_FOR_REFS) {
        return;
    }
    let Some(method) = ast::MethodCallExpr::cast(node) else { return };
    let Some(call) = utils::method_call(cx, ast::Expr::MethodCallExpr(method)) else { return };
    if call.name != "to_string" || !call.args().is_empty() {
        return;
    }
    let Some(function) = cx.method(method) else { return };
    if !of_trait(cx, function, TO_STRING) {
        return;
    }
    // `to_string` takes `&self`: the receiver, adjusted, is a reference to `Self`.
    let Some(arg_ty) = cx.adjusted_ty(call.receiver) else { return };
    let Some((self_ty, _)) = arg_ty.as_reference() else { return };
    let (deref_self_ty, deref_count) = peel_refs(self_ty.clone());
    if deref_count == 0 || !specializes_to_string(cx, &deref_self_ty) {
        return;
    }
    let Some(range) = cx.range(node) else { return };
    let db = cx.db();
    let target = cx.krate().to_display_target(db);
    let message = format!("calling `to_string` on `{}`", arg_ty.display(db, target));
    let help = format!(
        "`{}` implements `ToString` through a slower blanket impl, but `{}` has a fast \
         specialization of `ToString`",
        self_ty.display(db, target),
        deref_self_ty.display(db, target)
    );
    let sugg = format!(
        "({}{}).to_string()",
        "*".repeat(deref_count),
        cx.source_of(call.receiver.syntax())
    );
    let span = cx.span(range);
    cx.lint(LINT, node, span.clone(), message).help(help).suggestion(
        span,
        "try dereferencing the receiver",
        sugg,
        Applicability::MachineApplicable,
    );
}

/// clippy's `specializes_tostring`: `str`, `String` and `Cow<'_, str>`.
fn specializes_to_string(cx: &Cx<'_, '_>, ty: &hir::Type<'_>) -> bool {
    ty.is_str()
        || is_std_adt(cx, ty, STRING)
        || (is_std_adt(cx, ty, COW) && ty.type_arguments().next().is_some_and(|it| it.is_str()))
}
