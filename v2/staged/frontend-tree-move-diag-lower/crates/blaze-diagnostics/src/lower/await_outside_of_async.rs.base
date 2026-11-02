//! E0728: `.await` outside of an `async` function or block.
//!
//! Follows rustc's `compiler/rustc_ast_lowering/src/expr.rs` (`lower_expr_await`) and
//! `diagnostics.rs` (`AwaitOnlyInAsyncFnAndBlocks`), with the scope it names as
//! `LoweringContext::with_new_scopes` sets `current_item` (MIT or Apache-2.0).

use hir::InFile;
use syntax::{AstNode, SyntaxNode, TextRange, ast};

use crate::{
    cx::{Cx, item_range, signature_range},
    lints::code_fact,
    lower::{
        Found, Phase,
        break_outside_of_loop::{edges, is_item},
    },
    model::Diag,
};

pub(crate) fn lower(cx: &Cx<'_, '_>, d: &hir::AwaitOutsideOfAsync) -> Vec<Found> {
    let node = cx.node(d.node);
    let keyword = match node.value.await_token() {
        Some(it) => it.text_range(),
        None => node.value.syntax().text_range(),
    };
    let span = cx.span(node.with_value(keyword));
    let mut diag =
        Diag::error(Some("E0728"), "`await` is only allowed inside `async` functions and blocks", span.clone());
    diag.span_label(span, "only allowed inside `async` functions and blocks");
    if let Some(scope) = scope(cx, &node.map(|it| it.syntax().clone())) {
        diag.span_label(cx.span(scope), "this is not `async`");
    }
    code_fact(cx, &mut diag, d.node.map(|it| it.syntax_node_ptr()));
    vec![Found::new(diag, Phase::Resolve)]
}

/// The span of the scope rustc's AST lowering is in at `node` (`current_item`): a function's
/// signature, a closure's head (`move |x| -> T`), a `gen` block, an anonymous const's
/// expression; none in a `const` or `static` item's own body.
fn scope(cx: &Cx<'_, '_>, node: &InFile<SyntaxNode>) -> Option<InFile<TextRange>> {
    for (ancestor, child) in edges(cx, node) {
        let child = &child.value;
        let value = &ancestor.value;
        if let Some(it) = ast::Fn::cast(value.clone()) {
            return Some(ancestor.with_value(signature_range(&it)));
        }
        if let Some(it) = ast::ClosureExpr::cast(value.clone())
            && it.body().is_some_and(|body| body.syntax() == child)
        {
            let end = match it.ret_type() {
                Some(ret) => ret.syntax().text_range().end(),
                None => it.param_list()?.syntax().text_range().end(),
            };
            return Some(ancestor.with_value(TextRange::new(item_range(it.syntax()).start(), end)));
        }
        if let Some(it) = ast::BlockExpr::cast(value.clone()) {
            if it.const_token().is_some() {
                return Some(ancestor.with_value(it.stmt_list()?.syntax().text_range()));
            }
            if it.gen_token().is_some() {
                return Some(ancestor.with_value(item_range(it.syntax())));
            }
        }
        if let Some(it) = ast::ArrayExpr::cast(value.clone())
            && it.semicolon_token().is_some()
            && it.exprs().nth(1).is_some_and(|len| len.syntax() == child)
        {
            return Some(ancestor.with_value(child.text_range()));
        }
        if ast::ConstArg::can_cast(value.kind()) {
            return Some(ancestor.with_value(value.text_range()));
        }
        if is_item(value.kind()) {
            return None;
        }
    }
    None
}
