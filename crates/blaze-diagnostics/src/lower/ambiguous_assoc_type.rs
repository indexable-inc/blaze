//! E0223 "ambiguous associated type": an associated type of a type no bound gives it of
//! (`<u8>::Name`, `T::A::B`, `Trait::A::B`, an inherent one).
//!
//! Follows rustc's `compiler/rustc_hir_analysis/src/hir_ty_lowering/errors.rs`
//! (`report_ambiguous_assoc_item`) (MIT or Apache-2.0).

use hir::InFile;
use syntax::view::AstNode as _;

use crate::{
    cx::Cx,
    lower::{Found, Phase},
    model::{Diag, Span},
};

pub(crate) fn lower(cx: &Cx<'_, '_>, d: &hir::AmbiguousAssocType) -> Vec<Found> {
    let segment = cx.node_view(d.segment);
    let range = match segment.value.syntax().parent() {
        Some(path) => path.text_range(),
        None => segment.value.syntax().text_range(),
    };
    let span = cx.span(InFile::new(segment.file_id, range));
    // alloc: one error, for an error hir found
    vec![Found::new(diag(span), Phase::Items)]
}

/// rustc's E0223 at `span`.
pub(crate) fn diag(span: Span) -> Diag {
    Diag::error(Some("E0223"), "ambiguous associated type", span)
}
