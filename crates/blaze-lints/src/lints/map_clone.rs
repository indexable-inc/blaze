//! `map_clone`: `.map(|x| *x)`, `.map(|&x| x)` and `.map(|x| x.clone())` on an iterator, an
//! `Option` or a `Result`, which `copied()` or `cloned()` say.
//!
//! Follows clippy's `clippy_lints/src/methods/map_clone.rs` at rust-lang/rust
//! 12433c307b5a497388ffe2f4e49dbf1ab8c71fa0 (MIT or Apache-2.0), for closures; not ported:
//! `Clone::clone` as a path, passed or called in the closure.

use hir::{Adjust, AsAssocItem as _, Mutability};
use syntax::{TextRange, view as ast};

use ast::{AstNode as _, HasArgList as _, HasGenericArgs as _, HasName as _};

use super::utils;
use crate::{check::Cx, diag::Applicability, msrvs};

const LINT: super::Implemented = super::Implemented::MapClone;

pub(super) fn check<'t>(cx: &mut Cx<'_, 't>, expr: ast::Expr<'t>) {
    if !cx.on(LINT) || utils::method_name(expr) != Some("map") {
        return;
    }
    let ast::Expr::MethodCallExpr(method) = expr else { return };
    let Some(call) = utils::method_call(cx, expr) else { return };
    let [ast::Expr::ClosureExpr(closure)] = *call.args() else { return };
    if !should_run_lint(cx, method, call.receiver) {
        return;
    }
    let (Some(param), Some(body)) =
        (closure.param_list().and_then(|it| it.params().next()), closure.body())
    else {
        return;
    };
    let body = utils::peel_blocks(body);
    let (Some(range), Some(receiver)) = (cx.range(expr.syntax()), cx.range(call.receiver.syntax()))
    else {
        return;
    };
    match param.pat() {
        Some(ast::Pat::RefPat(pat)) if pat.mut_token().is_none() => {
            if let Some(ast::Pat::IdentPat(inner)) = pat.pat()
                && plain(inner)
                && names(body, inner)
            {
                lint_explicit_closure(cx, expr, range, receiver, true);
            }
        }
        Some(ast::Pat::IdentPat(binding)) if plain(binding) => match body {
            ast::Expr::PrefixExpr(deref) if deref.op_kind() == Some(ast::UnaryOp::Deref) => {
                if let Some(inner) = deref.expr()
                    && names(inner, binding)
                    && cx
                        .ty(inner)
                        .and_then(|it| it.as_reference())
                        .is_some_and(|(_, m)| m == Mutability::Shared)
                {
                    lint_explicit_closure(cx, expr, range, receiver, true);
                }
            }
            ast::Expr::MethodCallExpr(clone) => {
                let db = cx.db();
                let (Some(obj), Some(name)) = (clone.receiver(), clone.name_ref()) else { return };
                if name.text() != "clone"
                    || clone.arg_list().is_some_and(|it| it.args().next().is_some())
                    || !names(obj, binding)
                {
                    return;
                }
                let clone_trait = cx.lang_trait(hir::LangItem::Clone);
                let Some(function) = cx.method(clone) else { return };
                if clone_trait.is_none()
                    || function
                        .as_assoc_item(db)
                        .and_then(|it| it.container_or_implemented_trait(db))
                        != clone_trait
                {
                    return;
                }
                let adjustments = cx.adjustments(obj).unwrap_or_default();
                if adjustments.iter().any(|it| matches!(it.kind, Adjust::Deref(Some(_)))) {
                    return;
                }
                let Some(obj_ty) = cx.ty(obj) else { return };
                match obj_ty.as_reference() {
                    Some((inner, Mutability::Shared)) => {
                        let copy = inner.is_copy(db);
                        lint_explicit_closure(cx, expr, range, receiver, copy);
                    }
                    Some(_) => {}
                    None => {
                        let span = cx.span(TextRange::new(receiver.end(), range.end()));
                        cx.lint(
                            LINT,
                            expr.syntax(),
                            span.clone(),
                            "you are needlessly cloning iterator elements",
                        )
                        .suggestion(
                            span,
                            "remove the `map` call",
                            "",
                            Applicability::MachineApplicable,
                        );
                    }
                }
            }
            _ => {}
        },
        _ => {}
    }
}

/// clippy's `should_run_lint`: `map` of `Iterator`, or of `Option` or `Result` not called on
/// what `as_ref` gives, which `useless_asref` says better.
fn should_run_lint<'t>(
    cx: &Cx<'_, 't>,
    method: ast::MethodCallExpr<'t>,
    receiver: ast::Expr<'t>,
) -> bool {
    let db = cx.db();
    let Some(function) = cx.method(method) else { return false };
    let Some(item) = function.as_assoc_item(db) else { return false };
    let famous = cx.famous();
    let iterator = famous.core_iter_Iterator();
    if iterator.is_some() && item.container_or_implemented_trait(db) == iterator {
        return true;
    }
    let Some(adt) = item.implementing_ty(db).and_then(|it| it.as_adt()) else { return false };
    let option = famous.core_option_Option().map(hir::Adt::Enum);
    let result = famous.core_result_Result().map(hir::Adt::Enum);
    if Some(adt) != option && Some(adt) != result {
        return false;
    }
    match receiver {
        ast::Expr::MethodCallExpr(previous) => {
            previous.name_ref().is_none_or(|it| it.text() != "as_ref")
        }
        _ => true,
    }
}

/// A binding by value, of nothing more: rustc's `BindingMode::NONE` with no subpattern.
fn plain(pat: ast::IdentPat<'_>) -> bool {
    pat.ref_token().is_none() && pat.mut_token().is_none() && pat.pat().is_none()
}

/// clippy's `ident_eq`: `expr` is a path of one segment, the binding's name.
fn names(expr: ast::Expr<'_>, binding: ast::IdentPat<'_>) -> bool {
    let ast::Expr::PathExpr(path) = expr else { return false };
    let Some(path) = path.path() else { return false };
    let (Some(segment), Some(name)) = (path.segment(), binding.name()) else { return false };
    path.qualifier().is_none()
        && segment.generic_arg_list().is_none()
        && segment.name_ref().is_some_and(|it| it.text() == name.text())
}

fn lint_explicit_closure<'t>(
    cx: &mut Cx<'_, 't>,
    expr: ast::Expr<'t>,
    range: TextRange,
    receiver: TextRange,
    is_copy: bool,
) {
    let (message, method) = if is_copy && cx.conf().meets(msrvs::ITERATOR_COPIED) {
        ("you are using an explicit closure for copying elements", "copied")
    } else {
        ("you are using an explicit closure for cloning elements", "cloned")
    };
    let span = cx.span(range);
    let code = format!("{}.{method}()", cx.snippet(receiver));
    cx.lint(LINT, expr.syntax(), span.clone(), message).suggestion(
        span,
        format!("consider calling the dedicated `{method}` method"),
        code,
        Applicability::MachineApplicable,
    );
}
