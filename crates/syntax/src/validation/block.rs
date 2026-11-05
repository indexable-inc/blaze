//! Logic for validating block expressions i.e. `ast::BlockExpr`.

use crate::{
    SyntaxError,
    view::{self as ast, AstNode as _, HasAttrs as _},
};

/// rustc parses inner attributes at the start of every block expression and refuses them in
/// its parser only in the branches of an `if` ("an inner attribute is not permitted in this
/// context"). Elsewhere (a `let` initializer, a closure body, a block a macro expands to) they
/// are attributes on an expression, which `#![feature(stmt_expr_attributes)]` allows; that gate
/// needs the crate's features, so it is not a syntax error.
pub(crate) fn validate_block_expr(block: ast::BlockExpr<'_>, errors: &mut Vec<SyntaxError>) {
    let Some(parent) = block.syntax().parent().and_then(ast::IfExpr::cast) else { return };
    let branch = parent.then_branch() == Some(block)
        || matches!(parent.else_branch(), Some(ast::ElseBranch::Block(it)) if it == block);
    if !branch {
        return;
    }
    if let Some(stmt_list) = block.stmt_list() {
        errors.extend(stmt_list.attrs().filter(|attr| attr.kind().is_inner()).map(|attr| {
            SyntaxError::new(
                "an inner attribute is not permitted in this context",
                attr.syntax().text_range(),
            )
        }));
    }
}
