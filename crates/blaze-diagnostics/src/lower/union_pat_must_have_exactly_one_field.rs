//! "union patterns should have exactly one field": a union's pattern naming none of its
//! fields or more than one, as `U {}`, `U { a, b }` or `U { .. }`. rustc gives it no code.
//!
//! Follows rustc's `compiler/rustc_hir_typeck/src/pat.rs` (`check_struct_pat_fields`) and
//! `compiler/rustc_hir_typeck/src/diagnostics.rs` (`UnionPatMultipleFields`) (MIT or
//! Apache-2.0).

use crate::{
    cx::Cx,
    lints::code_fact,
    lower::{Found, Phase},
    model::Diag,
};

pub(crate) fn lower(cx: &Cx<'_, '_>, d: &hir::UnionPatMustHaveExactlyOneField) -> Vec<Found> {
    let span = cx.span_of_ptr(d.pat);
    let mut diag = Diag::error(None, "union patterns should have exactly one field", span);
    code_fact(cx, &mut diag, d.pat.map(|it| it.syntax_node_ptr()));
    // alloc: the error's parts, for an error hir found
    vec![Found::new(diag, Phase::Bodies)]
}
