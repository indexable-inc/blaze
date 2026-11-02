//! E0030 and E0579: a range pattern that matches no value (`5..=1`, `5..5`, `..i32::MIN`).
//!
//! Follows rustc's `compiler/rustc_mir_build/src/thir/pattern/mod.rs` (`lower_pattern_range`)
//! and `compiler/rustc_mir_build/src/diagnostics.rs`
//! (`LowerRangeBoundMustBeLessThanOrEqualToUpper`, `LowerRangeBoundMustBeLessThanUpper`,
//! `UpperRangeBoundCannotBeMin`) (MIT or Apache-2.0). E0030's note is `-Zteach`'s only.

use syntax::view::{self as ast, RangeItem as _};

use crate::{
    cx::Cx,
    lints::code_fact,
    lower::{Found, Phase},
    model::Diag,
};

pub(crate) fn lower(cx: &Cx<'_, '_>, d: &hir::EmptyRangePattern) -> Vec<Found> {
    let span = cx.span_of_ptr(d.pat);
    let mut diag = match d.inclusive {
        true => {
            let message = "lower bound for range pattern must be less than or equal to upper bound";
            let mut diag = Diag::error(Some("E0030"), message, span.clone()); // clone: the error's span, the edge
            diag.span_label(span, "lower bound larger than upper bound");
            diag
        }
        false if has_no_start(cx, d) => {
            Diag::error(Some("E0579"), "exclusive upper bound for a range bound cannot be the minimum", span)
        }
        false => Diag::error(Some("E0579"), "lower bound for range pattern must be less than upper bound", span),
    };
    code_fact(cx, &mut diag, d.pat.map(|it| it.syntax_node_ptr()));
    vec![Found::new(diag, Phase::Mir)] // alloc: the error found, the edge
}

/// rustc's `lo_expr.is_none()`: the range is `..hi`.
fn has_no_start(cx: &Cx<'_, '_>, d: &hir::EmptyRangePattern) -> bool {
    match cx.node_view(d.pat).value {
        ast::Pat::RangePat(range) => range.start().is_none(),
        _ => false,
    }
}
