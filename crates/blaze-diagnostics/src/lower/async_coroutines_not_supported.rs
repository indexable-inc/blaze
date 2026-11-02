//! E0727: a `yield` in an `async` block, closure or function, "`async` coroutines are not yet
//! supported", which rustc's AST lowering gives as it lowers the `yield` to `{ expr; <error> }`.
//!
//! Follows rustc's `compiler/rustc_ast_lowering/src/expr.rs` (`lower_expr_yield`) and
//! `compiler/rustc_ast_lowering/src/errors.rs` (`AsyncCoroutinesNotSupported`) (MIT or
//! Apache-2.0).

use crate::{
    cx::Cx,
    lints::code_fact,
    lower::{Found, Phase},
    model::Diag,
};

pub(crate) fn lower(cx: &Cx<'_, '_>, d: &hir::AsyncCoroutinesNotSupported) -> Vec<Found> {
    let at = d.expr.map(|it| it.syntax_node_ptr());
    let mut diag = Diag::error(Some("E0727"), "`async` coroutines are not yet supported", cx.span_of(at));
    code_fact(cx, &mut diag, at);
    // alloc: the error's parts, for an error hir found
    vec![Found::new(diag, Phase::Lower)]
}
