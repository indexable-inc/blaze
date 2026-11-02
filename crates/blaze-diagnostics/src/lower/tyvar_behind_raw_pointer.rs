//! `tyvar_behind_raw_pointer` "type annotations needed" of a method called on a raw pointer
//! whose pointee is not known, in edition 2015 (a hard error, E0282, from 2018).
//!
//! Follows rustc's `compiler/rustc_hir_typeck/src/method/probe.rs` (`probe_op`) (MIT or
//! Apache-2.0).

use crate::{
    cx::Cx,
    lints::{code_fact, registry::LintId},
    lower::{Found, Phase, autoderef_recursion_limit::method_name_span},
    model::Diag,
};

const TYVAR_BEHIND_RAW_POINTER: LintId = LintId::named("tyvar_behind_raw_pointer");

pub(crate) fn lower(cx: &Cx<'_, '_>, d: &hir::TyvarBehindRawPointer) -> Vec<Found> {
    if !cx.lint_may_fire(TYVAR_BEHIND_RAW_POINTER) {
        // alloc: empty, allocates nothing
        return Vec::new();
    }
    let site = d.site.map(|it| it.syntax_node_ptr());
    let mut diag = Diag::lint("tyvar_behind_raw_pointer", "type annotations needed", method_name_span(cx, d.site));
    code_fact(cx, &mut diag, site);
    // alloc: the lint, for a call hir found
    vec![Found::lint(diag, Phase::Bodies, site)]
}
