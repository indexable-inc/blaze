//! hir's `MissingBody`: a free `const`, `static` or type alias, or an associated `const` or
//! type of an impl, without a body. Each is rustc's AST validation's (`ConstWithoutBody`,
//! `StaticWithoutBody`, `TyAliasWithoutBody`, `AssocConstWithoutBody`,
//! `AssocTypeWithoutBody`), which `checks::ast_validation` gives as its walk comes to the
//! item, so nothing is lowered here.

use crate::{cx::Cx, lower::Found};

pub(crate) fn lower(_cx: &Cx<'_, '_>, _d: &hir::MissingBody) -> Vec<Found> {
    Vec::new() // alloc: empty, allocates nothing
}
