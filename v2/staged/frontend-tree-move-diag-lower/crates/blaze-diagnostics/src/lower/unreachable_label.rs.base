//! E0767: a label named through a function, a closure, an `async` block or a constant, where
//! it can't be reached.
//!
//! Follows rustc's `compiler/rustc_resolve/src/late.rs` (`resolve_label`),
//! `diagnostics/impls.rs` and `diagnostics/mod.rs` (`UnreachableLabel`) (MIT or Apache-2.0).

use hir::InFile;
use syntax::{AstNode, TextRange};

use crate::{
    cx::Cx,
    lints::code_fact,
    lower::{
        Found, Phase,
        undeclared_label::{Resolution, Similar, resolve},
    },
    model::{Applicability, Diag, Span},
};

pub(crate) fn lower(cx: &Cx<'_, '_>, d: &hir::UnreachableLabel) -> Vec<Found> {
    let node = cx.node(d.node);
    let name = node.value.text().to_owned();
    let span = cx.span_of_ptr(d.node);
    let mut diag = match resolve(cx, &node.map(|it| it.syntax().clone()), &name) {
        Resolution::Unreachable { defined, similar } => unreachable(cx, span, &name, Some(defined), similar),
        _ => unreachable(cx, span, &name, None, None),
    };
    code_fact(cx, &mut diag, d.node.map(|it| it.syntax_node_ptr()));
    vec![Found::new(diag, Phase::Resolve)]
}

/// rustc's `UnreachableLabel` of the label `name` used at `span`, defined at `defined`.
pub(crate) fn unreachable(
    cx: &Cx<'_, '_>,
    span: Span,
    name: &str,
    defined: Option<InFile<TextRange>>,
    similar: Option<Similar>,
) -> Diag {
    let mut diag = Diag::error(Some("E0767"), format!("use of unreachable label `{name}`"), span.clone());
    diag.span_label(span.clone(), format!("unreachable label `{name}`"));
    if let Some(defined) = defined {
        diag.span_label(cx.span(defined), "unreachable label defined here");
    }
    diag.note("labels are unreachable through functions, closures, async blocks and modules");
    match similar {
        Some(similar) if similar.reachable => {
            diag.suggestion(span, "try using similarly named label", similar.name, Applicability::MaybeIncorrect);
            diag.span_label(cx.span(similar.at), "a label with a similar name is reachable");
        }
        Some(similar) => {
            diag.span_label(cx.span(similar.at), "a label with a similar name exists but is also unreachable");
        }
        None => {}
    }
    diag
}
