//! `get_first`: `x.get(0)` of a slice or a `VecDeque`, which `x.first()` or `x.front()` says.
//!
//! Follows clippy's `clippy_lints/src/methods/get_first.rs` at rust-lang/rust
//! 12433c307b5a497388ffe2f4e49dbf1ab8c71fa0 (MIT or Apache-2.0).

use hir::AsAssocItem as _;
use syntax::view as ast;

use ast::AstNode as _;

use super::utils;
use crate::{check::Cx, diag::Applicability};

const LINT: super::Implemented = super::Implemented::GetFirst;

pub(super) fn check<'t>(cx: &mut Cx<'_, 't>, expr: ast::Expr<'t>) {
    if !cx.on(LINT) || utils::method_name(expr) != Some("get") {
        return;
    }
    let ast::Expr::MethodCallExpr(method) = expr else { return };
    let Some(call) = utils::method_call(cx, expr) else { return };
    let [ast::Expr::Literal(index)] = call.args() else { return };
    let ast::LiteralKind::IntNumber(number) = index.kind() else { return };
    if number.value().ok() != Some(0) {
        return;
    }
    let db = cx.db();
    let Some(function) = cx.method(method) else { return };
    let Some(implementing) = function.as_assoc_item(db).and_then(|it| it.implementing_ty(db))
    else {
        return;
    };
    let replacement = if implementing.is_slice() {
        "first"
    } else if implementing.as_adt().is_some_and(|adt| {
        matches!(cx.std_def(&["alloc", "collections", "VecDeque"]), Some(hir::ModuleDef::Adt(it)) if it == adt)
    }) {
        "front"
    } else {
        return;
    };
    let (Some(range), Some(receiver)) = (cx.range(expr.syntax()), cx.range(call.receiver.syntax()))
    else {
        return;
    };
    let receiver = cx.snippet(receiver);
    let span = cx.span(range);
    cx.lint(
        LINT,
        expr.syntax(),
        span.clone(),
        format!("accessing first element with `{receiver}.get(0)`"),
    )
    .suggestion(
        span,
        "try",
        format!("{receiver}.{replacement}()"),
        Applicability::MachineApplicable,
    );
}
