//! hir's `PatternArgInExternFn`: a parameter bound by a pattern in a function of an `extern`
//! block, or of a trait method without a body. Both are rustc's AST validation's (E0130 and
//! E0642), which `checks::ast_validation` gives as its walk comes to the function, so nothing
//! is lowered here.

use crate::{cx::Cx, lower::Found};

pub(crate) fn lower(_cx: &Cx<'_, '_>, _d: &hir::PatternArgInExternFn) -> Vec<Found> {
    // alloc: empty, allocates nothing
    Vec::new()
}
