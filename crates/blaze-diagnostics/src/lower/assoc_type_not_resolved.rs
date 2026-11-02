//! E0220 "associated type `Name` not found for `T`", E0221 "ambiguous associated type `Name`
//! in bounds of `T`" and E0391 of an associated type shorthand (`T::Name`, `Self::Name`,
//! `Trait::Name`).
//!
//! Follows rustc's `compiler/rustc_hir_analysis/src/hir_ty_lowering/errors.rs`
//! (`complain_about_assoc_item_not_found`) and `hir_ty_lowering/mod.rs`
//! (`probe_single_bound_for_assoc_item`) (MIT or Apache-2.0).

use hir::{AssocTypeFailure, InFile};
use syntax::view::AstNode as _;

use crate::{
    cx::Cx,
    lower::{Found, Phase},
    model::Diag,
};

pub(crate) fn lower(cx: &Cx<'_, '_>, d: &hir::AssocTypeNotResolved) -> Vec<Found> {
    let segment = cx.node_view(d.segment);
    let range = match segment.value.name_ref() {
        Some(name) => name.syntax().text_range(),
        None => segment.value.syntax().text_range(),
    };
    let span = cx.span(InFile::new(segment.file_id, range));
    let name = match segment.value.name_ref() {
        Some(name) => name.text().to_string(), // alloc: the error's text
        None => String::new(),
    };
    let qself = match d.qself {
        // alloc: the error's text, for an error hir found
        either::Either::Left(param) => param.name(cx.db).display(cx.db, cx.edition).to_string(),
        // alloc: the error's text, for an error hir found
        either::Either::Right(trait_) => trait_.name(cx.db).display(cx.db, cx.edition).to_string(),
    };
    let diag = match d.failure {
        AssocTypeFailure::NotFound => {
            let mut diag = Diag::error(
                Some("E0220"),
                // alloc: the error's text, for an error hir found
                format!("associated type `{name}` not found for `{qself}`"),
                // clone: a handle, on the error path
                span.clone(),
            );
            // alloc: the error's text, for an error hir found
            diag.span_label(span, format!("associated type `{name}` not found"));
            diag
        }
        AssocTypeFailure::Ambiguous => {
            let mut diag = Diag::error(
                Some("E0221"),
                // alloc: the error's text, for an error hir found
                format!("ambiguous associated type `{name}` in bounds of `{qself}`"),
                // clone: a handle, on the error path
                span.clone(),
            );
            // alloc: the error's text, for an error hir found
            diag.span_label(span, format!("ambiguous associated type `{name}`"));
            diag
        }
        AssocTypeFailure::Cycle => Diag::error(
            Some("E0391"),
            // alloc: the error's text, for an error hir found
            format!("cycle detected when computing the bounds for type parameter `{qself}`"),
            span,
        ),
    };
    // alloc: one error, for an error hir found
    vec![Found::new(diag, Phase::Items)]
}
