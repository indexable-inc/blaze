//! What rustc's borrow checker gives: its errors (E0499, E0502, E0505, E0382, E0596, E0597,
//! E0716, E0384, .., and the ones with no code, "lifetime may not live long enough"), and the
//! lint `unused_mut`.
//!
//! Follows rustc's `compiler/rustc_borrowck/src/borrowck_errors.rs` and
//! `compiler/rustc_borrowck/src/lib.rs` (`lint_unused_mut`) with
//! `compiler/rustc_borrowck/src/session_diagnostics.rs` (`VarNeedNotMut`) (MIT or
//! Apache-2.0). hir gives an error's code, its message, its primary span and rustc's labels,
//! notes, helps and suggestions in rustc's words and order (`crates/hir-ty-borrowck/src/check.rs`);
//! this turns their ranges into spans and nothing else.

use hir::InFile;
use syntax::{
    SyntaxKind, TextRange,
    view::{self as ast, AstNode as _},
};

use crate::{
    cx::Cx,
    lints::{code_fact, registry::LintId},
    lower::{Found, const_check_error::phase},
    model::{Applicability, Diag, MultiSpan, Sublevel, SuggestionStyle},
};

pub(crate) fn lower(cx: &Cx<'_, '_>, d: &hir::BorrowckError) -> Vec<Found> {
    if d.code == "unused_mut" {
        return unused_mut(cx, d);
    }
    let node = cx.syntax_view(d.node);
    let range = match d.at_end {
        true => node.value.last_token().map_or(node.value.text_range(), |it| it.text_range()),
        false => node.value.text_range(),
    };
    let span = cx.span(node.with_value(range));
    let mut diag = match d.code.starts_with(|it: char| it.is_ascii_lowercase()) {
        // clone: the message hir owns, once per diagnostic (the edge).
        true => Diag::lint(d.code, d.message.clone(), span),
        // clone: the message hir owns, once per diagnostic (the edge).
        false => Diag::error((!d.code.is_empty()).then_some(d.code), d.message.clone(), span),
    };
    parts(cx, &mut diag, d);
    code_fact(cx, &mut diag, d.node);
    let phase = phase(cx, d.node);
    match diag.is_lint() {
        // alloc: `lower`'s one `Vec` per diagnostic (lower/mod.rs's interface, the edge).
        true => vec![Found::lint(diag, phase, d.node)],
        // alloc: `lower`'s one `Vec` per diagnostic (lower/mod.rs's interface, the edge).
        false => vec![Found::new(diag, phase)],
    }
}

/// The labels, notes, helps and suggestions hir gives, each range a span. Every part is
/// built once per diagnostic (the edge), sized from hir's counts.
fn parts(cx: &Cx<'_, '_>, diag: &mut Diag, d: &hir::BorrowckError) {
    for &(range, ref label) in &d.labels {
        // clone: a label's text hir owns, once per diagnostic (the edge).
        diag.span_label(cx.span(range), label.clone());
    }
    for child in &d.children {
        let level = match child.level {
            hir::BorrowckSublevel::Note => Sublevel::Note,
            hir::BorrowckSublevel::Help => Sublevel::Help,
        };
        let span = MultiSpan {
            // alloc: a note's spans, once per diagnostic (the edge).
            primary: child.spans.iter().map(|&it| cx.span(it)).collect(),
            // clone: a note's labels hir owns, once per diagnostic (the edge).
            labels: child.labels.iter().map(|&(range, ref label)| (cx.span(range), label.clone())).collect(),
        };
        // clone: a note's text hir owns, once per diagnostic (the edge).
        diag.sub(level, child.message.clone(), span, false);
    }
    for suggestion in &d.suggestions {
        let applicability = match suggestion.applicability {
            hir::BorrowckApplicability::MachineApplicable => Applicability::MachineApplicable,
            hir::BorrowckApplicability::MaybeIncorrect => Applicability::MaybeIncorrect,
            hir::BorrowckApplicability::HasPlaceholders => Applicability::HasPlaceholders,
            hir::BorrowckApplicability::Unspecified => Applicability::Unspecified,
        };
        let style = match suggestion.verbose {
            true => SuggestionStyle::ShowAlways,
            false => SuggestionStyle::ShowCode,
        };
        // clone: a suggestion's edits hir owns, once per diagnostic (the edge).
        let edits = suggestion.parts.iter().map(|&(range, ref code)| (cx.span(range), code.clone())).collect();
        // clone: a suggestion's text hir owns, once per diagnostic (the edge).
        diag.suggestion_with_style(edits, suggestion.message.clone(), applicability, style);
    }
}

/// rustc's `VarNeedNotMut`, at the binding's pattern, with the suggestion to remove `mut `
/// (the source map's `span_until_non_whitespace` of the pattern).
fn unused_mut(cx: &Cx<'_, '_>, d: &hir::BorrowckError) -> Vec<Found> {
    const LINT: LintId = LintId::named("unused_mut");
    if !cx.lint_may_fire(LINT) {
        // alloc: none, an empty `Vec` allocates nothing.
        return Vec::new();
    }
    let InFile { file_id, value: node } = cx.syntax_view(d.node);
    let start = node.text_range().start();
    let mut_token = match node.kind() {
        SyntaxKind::SELF_PARAM => ast::SelfParam::cast(node).and_then(|it| it.mut_token()),
        _ => ast::IdentPat::cast(node).and_then(|it| it.mut_token()),
    };
    // `mut` and the white space after it, up to what comes next.
    let mut_range = match mut_token {
        Some(token) if token.text_range().start() == start => {
            let mut end = token.text_range().end();
            let mut next = token.next_token();
            while let Some(it) = next.filter(|it| it.kind() == SyntaxKind::WHITESPACE) {
                end = it.text_range().end();
                next = it.next_token();
            }
            TextRange::new(start, end)
        }
        _ => TextRange::empty(start),
    };
    let span = cx.span_of(d.node);
    let mut diag = Diag::lint("unused_mut", "variable does not need to be mutable", span);
    if !mut_range.is_empty() {
        diag.suggestion_with_style(
            // alloc: the suggestion's one edit, for a lint that fires (the edge).
            vec![(cx.span(InFile::new(file_id, mut_range)), String::new())],
            "remove this `mut`",
            Applicability::MachineApplicable,
            SuggestionStyle::HideCodeInline,
        );
    }
    code_fact(cx, &mut diag, d.node);
    // alloc: `lower`'s one `Vec` per diagnostic (lower/mod.rs's interface, the edge).
    vec![Found::lint(diag, phase(cx, d.node), d.node)]
}

#[cfg(test)]
mod tests {
    use blaze_alloc_count::assert_at_most;
    use hir::InFile;
    use syntax::view::{self as ast, AstNode as _};

    use crate::lints::with_cx;

    /// A lint allowed on the command line costs a bit test: lowering what hir gives of it
    /// (`unused_mut`, `blaze::const_impure`) allocates nothing.
    #[test]
    fn an_allowed_lint_allocates_nothing() {
        let fixture = "//- /main.rs crate:main\nfn main() { let mut n = 1; let _ = n; }\n";
        with_cx(fixture, &["-A", "unused_mut", "-A", "blaze::const_impure"], |cx| {
            let file = cx.krate.root_module(cx.db).definition_source_file_id(cx.db);
            let pat = cx.root(file).descendants().find_map(ast::IdentPat::cast).expect("the pattern `mut n`");
            let node = InFile::new(file, ast::ptr(pat.syntax()));
            let unused_mut = hir::BorrowckError {
                node,
                at_end: false,
                code: "unused_mut",
                message: String::new(),
                labels: Vec::new(),
                children: Vec::new(),
                suggestions: Vec::new(),
            };
            let impure = hir::ConstImpure { node, message: String::new() };
            // The first lowering decides the crate's lints that may fire, once per crate.
            assert!(super::lower(cx, &unused_mut).is_empty());
            let found = assert_at_most(0, "an allowed lint's lowering", || {
                (super::lower(cx, &unused_mut).len(), crate::lower::const_impure::lower(cx, &impure).len())
            });
            assert_eq!(found, (0, 0));
        });
    }
}
