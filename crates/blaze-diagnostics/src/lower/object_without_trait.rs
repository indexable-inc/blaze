//! E0224 "at least one trait is required for an object type" of a `dyn` whose bounds are all
//! relaxed (`dyn ?Sized`), at the whole object type (the bound itself for a bare one of Rust
//! 2015); one with no trait written is the parser's (`parse.rs`).
//!
//! Follows rustc's `compiler/rustc_hir_analysis/src/hir_ty_lowering/dyn_trait.rs`
//! (`lower_trait_object_ty`: `TraitObjectDeclaredWithNoTraits`) (MIT or Apache-2.0).

use syntax::{SyntaxKind, view::AstNode as _};

use crate::{
    cx::Cx,
    lower::{Found, Phase},
    model::Diag,
};

pub(crate) fn lower(cx: &Cx<'_, '_>, d: &hir::ObjectWithoutTrait) -> Vec<Found> {
    let bound = cx.node_view(d.bound);
    let object = bound
        .value
        .syntax()
        .ancestors()
        .find(|it| it.kind() == SyntaxKind::DYN_TRAIT_TYPE)
        .unwrap_or(bound.value.syntax());
    let span = cx.span(bound.with_value(object.text_range()));
    let diag = Diag::error(Some("E0224"), "at least one trait is required for an object type", span);
    // alloc: one error, for an error hir found
    vec![Found::new(diag, Phase::Items)]
}
