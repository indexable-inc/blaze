//! A body the borrow checker did not check: blaze's own error, not rustc's. rustc checks every
//! body; a body blaze can't check is one it can't say rustc accepts, so it is refused
//! (no-fallback) until the borrow checker supports what is in it.

use blaze_items::Fact;
use syntax::view::{self as ast, AstNode as _};

use crate::{
    cx::{Cx, signature_range_view as signature_range},
    lints::body_path,
    lower::{Found, const_check_error::phase},
    model::Diag,
};

pub(crate) fn lower(cx: &Cx<'_, '_>, d: &hir::BorrowckSkipped) -> Vec<Found> {
    let node = cx.syntax_view(d.node);
    // A function is pointed at by its signature, as rustc points at an item (`def_span`).
    let span = match node.value.parent().and_then(ast::Fn::cast) {
        Some(item) => cx.span(node.with_value(signature_range(item))),
        None => cx.span_of(d.node),
    };
    // The item is found once, for both the message and the fact (`code_fact` would find it
    // again).
    let item = body_path(cx, d.node);
    let path = match &item {
        Some((_, path)) => path.as_str(),
        None => cx.snippet(&span).unwrap_or_default(),
    };
    // alloc: the message, once per diagnostic (the edge).
    let mut diag = Diag::error(None, format!("blaze cannot borrow-check `{path}`: {}", d.reason), span);
    if let Some((item, path)) = &item {
        let name = path.rsplit("::").next().unwrap_or(path);
        // alloc: the fact's text, once per diagnostic (the edge).
        diag.fact(Fact::Code(cx.key(*item)), format!("the code of `{name}`"));
    }
    // alloc: `lower`'s one `Vec` per diagnostic (lower/mod.rs's interface, the edge).
    vec![Found::new(diag, phase(cx, d.node))]
}
