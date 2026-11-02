//! The evaluation of a free `const` or a `static` failed: rustc's E0080 with the evaluator's
//! reason as its message, E0391 for a value that needs itself, and blaze's own error for what
//! its evaluator cannot do (no-fallback: a value blaze did not evaluate is one it can't say
//! rustc accepts).
//!
//! Follows rustc's `compiler/rustc_const_eval/src/const_eval/error.rs` (`report`) and
//! `compiler/rustc_interface/src/passes.rs` (`analysis`), MIT or Apache-2.0.

use hir::ConstEvalFailure;

use crate::{
    cx::Cx,
    lints::code_fact,
    lower::{Found, const_check_error::phase},
    model::Diag,
};

pub(crate) fn lower(cx: &Cx<'_, '_>, d: &hir::ConstEvalFailed) -> Vec<Found> {
    let span = cx.span_of(d.node);
    let what = match d.is_static {
        true => "static",
        false => "constant",
    };
    let mut diag = match &d.failure {
        ConstEvalFailure::Failed(reason) => {
            // clone: the message hir owns, once per diagnostic (the edge).
            let mut diag = Diag::error(Some("E0080"), reason.clone(), span);
            // alloc: the note's text, once per diagnostic (the edge).
            diag.note(format!("evaluation of {what} `{}` failed", d.item));
            diag
        }
        ConstEvalFailure::Cycle => Diag::error(
            Some("E0391"),
            // alloc: the message, once per diagnostic (the edge).
            format!("cycle detected when evaluating {what} `{}`: its value needs itself", d.item),
            span,
        ),
        ConstEvalFailure::Unsupported(reason) => Diag::error(
            None,
            // alloc: the message, once per diagnostic (the edge).
            format!("blaze cannot evaluate {what} `{}`: {reason}", d.item),
            span,
        ),
    };
    code_fact(cx, &mut diag, d.node);
    // alloc: `lower`'s one `Vec` per diagnostic (lower/mod.rs's interface, the edge).
    vec![Found::new(diag, phase(cx, d.node))]
}
