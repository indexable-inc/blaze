//! E0614 "type `u32` cannot be dereferenced": a `*` on a type that is neither a pointer nor
//! implements `Deref`, with the parentheses after a block rustc's parser ended a statement at.
//!
//! Follows rustc's `compiler/rustc_hir_typeck/src/expr.rs` (`check_expr_unop`) and
//! `diagnostics.rs` (`CantDereference`) (MIT or Apache-2.0).

use hir::LangItem;

use super::binary_operator_cannot_be_applied::{code_fact, parentheses_needed};
use crate::{
    cx::Cx,
    lower::{Found, Phase},
    model::Diag,
};

pub(crate) fn lower<'db>(cx: &Cx<'_, 'db>, d: &hir::CannotBeDereferenced<'db>) -> Vec<Found> {
    let span = cx.span_of_ptr(d.expr);
    // clone: the type as rustc prints it, for an error hir found
    let ty = d.at_error.clone().unwrap_or_else(|| cx.ty(&d.found));
    let message = format!("type `{ty}` cannot be dereferenced"); // alloc: the error's message
    let mut diag = Diag::error(Some("E0614"), message, span.clone()); // clone: the error's span
    diag.span_label(span, "can't be dereferenced");
    parentheses_needed(cx, d.expr, &mut diag);
    if let Some(deref) = hir::Trait::lang(cx.db, cx.krate, LangItem::Deref) {
        let fact = format!("whether `{ty}` implements `Deref`"); // alloc: the error's fact
        diag.fact_opt(cx.implements_fact(deref, &d.found), fact);
    }
    code_fact(cx, d.expr.map(|it| it.syntax_node_ptr()), &mut diag);
    vec![Found::new(diag, Phase::Bodies)] // alloc: the error found
}
