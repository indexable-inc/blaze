//! E0055 "reached the recursion limit while auto-dereferencing `{ty}`", labelled "deref
//! recursion limit reached", with help "consider increasing the recursion limit by adding a
//! `#![recursion_limit = "{suggested_limit}"]` attribute to your crate (`{crate_name}`)".
//!
//! Follows rustc's `compiler/rustc_hir_analysis/src/autoderef.rs`
//! (`report_autoderef_recursion_limit_error`) and `errors.rs`
//! (`AutoDerefReachedRecursionLimit`) (MIT or Apache-2.0).

use hir::SpanSyntax;
use syntax::view::{self as ast, AstNode as _};

use crate::{
    cx::Cx,
    lower::{Found, Phase},
    model::{Diag, Span},
};

pub(crate) fn lower<'db>(cx: &Cx<'_, 'db>, d: &hir::AutoderefRecursionLimit<'db>) -> Vec<Found> {
    let at = method_name_span(cx, d.site);
    let ty = cx.ty(&d.ty);
    // alloc: the error's text, for an error hir found
    let message = format!("reached the recursion limit while auto-dereferencing `{ty}`");
    // clone: a handle, on the error path
    let mut diag = Diag::error(Some("E0055"), message, at.clone());
    diag.span_label(at, "deref recursion limit reached");
    let krate = cx.krate.display_name(cx.db).expect("a crate blaze checks has a name");
    // alloc: the error's text, for an error hir found
    diag.help(format!(
        "consider increasing the recursion limit by adding a `#![recursion_limit = \"{}\"]` attribute to your crate (`{krate}`)",
        d.suggested_limit,
    ));
    // alloc: the error's parts, for an error hir found
    vec![Found::new(diag, Phase::Bodies)]
}

/// rustc's span of a method call's probe: its name; a field access's field; else the site.
pub(crate) fn method_name_span(cx: &Cx<'_, '_>, site: SpanSyntax) -> Span {
    let node = cx.syntax_view(site.map(|it| it.syntax_node_ptr()));
    let name = match ast::Expr::cast(node.value) {
        Some(ast::Expr::MethodCallExpr(it)) => it.name_ref(),
        Some(ast::Expr::FieldExpr(it)) => it.name_ref(),
        _ => None,
    };
    let range = name.map_or(node.value.text_range(), |it| it.syntax().text_range());
    cx.span(node.with_value(range))
}
