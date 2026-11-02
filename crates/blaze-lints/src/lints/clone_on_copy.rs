//! `clone_on_copy`: `x.clone()` of a type that is `Copy`.
//!
//! Follows clippy's `clippy_lints/src/methods/clone_on_copy.rs` at rust-lang/rust
//! 12433c307b5a497388ffe2f4e49dbf1ab8c71fa0 (MIT or Apache-2.0), for the method call; not
//! ported: `Clone::clone(&x)` called as a function.

use hir::{Adjust, AsAssocItem as _, HirDisplay as _};
use syntax::view as ast;

use ast::AstNode as _;

use super::utils;
use crate::{check::Cx, diag::Applicability};

const LINT: super::Implemented = super::Implemented::CloneOnCopy;

/// clippy's `ParentIsSuffixExpr`: what the clone is used in.
#[derive(PartialEq, Eq)]
enum Parent {
    /// An operand that binds tighter than `*`: it needs the dereference in parentheses.
    Suffix,
    Other,
    /// A use that a copy would not serve as the clone does.
    Keep,
}

pub(super) fn check<'t>(cx: &mut Cx<'_, 't>, expr: ast::Expr<'t>) {
    if !cx.on(LINT) || utils::method_name(expr) != Some("clone") {
        return;
    }
    let ast::Expr::MethodCallExpr(method) = expr else { return };
    let Some(call) = utils::method_call(cx, expr) else { return };
    if !call.args().is_empty() {
        return;
    }
    let db = cx.db();
    let clone = cx.lang_trait(hir::LangItem::Clone);
    let Some(function) = cx.method(method) else { return };
    if clone.is_none()
        || function.as_assoc_item(db).and_then(|it| it.container_or_implemented_trait(db)) != clone
    {
        return;
    }
    let adjustments = cx.adjustments(call.receiver).unwrap_or_default();
    let Some(arg_ty) =
        adjustments.last().map(|it| it.target.clone()).or_else(|| cx.ty(call.receiver))
    else {
        return;
    };
    if arg_ty.as_reference().is_some_and(|(inner, _)| inner.is_reference()) {
        return;
    }
    let Some(ty) = cx.ty(expr) else { return };
    if !ty.is_copy(db) {
        return;
    }
    let parent = parent_of(cx, expr);
    if parent == Parent::Keep {
        return;
    }
    let (Some(range), Some(receiver)) = (cx.range(expr.syntax()), cx.range(call.receiver.syntax()))
    else {
        return;
    };
    let snippet = cx.snippet(receiver);
    let derefs = adjustments.iter().take_while(|it| matches!(it.kind, Adjust::Deref(_))).count();
    let stars = "*".repeat(derefs);
    let (help, sugg) = match (derefs, parent) {
        (0, _) => ("try removing the `clone` call", snippet.to_owned()),
        (_, Parent::Suffix) => ("try dereferencing it", format!("({stars}{snippet})")),
        _ => ("try dereferencing it", format!("{stars}{snippet}")),
    };
    let shown = ty.display(db, cx.krate().to_display_target(db)).to_string();
    let span = cx.span(range);
    cx.lint(
        LINT,
        expr.syntax(),
        span.clone(),
        format!("using `clone` on type `{shown}` which implements the `Copy` trait"),
    )
    .suggestion(span, help, sugg, Applicability::MachineApplicable);
}

/// clippy's `check_parent_is_suffix_expr`.
fn parent_of<'t>(cx: &Cx<'_, 't>, expr: ast::Expr<'t>) -> Parent {
    let mut used = expr.syntax();
    while let Some(parent) = used.parent()
        && ast::ParenExpr::can_cast(parent.kind())
    {
        used = parent;
    }
    let Some(parent) = used.parent() else { return Parent::Other };
    if let Some(stmt) = ast::LetStmt::cast(parent) {
        let by_ref =
            matches!(stmt.pat(), Some(ast::Pat::IdentPat(pat)) if pat.ref_token().is_some());
        return if by_ref { Parent::Keep } else { Parent::Other };
    }
    match ast::Expr::cast(parent) {
        Some(ast::Expr::RefExpr(_)) => Parent::Keep,
        Some(ast::Expr::MethodCallExpr(call))
            if call.receiver().is_some_and(|it| it.syntax() == used) =>
        {
            let changed = cx.ty(expr).zip(cx.adjusted_ty(expr)).is_some_and(|(a, b)| a != b);
            if changed { Parent::Keep } else { Parent::Suffix }
        }
        Some(
            ast::Expr::TryExpr(_)
            | ast::Expr::AwaitExpr(_)
            | ast::Expr::FieldExpr(_)
            | ast::Expr::IndexExpr(_),
        ) => Parent::Suffix,
        _ => Parent::Other,
    }
}
