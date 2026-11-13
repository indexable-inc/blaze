//! The pattern syntax rustc's parser refuses: a `box` pattern, removed with `box_patterns`
//! in Rust 1.100 (rust-lang/rust#156749), in a pattern's place and as a struct pattern's
//! field shorthand (`Packet { box x }`).
//!
//! Follows rustc's `compiler/rustc_parse/src/parser/pat.rs` (`parse_pat_box`,
//! `parse_pat_field`) and `diagnostics.rs` (`BoxPatsRemoved`, `UseDerefMacro`) (MIT or
//! Apache-2.0).

use syntax::{
    SyntaxKind, TextRange,
    view::{self as ast, AstNode as _, HasName as _},
};

use super::{Out, V};
use crate::model::{Applicability, Diag};

/// `BoxPatsRemoved` at the `box` of a box pattern.
pub(super) fn box_pat<'db>(v: &V<'_, '_, 'db>, node: ast::Node<'db>, out: &mut Out) {
    let Some(pat) = ast::BoxPat::cast(node) else { return };
    let (Some(keyword), Some(inner)) = (pat.box_token(), pat.pat()) else { return };
    let keyword = keyword.text_range();
    let inner = inner.syntax().text_range();
    // `box` and the white space after it.
    let before = TextRange::new(keyword.start(), inner.start());
    // A field's shorthand: `box x` binds the field `x`, which a `deref!` has to name.
    let field = node
        .parent()
        .filter(|it| it.kind() == SyntaxKind::RECORD_PAT_FIELD)
        .filter(|it| ast::RecordPatField::cast(*it).is_some_and(|it| it.name_ref().is_none()))
        .and_then(|_| match pat.pat() {
            Some(ast::Pat::IdentPat(it)) => it.name(),
            _ => None,
        });
    let mut diag = Diag::error(
        None,
        "`box` patterns have been removed (feature `box_patterns`)",
        v.span(keyword),
    );
    diag.help("enable feature `deref_patterns` instead and...");
    diag.suggestion_verbose(
        v.span(before),
        "...if possible just remove keyword `box`...",
        "",
        Applicability::MaybeIncorrect,
    );
    // alloc: the error's suggestion
    let mut parts = Vec::new();
    let open = match field {
        Some(name) => {
            // alloc: the error's suggestion
            parts.push((v.span(before), format!("{}: ", name.text())));
            TextRange::empty(inner.start())
        }
        None => before,
    };
    // alloc: the error's suggestion
    parts.push((v.span(open), "deref!(".to_owned()));
    // alloc: the error's suggestion
    parts.push((v.span(TextRange::empty(inner.end())), ")".to_owned()));
    diag.multipart_suggestion(
        "...otherwise replace it with an invocation of macro `deref`",
        parts,
        Applicability::MaybeIncorrect,
    );
    v.emit_parse(out, node, diag);
}
