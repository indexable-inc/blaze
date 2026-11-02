//! E0282 "type annotations needed" of a method called on a raw pointer whose pointee is not
//! known, labelled "cannot call a method on a raw pointer with an unknown pointee type"
//! (rustc's `probe_op` passes `should_label_span = false`, so no "cannot infer type" label).
//!
//! Follows rustc's `compiler/rustc_hir_typeck/src/method/probe.rs` (`probe_op`) (MIT or
//! Apache-2.0).

use crate::{
    cx::Cx,
    lower::{Found, Phase, autoderef_recursion_limit::method_name_span},
    model::Diag,
};

pub(crate) fn lower(cx: &Cx<'_, '_>, d: &hir::MethodOnUnknownRawPointee) -> Vec<Found> {
    let at = method_name_span(cx, d.site);
    // clone: a handle, on the error path
    let mut diag = Diag::error(Some("E0282"), "type annotations needed", at.clone());
    diag.span_label(at, "cannot call a method on a raw pointer with an unknown pointee type");
    // alloc: the error's parts, for an error hir found
    vec![Found::new(diag, Phase::Bodies)]
}
