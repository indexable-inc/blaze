//! E0730: a slice pattern with `..` matched against an array whose length is not known, as
//! `[a, b, ..]` against `[T; N]` with `N` a const parameter.
//!
//! Follows rustc's `compiler/rustc_hir_typeck/src/pat.rs` (`check_array_pat_len`,
//! `error_scrutinee_unfixed_length`) (MIT or Apache-2.0).

use crate::{
    cx::Cx,
    lints::code_fact,
    lower::{Found, Phase},
    model::Diag,
};

pub(crate) fn lower(cx: &Cx<'_, '_>, d: &hir::ArrayPatternWithoutFixedLength) -> Vec<Found> {
    let span = cx.span_of_ptr(d.pat);
    let mut diag =
        Diag::error(Some("E0730"), "cannot pattern-match on an array without a fixed length", span);
    code_fact(cx, &mut diag, d.pat.map(|it| it.syntax_node_ptr()));
    vec![Found::new(diag, Phase::Bodies)] // alloc: the error found
}
