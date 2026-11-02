//! hir's `IntLiteralTooLarge`: an integer literal whose digits do not fit `u128`, rustc's
//! `LitError::IntTooLarge`, reported by AST lowering (so only for code cfg keeps), with the
//! limit written in the literal's radix.
//!
//! Follows `compiler/rustc_session/src/errors.rs` (`report_lit_error`, `IntLiteralTooLarge`)
//! (MIT or Apache-2.0).

use hir::InFile;
use syntax::view::AstNode as _;

use crate::{
    cx::Cx,
    lower::{Found, Phase},
    model::Diag,
};

pub(crate) fn lower(cx: &Cx<'_, '_>, d: &hir::IntLiteralTooLarge) -> Vec<Found> {
    let lit = cx.node_view(d.node);
    let at = cx.span(InFile::new(lit.file_id, lit.value.syntax().text_range()));
    // alloc: the literal's text, for an error hir found
    let text = lit.value.syntax().text().to_string();
    let max = u128::MAX;
    // alloc: the note's text, for an error hir found
    let limit = match text.get(..2) {
        Some("0b") => format!("{max:#b}"),
        Some("0o") => format!("{max:#o}"),
        Some("0x") => format!("{max:#x}"),
        _ => format!("{max}"),
    };
    let mut diag = Diag::error(None, "integer literal is too large", at);
    // alloc: the note's text, for an error hir found
    diag.note(format!("value exceeds limit of `{limit}`"));
    // alloc: the error's parts, for an error hir found
    vec![Found::new(diag, Phase::Lower)]
}
