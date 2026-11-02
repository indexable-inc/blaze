//! E0719 of a trait object that gives an associated type two values: "the value of the
//! associated type `X` in `Tr` is already specified".
//!
//! Follows rustc's `compiler/rustc_hir_analysis/src/hir_ty_lowering/bounds.rs`
//! (`lower_assoc_item_constraint`'s `dup_constraints`, `ValueOfAssociatedStructAlreadySpecified`)
//! and `dyn_trait.rs` (`lower_trait_object_ty`) (MIT or Apache-2.0).

use syntax::view::AstNode as _;

use crate::{
    cx::Cx,
    lower::{Found, Phase},
    model::Diag,
};

pub(crate) fn lower(cx: &Cx<'_, '_>, d: &hir::ConflictingAssocTypeBindings) -> Vec<Found> {
    let span = cx.span(cx.node_view(d.bound).map(|it| it.syntax().text_range()));
    let name = d.assoc.name(cx.db).expect("an associated type has a name");
    let trait_ = d.assoc.container_trait(cx.db).expect("a trait object's associated type is its trait's");
    let mut diag = Diag::error(
        Some("E0719"),
        // alloc: the error's text, for an error hir found
        format!(
            "the value of the associated type `{}` in trait `{}` is already specified",
            name.display(cx.db, cx.edition),
            trait_.name(cx.db).display(cx.db, cx.edition)
        ),
        // clone: a handle, on the error path
        span.clone(),
    );
    diag.span_label(span, "re-bound here");
    // alloc: one error, for an error hir found
    vec![Found::new(diag, Phase::Items)]
}
