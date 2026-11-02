//! "the `Self` constructor can only be used with tuple or unit structs" (no code), at a `Self`
//! expression whose impl's type has no constructor: with "did you mean to use one of the
//! enum's variants?" for an enum, "use curly brackets" `Self { /* fields */ }` for a struct
//! or union.
//!
//! Follows rustc's `compiler/rustc_hir_typeck/src/fn_ctxt/_impl.rs` (`instantiate_value_path`,
//! `Res::SelfCtor`) (MIT or Apache-2.0).

use hir::{Adt, InFile};

use crate::{
    cx::Cx,
    lints::code_fact,
    lower::{Found, Phase},
    model::{Applicability, Diag},
};

pub(crate) fn lower(cx: &Cx<'_, '_>, d: &hir::SelfCtorNotTupleOrUnit<'_>) -> Vec<Found> {
    let ptr = InFile::new(d.expr.file_id, d.expr.value.syntax_node_ptr());
    let at = cx.span_of(ptr);
    // clone: a handle, on the error path
    let mut diag =
        Diag::error(None, "the `Self` constructor can only be used with tuple or unit structs", at.clone());
    match d.self_ty.as_adt() {
        Some(Adt::Enum(_)) => {
            diag.help("did you mean to use one of the enum's variants?");
        }
        Some(Adt::Struct(_) | Adt::Union(_)) => {
            diag.suggestion(at, "use curly brackets", "Self { /* fields */ }", Applicability::HasPlaceholders);
        }
        None => {}
    }
    code_fact(cx, &mut diag, ptr);
    // alloc: the error's parts, for an error hir found
    vec![Found::new(diag, Phase::Bodies)]
}
