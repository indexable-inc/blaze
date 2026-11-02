//! E0562 "`impl Trait` is not allowed in {position}", the position read from where the type
//! is written, with the note rustc gives.
//!
//! Follows rustc's `compiler/rustc_ast_lowering/src/lib.rs` (`ImplTraitPosition`,
//! `lower_ty_direct`'s `MisplacedImplTrait`) (MIT or Apache-2.0).

use syntax::{SyntaxKind, view::AstNode as _};

use crate::{
    cx::Cx,
    lower::{Found, Phase},
    model::Diag,
};

pub(crate) fn lower(cx: &Cx<'_, '_>, d: &hir::ImplTraitNotAllowed) -> Vec<Found> {
    let ty = cx.node_view(d.ty);
    let span = cx.span(ty.map(|it| it.syntax().text_range()));
    let position = ty
        .value
        .syntax()
        .ancestors()
        .skip(1)
        .find_map(|it| match it.kind() {
            SyntaxKind::RECORD_FIELD | SyntaxKind::TUPLE_FIELD => Some("field types"),
            SyntaxKind::LET_STMT => Some("the type of variable bindings"),
            SyntaxKind::CONST => Some("const types"),
            SyntaxKind::STATIC => Some("static types"),
            SyntaxKind::TYPE_ALIAS => Some("type aliases"),
            SyntaxKind::FN_PTR_TYPE => Some("`fn` pointer params"),
            SyntaxKind::TYPE_BOUND | SyntaxKind::WHERE_PRED => Some("bounds"),
            SyntaxKind::TYPE_PARAM | SyntaxKind::CONST_PARAM => Some("generic parameter defaults"),
            SyntaxKind::IMPL => Some("impl headers"),
            SyntaxKind::CAST_EXPR => Some("cast expression types"),
            SyntaxKind::GENERIC_ARG_LIST => Some("paths"),
            _ => None,
        })
        .unwrap_or("paths");
    let mut diag = Diag::error(
        Some("E0562"),
        format!("`impl Trait` is not allowed in {position}"), // alloc: the error's text
        span,
    );
    diag.note("`impl Trait` is only allowed in arguments and return types of functions and methods");
    // alloc: one error, for an error hir found
    vec![Found::new(diag, Phase::Lower)]
}
