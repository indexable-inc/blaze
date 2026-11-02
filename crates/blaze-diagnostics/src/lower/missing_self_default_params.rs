//! E0393 "the type parameter `Rhs` must be explicitly specified": a trait object that leaves
//! out a type parameter whose default names `Self`.
//!
//! Follows rustc's `compiler/rustc_hir_analysis/src/hir_ty_lowering/errors.rs`
//! (`report_missing_type_params`) (MIT or Apache-2.0).

use syntax::view::AstNode as _;

use crate::{
    cx::Cx,
    lower::{Found, Phase},
    model::Diag,
};

pub(crate) fn lower(cx: &Cx<'_, '_>, d: &hir::MissingSelfDefaultParams) -> Vec<Found> {
    let span = cx.span(cx.node_view(d.bound).map(|it| it.syntax().text_range()));
    // alloc: the error's text, for an error hir found
    let names: Vec<String> = d
        .params
        .iter()
        .map(|it| format!("`{}`", it.name(cx.db).display(cx.db, cx.edition)))
        .collect();
    let (noun, list) = match names.as_slice() {
        // clone: the error's text
        [one] => ("type parameter", one.clone()),
        _ => ("type parameters", names.join(", ")),
    };
    // clone: a handle, on the error path
    let mut diag =
        Diag::error(Some("E0393"), format!("the {noun} {list} must be explicitly specified"), span.clone());
    // alloc: the error's text, for an error hir found
    diag.span_label(span, format!("missing reference{} to {list}", if names.len() == 1 { "" } else { "s" }));
    diag.note("because the parameter default references `Self`, the parameter must be specified on the object type");
    // alloc: one error, for an error hir found
    vec![Found::new(diag, Phase::Items)]
}
