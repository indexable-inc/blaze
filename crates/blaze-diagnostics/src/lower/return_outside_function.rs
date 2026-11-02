//! E0572: `return` or `become` in a constant's body, with the function around it named when
//! there is one.
//!
//! Follows rustc's `compiler/rustc_hir_typeck/src/expr.rs` (`emit_return_outside_of_fn_body`)
//! and `diagnostics.rs` (`ReturnStmtOutsideOfFnBody`) (MIT or Apache-2.0).

use syntax::{
    TextRange,
    view::{self as ast, AstNode as _},
};

use crate::{
    cx::{Cx, item_range_view as item_range},
    lints::code_fact,
    lower::{
        Found, Phase,
        break_outside_of_loop::{edges, is_item},
    },
    model::Diag,
};

pub(crate) fn lower(cx: &Cx<'_, '_>, d: &hir::ReturnOutsideFunction) -> Vec<Found> {
    let at = d.expr.map(|it| it.syntax_node_ptr());
    let kind = match d.kind {
        hir::ReturnKind::ReturnExpr => "return",
        hir::ReturnKind::BecomeExpr => "become",
    };
    let span = cx.span_of(at);
    // alloc: the error's text, for an error hir found
    let mut diag = Diag::error(Some("E0572"), format!("{kind} statement outside of function body"), span);
    let mut body = None;
    for (it, child) in edges(cx, cx.syntax_view(at)) {
        if body.is_none() {
            let is_body = ast::ConstArg::can_cast(it.value.kind())
                || ast::BlockExpr::cast(it.value).is_some_and(|it| it.const_token().is_some())
                || ast::ArrayExpr::cast(it.value).is_some_and(|array| {
                    array.semicolon_token().is_some()
                        && array.exprs().nth(1).is_some_and(|len| len.syntax() == child.value)
                });
            if is_body {
                let range: TextRange = match ast::BlockExpr::cast(it.value) {
                    Some(block) => block.stmt_list().map_or(it.value.text_range(), |it| it.syntax().text_range()),
                    None if ast::ConstArg::can_cast(it.value.kind()) => it.value.text_range(),
                    None => child.value.text_range(),
                };
                body = Some(it.with_value(range));
            }
        }
        if is_item(it.value.kind()) {
            if let (Some(body), Some(item)) = (body, ast::Fn::cast(it.value))
                && item.body().is_some()
            {
                // alloc: the error's text, for an error hir found
                diag.span_label(cx.span(body), format!("the {kind} is part of this body..."));
                diag.span_label(cx.span(it.with_value(item_range(item.syntax()))), "...not the enclosing function body");
            }
            break;
        }
    }
    code_fact(cx, &mut diag, at);
    // alloc: the error's parts, for an error hir found
    vec![Found::new(diag, Phase::Bodies)]
}
