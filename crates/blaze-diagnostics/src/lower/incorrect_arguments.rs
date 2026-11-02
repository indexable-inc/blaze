//! E0308 "arguments to this function are incorrect": a call with as many arguments as the
//! callee takes, some of which do not fit, with each argument that is of the wrong type,
//! swapped or moved labelled, the function's definition, and the arguments to swap or reorder.
//! A call whose one error is an argument of the wrong type is rustc's "mismatched types" of
//! that argument instead, which is the front end's `TypeMismatch` of it
//! ([`super::type_mismatch`]), so nothing is reported here for it.
//!
//! Follows rustc's `compiler/rustc_hir_typeck/src/fn_ctxt/checks.rs` (`report_arg_errors`, its
//! special case of one `Error::Invalid`) and `fn_ctxt/arg_matrix.rs` (MIT or Apache-2.0),
//! through the call's matrix of [`super::mismatched_arg_count`].

use hir::HirFileId;
use syntax::view as ast;

use super::mismatched_arg_count::{CallCx, Inputs};
use crate::{
    cx::Cx,
    lints::code_fact,
    lower::{Found, Phase},
    model::Diag,
};

pub(crate) fn lower<'db>(cx: &Cx<'_, 'db>, d: &hir::IncorrectArguments) -> Vec<Found> {
    let at = d.call_expr.map(|it| it.syntax_node_ptr());
    let node = cx.node_view(d.call_expr);
    let mut diag = match node.value.left().and_then(|it| CallCx::new(cx, node.file_id, it, Inputs::Fit)) {
        // alloc: empty, allocates nothing
        Some(call) if call.single_mismatch() => return Vec::new(),
        Some(call) => call.report(),
        None => Diag::error(Some("E0308"), "arguments to this function are incorrect", cx.span_of(at)),
    };
    code_fact(cx, &mut diag, at);
    // alloc: the error's parts, for an error hir found
    vec![Found::new(diag, Phase::Bodies)]
}

/// Whether rustc reports the argument of `call` that does not fit as its own "mismatched
/// types": the call's one error. Otherwise the call's `IncorrectArguments` reports all of them.
pub(super) fn single_mismatch<'db>(cx: &Cx<'_, 'db>, file: HirFileId, call: ast::Expr<'db>) -> bool {
    CallCx::new(cx, file, call, Inputs::Fit).is_some_and(|it| it.single_mismatch())
}
