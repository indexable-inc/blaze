//! "reached pattern complexity limit": match checking gave up past
//! `#![pattern_complexity_limit]`.
//!
//! Follows rustc's `compiler/rustc_pattern_analysis/src/rustc.rs` (`complexity_exceeded`: at
//! the whole `match`, else at the scrutinee) (MIT or Apache-2.0).

use crate::{
    cx::Cx,
    lints::code_fact,
    lower::{Found, Phase},
    model::Diag,
};

pub(crate) fn lower(cx: &Cx<'_, '_>, d: &hir::PatternComplexityLimit) -> Vec<Found> {
    let mut diag = Diag::error(None, "reached pattern complexity limit", cx.span_of(d.at));
    code_fact(cx, &mut diag, d.at);
    vec![Found::new(diag, Phase::Mir)] // alloc: the error found, the edge
}
