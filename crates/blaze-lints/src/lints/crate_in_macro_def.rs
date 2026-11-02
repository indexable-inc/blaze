//! `crate_in_macro_def`: `crate::` in an exported `macro_rules!`, which names the crate the
//! macro is called from rather than its own.
//!
//! Follows clippy's `clippy_lints/src/crate_in_macro_def.rs` at rust-lang/rust
//! 12433c307b5a497388ffe2f4e49dbf1ab8c71fa0 (MIT or Apache-2.0).

use syntax::{
    NodeOrToken, SyntaxKind, TextRange,
    view::{self as ast, AstNode as _, HasAttrs as _},
};

use crate::{check::Cx, diag::Applicability};

const LINT: super::Implemented = super::Implemented::CrateInMacroDef;

pub(super) fn check<'t>(cx: &mut Cx<'_, 't>, rules: ast::MacroRules<'t>) {
    if !cx.on(LINT) || !cx.in_file() {
        return;
    }
    let exported = rules.attrs().any(|attr| {
        attr.path().is_some_and(|path| {
            path.qualifier().is_none() && path.syntax().text() == "macro_export"
        })
    });
    let Some(body) = rules.token_tree() else { return };
    if !exported {
        return;
    }
    let Some(range) = unhygienic_crate(body.syntax()) else { return };
    let span = cx.span(range);
    cx.lint(LINT, rules.syntax(), span.clone(), "`crate` references the macro call's crate")
        .suggestion(
            span,
            "to reference the macro definition's crate, use",
            "$crate".to_owned(),
            Applicability::MachineApplicable,
        );
}

/// clippy's `contains_unhygienic_crate_reference`: the first `crate` followed by `::` and not
/// by `$`, in the trees of `tt` in order, between its delimiters.
fn unhygienic_crate(tt: ast::Node<'_>) -> Option<TextRange> {
    let mut trees = tt.children_with_tokens().filter(|it| !it.kind().is_trivia());
    trees.next()?;
    let mut prev_is_dollar = false;
    while let Some(tree) = trees.next() {
        // The last tree is the closing delimiter.
        if trees.clone().next().is_none() {
            break;
        }
        if !prev_is_dollar && tree.kind() == SyntaxKind::CRATE_KW && path_sep_next(trees.clone())
        {
            return Some(tree.text_range());
        }
        if let NodeOrToken::Node(node) = tree
            && let Some(found) = unhygienic_crate(node)
        {
            return Some(found);
        }
        prev_is_dollar = tree.kind() == SyntaxKind::DOLLAR;
    }
    None
}

/// Whether `::` comes next in `rest`, as one token or as two joined colons. A closing
/// delimiter is neither, so it needs no excluding.
fn path_sep_next<'t>(mut rest: impl Iterator<Item = ast::Element<'t>>) -> bool {
    match (rest.next(), rest.next()) {
        (Some(first), _) if first.kind() == SyntaxKind::COLON2 => true,
        (Some(NodeOrToken::Token(first)), Some(NodeOrToken::Token(second))) => {
            first.kind() == SyntaxKind::COLON
                && second.kind() == SyntaxKind::COLON
                && first.text_range().end() == second.text_range().start()
        }
        _ => false,
    }
}
