//! `str_to_string`: `s.to_string()` where `s` is a `&str`, which `s.to_owned()` says.
//!
//! Follows clippy's `clippy_lints/src/strings.rs` (`StrToString`, MIT or Apache-2.0), written
//! from what is known of its source rather than read from a checkout: no copy of clippy's
//! source was at hand. Its name, message, span, suggestion and applicability are clippy's; as
//! clippy, it asks only the method's name and the receiver's type, not which `to_string` it is.

use syntax::view::{self as ast, AstNode as _};

use super::utils;
use crate::{check::Cx, diag::Applicability};

const LINT: super::Implemented = super::Implemented::StrToString;

pub(super) fn check<'t>(cx: &mut Cx<'_, 't>, node: ast::Node<'t>) {
    if !cx.on(LINT) {
        return;
    }
    let Some(method) = ast::MethodCallExpr::cast(node) else { return };
    let Some(call) = utils::method_call(cx, ast::Expr::MethodCallExpr(method)) else { return };
    if call.name != "to_string" || !call.args().is_empty() {
        return;
    }
    let Some(ty) = cx.ty(call.receiver) else { return };
    if !ty.as_reference().is_some_and(|(inner, _)| inner.is_str()) {
        return;
    }
    let Some(range) = cx.range(node) else { return };
    let sugg = format!("{}.to_owned()", cx.source_of(call.receiver.syntax()));
    let span = cx.span(range);
    cx.lint(LINT, node, span.clone(), "`to_string()` called on a `&str`").suggestion(
        span,
        "try",
        sugg,
        Applicability::MachineApplicable,
    );
}
