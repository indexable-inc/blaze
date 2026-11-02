//! `wrap_binder!()` into, or `unwrap_binder!()` of, a type that is not an unsafe binder: an
//! error with no code, at the written type when there is one.
//!
//! Follows rustc's `compiler/rustc_hir_typeck/src/expr.rs` (`check_expr_unsafe_binder_cast`)
//! (MIT or Apache-2.0).

use syntax::view::{self as ast, AstNode as _};

use crate::{
    cx::Cx,
    lints::code_fact,
    lower::{Found, Phase},
    model::Diag,
    print,
};

pub(crate) fn lower(cx: &Cx<'_, '_>, d: &hir::UnsafeBinderCastOfNonBinder<'_>) -> Vec<Found> {
    let at = d.expr.map(|it| it.syntax_node_ptr());
    let node = cx.syntax_view(at);
    let cast = ast::UnsafeBinderCastExpr::cast(node.value);
    let written = cast.and_then(|it| it.ty()).map(|it| it.syntax().text_range());
    let found = print::sort_string(cx, &d.found);
    let (range, message, note) = match d.kind {
        hir::UnsafeBinderCastKind::Wrap => (
            written.unwrap_or(node.value.text_range()),
            // alloc: the error's text, for an error hir found
            format!("`wrap_binder!()` can only wrap into unsafe binder, not {found}"),
            "unsafe binders are the only valid output of wrap",
        ),
        hir::UnsafeBinderCastKind::Unwrap => (
            written
                .or_else(|| cast.and_then(|it| it.expr()).map(|it| it.syntax().text_range()))
                .unwrap_or(node.value.text_range()),
            // alloc: the error's text, for an error hir found
            format!("expected unsafe binder, found {found} as input of `unwrap_binder!()`"),
            "only an unsafe binder type can be unwrapped",
        ),
    };
    let mut diag = Diag::error(None, message, cx.span(node.with_value(range)));
    diag.note(note);
    code_fact(cx, &mut diag, at);
    // alloc: the error's parts, for an error hir found
    vec![Found::new(diag, Phase::Bodies)]
}
