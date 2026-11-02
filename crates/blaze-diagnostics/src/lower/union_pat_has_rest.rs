//! "`..` cannot be used in union patterns": a union's pattern with a rest, as `U { a, .. }`.
//! rustc gives it no code.
//!
//! Follows rustc's `compiler/rustc_hir_typeck/src/pat.rs` (`check_struct_pat_fields`) and
//! `compiler/rustc_hir_typeck/src/diagnostics.rs` (`UnionPatDotDot`) (MIT or Apache-2.0).

use crate::{
    cx::Cx,
    lints::code_fact,
    lower::{Found, Phase},
    model::Diag,
};

pub(crate) fn lower(cx: &Cx<'_, '_>, d: &hir::UnionPatHasRest) -> Vec<Found> {
    let mut diag = Diag::error(None, "`..` cannot be used in union patterns", cx.span_of_ptr(d.pat));
    code_fact(cx, &mut diag, d.pat.map(|it| it.syntax_node_ptr()));
    // alloc: the error's parts, for an error hir found
    vec![Found::new(diag, Phase::Bodies)]
}
