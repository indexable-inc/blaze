//! "return type notation not allowed in this position yet": `T::method(..)` where return type
//! notation is not taken. On a segment before the last, or on the last segment of a value path,
//! AST lowering refuses the `(..)`; on the last segment of a type, type lowering refuses the
//! whole type.
//!
//! Follows rustc's `compiler/rustc_ast_lowering/src/path.rs` (`lower_qpath`,
//! `lower_path_segment`, `GenericArgs::ParenthesizedElided`),
//! `compiler/rustc_ast_lowering/src/diagnostics.rs` (`BadReturnTypeNotation::Position`),
//! `compiler/rustc_hir_analysis/src/hir_ty_lowering/mod.rs` (`lower_ty`, `TyKind::Path`) and
//! `compiler/rustc_hir_analysis/src/diagnostics.rs` (`BadReturnTypeNotation`) (MIT or
//! Apache-2.0).

use hir::InFile;
use syntax::view::{self as ast, AstNode as _};

use crate::{
    cx::Cx,
    lower::{Found, Phase, incorrect_generics_len::site_fact},
    model::Diag,
};

pub(crate) fn lower(cx: &Cx<'_, '_>, d: &hir::BadRtn) -> Vec<Found> {
    let rtn = cx.node_view(d.rtn);
    let file = rtn.file_id;
    let ty = rtn
        .value
        .syntax()
        .parent()
        .and_then(ast::PathSegment::cast)
        .and_then(|segment| {
            let path = segment.parent_path();
            let top = path.top_path();
            (path == top).then(|| top.syntax().parent().and_then(ast::PathType::cast))?
        });
    let (range, phase) = match ty {
        Some(ty) => (ty.syntax().text_range(), Phase::Items),
        None => (rtn.value.syntax().text_range(), Phase::Resolve),
    };
    let mut diag = Diag::error(
        None,
        "return type notation not allowed in this position yet",
        cx.span(InFile::new(file, range)),
    );
    site_fact(cx, &mut diag, InFile::new(file, rtn.value.syntax()));
    vec![Found::new(diag, phase)] // alloc: the error found
}
