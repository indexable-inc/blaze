//! `method_call_on_diverging_infer_var` "method call on a diverging inference variable": a
//! method called on a value whose type is a variable a coercion of `!` made (`let x = loop {};
//! x.method()`), which the probe set to `!` (Rust 1.99; an error before it, and to be one again).
//!
//! Follows rustc's `compiler/rustc_hir_typeck/src/method/probe.rs` (`probe_op`,
//! `MethodCallOnDivergingInferenceVariable`) (MIT or Apache-2.0).

use crate::{
    cx::Cx,
    lints::{code_fact, registry::LintId},
    lower::{Found, Phase, autoderef_recursion_limit::method_name_span},
    model::Diag,
};

const METHOD_CALL_ON_DIVERGING_INFER_VAR: LintId =
    LintId::named("method_call_on_diverging_infer_var");

pub(crate) fn lower(cx: &Cx<'_, '_>, d: &hir::MethodCallOnDivergingInferVar) -> Vec<Found> {
    if !cx.lint_may_fire(METHOD_CALL_ON_DIVERGING_INFER_VAR) {
        // alloc: empty, allocates nothing
        return Vec::new();
    }
    let site = d.site.map(|it| it.syntax_node_ptr());
    let mut diag = Diag::lint(
        "method_call_on_diverging_infer_var",
        "method call on a diverging inference variable",
        method_name_span(cx, d.site),
    );
    diag.help("consider providing a type annotation");
    code_fact(cx, &mut diag, site);
    // alloc: the lint, for a call hir found
    vec![Found::lint(diag, Phase::Bodies, site)]
}
