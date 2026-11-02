//! E0059 "cannot use call notation; the first type parameter for the function trait is
//! neither a tuple nor unit", at the call. Only under `unboxed_closures`.
//!
//! Follows rustc's `compiler/rustc_hir_typeck/src/fn_ctxt/checks.rs` (`check_argument_types`)
//! (MIT or Apache-2.0).

use hir::InFile;

use crate::{
    cx::Cx,
    lints::code_fact,
    lower::{Found, Phase},
    model::Diag,
};

pub(crate) fn lower(cx: &Cx<'_, '_>, d: &hir::FnTraitArgsNotTuple) -> Vec<Found> {
    let ptr = InFile::new(d.call.file_id, d.call.value.syntax_node_ptr());
    let message = "cannot use call notation; the first type parameter for the function trait is neither a tuple nor unit";
    let mut diag = Diag::error(Some("E0059"), message, cx.span_of(ptr));
    code_fact(cx, &mut diag, ptr);
    // alloc: the error's parts, for an error hir found
    vec![Found::new(diag, Phase::Bodies)]
}
