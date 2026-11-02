//! E0225 "only auto traits can be used as additional traits in a trait object".
//!
//! Follows rustc's `compiler/rustc_hir_analysis/src/hir_ty_lowering/dyn_trait.rs`
//! (`lower_trait_object_ty`) and `errors.rs` (`report_trait_object_addition_traits`) (MIT or
//! Apache-2.0).

use syntax::view::AstNode as _;

use crate::{
    cx::Cx,
    lower::{Found, Phase},
    model::Diag,
};

pub(crate) fn lower(cx: &Cx<'_, '_>, d: &hir::AdditionalNonAutoTraits) -> Vec<Found> {
    let span_of = |ty: hir::InFile<syntax::AstPtr<syntax::ast::Type>>| {
        cx.span(cx.node_view(ty).map(|it| it.syntax().text_range()))
    };
    let first = span_of(d.first);
    let &second = d.additional.first().expect("hir reports a second non-auto trait at least");
    let mut diag = Diag::error(
        Some("E0225"),
        "only auto traits can be used as additional traits in a trait object",
        span_of(second),
    );
    diag.span_label(first, "first non-auto trait");
    for &it in d.additional.iter() {
        diag.span_label(span_of(it), "additional non-auto trait");
    }
    diag.help("consider creating a new trait with all of these as supertraits and using that trait here instead: `trait NewTrait: A + B {}`");
    diag.note("auto-traits like `Send` and `Sync` are traits that have special properties; for more information on them, visit <https://doc.rust-lang.org/reference/special-types-and-traits.html#auto-traits>");
    // alloc: one error, for an error hir found
    vec![Found::new(diag, Phase::Items)]
}
