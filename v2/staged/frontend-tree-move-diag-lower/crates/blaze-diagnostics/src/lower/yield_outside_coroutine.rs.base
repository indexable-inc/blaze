//! E0627: `yield` outside of a coroutine, after the error rustc's AST lowering gives the first
//! `yield` of a body that is not a coroutine's.
//!
//! Follows rustc's `compiler/rustc_ast_lowering/src/expr.rs` (`lower_expr_yield`: the body's
//! `coroutine_kind`, set by its first `yield`, and `current_item`), `item.rs` (`lower_body`)
//! and `diagnostics.rs` (`YieldInClosure`), `compiler/rustc_hir_typeck/src/expr.rs`
//! (`check_expr_yield`) and `diagnostics.rs` (`YieldExprOutsideOfCoroutine`) (MIT or
//! Apache-2.0).

use hir::InFile;
use syntax::{AstNode, SyntaxNode, TextRange, TextSize, WalkEvent, ast};

use crate::{
    cx::{Cx, signature_range},
    lints::code_fact,
    lower::{
        Found, Phase,
        break_outside_of_loop::{edges, is_item},
    },
    model::{Applicability, Diag},
};

pub(crate) fn lower(cx: &Cx<'_, '_>, d: &hir::YieldOutsideCoroutine) -> Vec<Found> {
    let at = d.expr.map(|it| it.syntax_node_ptr());
    let span = cx.span_of(at);
    let node = cx.syntax(at);
    let mut found = Vec::new();
    if let Some((body, item)) = lowered_body(cx, &node)
        && first_yield(&body, &node)
    {
        let mut diag =
            Diag::error(None, "`yield` can only be used in `#[coroutine]` closures, or `gen` blocks", span.clone());
        if let Some(item) = item {
            diag.suggestion_verbose(
                cx.span(item.map(TextRange::empty)),
                "use `#[coroutine]` to make this closure a coroutine",
                "#[coroutine] ",
                Applicability::MaybeIncorrect,
            );
        }
        code_fact(cx, &mut diag, at);
        found.push(Found::new(diag, Phase::Resolve));
    }
    let mut diag = Diag::error(Some("E0627"), "yield expression outside of coroutine literal", span);
    code_fact(cx, &mut diag, at);
    found.push(Found::new(diag, Phase::Bodies));
    found
}

/// The node of the body rustc's `lower_body` lowers `node` in, and where `current_item` starts
/// there, if it is set: a function's signature, an anonymous or inline const's expression; not
/// in a `const` or `static` item's own body.
fn lowered_body(cx: &Cx<'_, '_>, node: &InFile<SyntaxNode>) -> Option<(InFile<SyntaxNode>, Option<InFile<TextSize>>)> {
    for (ancestor, child) in edges(cx, node) {
        if !is_body(&ancestor.value, &child.value) {
            continue;
        }
        let value = &ancestor.value;
        let item = if let Some(it) = ast::Fn::cast(value.clone()) {
            Some(ancestor.with_value(signature_range(&it).start()))
        } else if let Some(it) = ast::BlockExpr::cast(value.clone())
            && it.const_token().is_some()
        {
            it.stmt_list().map(|list| ancestor.with_value(list.syntax().text_range().start()))
        } else if ast::ArrayExpr::can_cast(value.kind()) {
            Some(child.with_value(child.value.text_range().start()))
        } else if ast::ConstArg::can_cast(value.kind()) {
            Some(ancestor.with_value(value.text_range().start()))
        } else {
            None
        };
        return Some((ancestor, item));
    }
    None
}

/// Whether `node` starts a body of its own that `child` is in: rustc lowers each with its own
/// `coroutine_kind`.
fn is_body(node: &SyntaxNode, child: &SyntaxNode) -> bool {
    if let Some(it) = ast::ClosureExpr::cast(node.clone()) {
        return it.body().is_some_and(|body| body.syntax() == child);
    }
    if let Some(it) = ast::BlockExpr::cast(node.clone()) {
        return it.async_token().is_some() || it.gen_token().is_some() || it.const_token().is_some();
    }
    if let Some(it) = ast::ArrayExpr::cast(node.clone()) {
        return it.semicolon_token().is_some() && it.exprs().nth(1).is_some_and(|len| len.syntax() == child);
    }
    ast::ConstArg::can_cast(node.kind()) || is_item(node.kind())
}

/// Whether `node` is the first `yield` of `body` as rustc lowers it: none of the body before
/// it, outside the bodies within it. A `yield` from a macro's expansion is taken to be the
/// first. One pre-order walk of `body` up to its first `yield`, the bodies within it skipped.
fn first_yield(body: &InFile<SyntaxNode>, node: &InFile<SyntaxNode>) -> bool {
    if body.file_id != node.file_id {
        return true;
    }
    let mut walk = body.value.preorder();
    while let Some(event) = walk.next() {
        let WalkEvent::Enter(it) = event else { continue };
        if it == body.value {
            continue;
        }
        let Some(parent) = it.parent() else { continue };
        if parent != body.value && is_body(&parent, &it) {
            walk.skip_subtree();
            continue;
        }
        if ast::YieldExpr::can_cast(it.kind()) {
            return it == node.value;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lower::break_outside_of_loop::tests::with_file;

    /// [`first_yield`] by the plain rule: the first `yield` of the body whose walk up to the
    /// body meets no body of its own.
    fn plain(body: &InFile<SyntaxNode>, node: &InFile<SyntaxNode>) -> bool {
        let first = body.value.descendants().filter_map(ast::YieldExpr::cast).find(|it| {
            let mut child = it.syntax().clone();
            for ancestor in it.syntax().ancestors().skip(1) {
                if ancestor == body.value {
                    return true;
                }
                if is_body(&ancestor, &child) {
                    return false;
                }
                child = ancestor;
            }
            false
        });
        first.is_some_and(|it| it.syntax() == &node.value)
    }

    /// The walk that stops at the first `yield` agrees with the plain rule for each `yield`,
    /// in bodies with closures, `async` and `const` blocks and items within them.
    #[test]
    fn the_first_yield_is_that_of_the_plain_rule() {
        let source = r#"
fn f() {
    let _ = || { yield 1; };
    async { yield 2; };
    yield 3;
    let _ = [0; { yield 4; 1 }];
    yield yield 5;
    const { yield 6; };
    fn g() { yield 7; }
    let _ = || yield 8;
}
fn main() {}
"#;
        with_file(source, |cx, root| {
            let mut compared = 0;
            let mut first = 0;
            for it in root.value.descendants().filter(|it| ast::YieldExpr::can_cast(it.kind())) {
                let node = root.with_value(it);
                let Some((body, _)) = lowered_body(cx, &node) else { continue };
                let fast = first_yield(&body, &node);
                assert_eq!(fast, plain(&body, &node), "at {:?}", node.value);
                compared += 1;
                first += usize::from(fast);
            }
            assert!(compared >= 8, "each `yield` has a body: {compared}");
            assert!(first >= 5 && first < compared, "some `yield`s are first and some are not: {first}");
        });
    }
}
