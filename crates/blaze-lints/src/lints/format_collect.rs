//! `format_collect`: `iter.map(|x| format!(..)).collect::<String>()`, a `String` made for each
//! element only to be appended, where `fold` with `write!` makes none.
//!
//! Follows clippy's `clippy_lints/src/methods/format_collect.rs` (MIT or Apache-2.0), written
//! from what is known of its source rather than read from a checkout: no copy of clippy's
//! source was at hand. Its name, message, help, note and spans are clippy's.

use syntax::view::{self as ast, AstNode as _};

use super::{
    implicit_clone::{STRING, is_std_adt, of_trait},
    utils,
};
use crate::check::Cx;

const LINT: super::Implemented = super::Implemented::FormatCollect;

pub(super) fn check<'t>(cx: &mut Cx<'_, 't>, node: ast::Node<'t>) {
    if !cx.on(LINT) {
        return;
    }
    let Some(collect) = ast::MethodCallExpr::cast(node) else { return };
    let expr = ast::Expr::MethodCallExpr(collect);
    let Some(call) = utils::method_call(cx, expr) else { return };
    if call.name != "collect" || !call.args().is_empty() {
        return;
    }
    let Some(map) = utils::method_call(cx, call.receiver) else { return };
    let [ast::Expr::ClosureExpr(closure)] = map.args() else { return };
    if map.name != "map" {
        return;
    }
    let Some(value) = closure.body().and_then(peel_written_blocks) else { return };
    let ast::Expr::MacroExpr(value) = value else { return };
    let Some(format) = value.macro_call() else { return };
    if cx.std_macro(format).is_none_or(|it| it.as_str() != "format") {
        return;
    }
    let ast::Expr::MethodCallExpr(collect_method) = expr else { return };
    let Some(function) = cx.method(collect_method) else { return };
    if !of_trait(cx, function, &["core", "iter", "Iterator"])
        || cx.ty(expr).is_none_or(|it| !is_std_adt(cx, &it, STRING))
    {
        return;
    }
    let (Some(range), Some(map_name), Some(call_site)) = (
        cx.range(node),
        cx.range(map.name_ref.syntax()),
        cx.range(format.syntax()),
    ) else {
        return;
    };
    let span = cx.span(range);
    let (map_name, call_site) = (cx.span(map_name), cx.span(call_site));
    cx.lint(LINT, node, span, "use of `format` to build up a string from an iterator")
        .span_help(map_name, "call `fold` instead")
        .span_note(call_site, "... and use the `write!` macro here");
}

/// clippy's `peel_non_expn_blocks`: the closure's value with the blocks written around it taken
/// off, each to its tail, whatever statements come before.
fn peel_written_blocks(expr: ast::Expr<'_>) -> Option<ast::Expr<'_>> {
    let mut expr = expr;
    while let ast::Expr::BlockExpr(block) = expr {
        let plain = block.async_token().is_none()
            && block.const_token().is_none()
            && block.gen_token().is_none()
            && block.try_block_modifier().is_none();
        if !plain {
            break;
        }
        expr = block.tail_expr()?;
    }
    Some(expr)
}
