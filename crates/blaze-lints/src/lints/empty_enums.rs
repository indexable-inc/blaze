//! `empty_enums`: an enum with no variants.
//!
//! Follows clippy's `clippy_lints/src/empty_enums.rs` at rust-lang/rust
//! 12433c307b5a497388ffe2f4e49dbf1ab8c71fa0 (MIT or Apache-2.0). Like clippy's it runs in what
//! macros expand to, so it is found in what a derive writes.

use syntax::{
    SyntaxKind,
    view::{self as ast, AstNode as _},
};

use crate::check::Cx;

const LINT: super::Implemented = super::Implemented::EmptyEnums;

pub(super) fn check<'t>(cx: &mut Cx<'_, 't>, item: ast::Enum<'t>) {
    if !cx.on(LINT) {
        return;
    }
    let empty = item.variant_list().is_none_or(|it| it.variants().next().is_none());
    if !empty {
        return;
    }
    let Some(span) = cx.item_span(item.syntax()) else { return };
    if cx.expansion().is_none() && contains_cfg(item) {
        return;
    }
    cx.lint(LINT, item.syntax(), span, "enum with no variants").help(
        "consider using the uninhabited type `!` (never type) or a wrapper around it to introduce \
         a type which can't be instantiated",
    );
}

/// clippy's `span_contains_cfg`: whether the item's span has `#[cfg` in it, past its own
/// attributes.
fn contains_cfg(item: ast::Enum<'_>) -> bool {
    let range = crate::check::item_range(item.syntax());
    let mut tokens = item
        .syntax()
        .descendants_with_tokens()
        .filter_map(|it| it.into_token())
        .filter(|it| range.contains_range(it.text_range()));
    while let Some(token) = tokens.next() {
        if token.kind() != SyntaxKind::POUND {
            continue;
        }
        let mut rest = tokens.clone().skip_while(|it| it.kind().is_trivia());
        if rest.next().is_some_and(|it| it.kind() == SyntaxKind::L_BRACK)
            && rest.next().is_some_and(|it| it.text() == "cfg")
        {
            return true;
        }
    }
    false
}
