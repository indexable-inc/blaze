//! "functional record updates are not allowed in destructuring assignments": `S { a, ..base }`
//! on the left of `=`, where only `..` may end the fields. rustc gives it no code.
//!
//! Follows rustc's `compiler/rustc_ast_lowering/src/expr.rs` (`destructure_assign_mut`) and
//! `compiler/rustc_ast_lowering/src/diagnostics.rs`
//! (`FunctionalRecordUpdateDestructuringAssignment`) (MIT or Apache-2.0).

use crate::{
    cx::Cx,
    lints::code_fact,
    lower::{Found, Phase},
    model::{Applicability, Diag},
};

pub(crate) fn lower(cx: &Cx<'_, '_>, d: &hir::FruInDestructuringAssignment) -> Vec<Found> {
    let span = cx.span_of_ptr(d.node);
    let message = "functional record updates are not allowed in destructuring assignments";
    let mut diag = Diag::error(None, message, span.clone()); // clone: the error's span
    diag.suggestion(span, "consider removing the trailing pattern", "", Applicability::MachineApplicable);
    code_fact(cx, &mut diag, d.node.map(|it| it.syntax_node_ptr()));
    vec![Found::new(diag, Phase::Resolve)] // alloc: the error found
}
