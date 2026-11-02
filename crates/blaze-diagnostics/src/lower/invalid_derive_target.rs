//! E0774, `#[derive]` on something that is not a `struct`, `enum` or `union`: the attribute
//! "not applicable here" and the item "not a `struct`, `enum` or `union`".
//!
//! Follows rustc's `compiler/rustc_builtin_macros/src/derive.rs` (`report_bad_target`) and
//! `diagnostics.rs` (`BadDeriveTarget`) (MIT or Apache-2.0).

use hir::InFile;
use syntax::view::{self as ast, AstNode as _};

use crate::{
    cx::{Cx, item_range_view as item_range},
    lower::{Found, Phase},
    model::Diag,
};

pub(crate) fn lower(cx: &Cx<'_, '_>, d: &hir::InvalidDeriveTarget) -> Vec<Found> {
    let file = d.range.file_id;
    let root = cx.root(file);
    let message = "`derive` may only be applied to `struct`s, `enum`s and `union`s";
    let Some(attr) = root.covering_element(d.range.value).ancestors().find_map(ast::Attr::cast) else {
        let diag = Diag::error(Some("E0774"), message, cx.span(d.range));
        // alloc: the error's parts, for an error hir found
        return vec![Found::new(diag, Phase::Expand)];
    };
    let span = cx.span(InFile::new(file, attr.syntax().text_range()));
    // clone: on the error path, for an error hir found
    let mut diag = Diag::error(Some("E0774"), message, span.clone());
    diag.span_label(span, "not applicable here");
    if let Some(item) = attr.syntax().parent() {
        diag.span_label(
            cx.span(InFile::new(file, item_range(item))),
            "not a `struct`, `enum` or `union`",
        );
    }
    // alloc: the error's parts, for an error hir found
    vec![Found::new(diag, Phase::Expand)]
}
