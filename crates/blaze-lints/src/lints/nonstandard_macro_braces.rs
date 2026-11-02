//! `nonstandard_macro_braces`: a macro called with other brackets than the ones it is usually
//! called with, as `format! { .. }` or `vec!(..)`; `standard-macro-braces` in `clippy.toml`
//! names more macros and their brackets.
//!
//! Follows clippy's `clippy_lints/src/nonstandard_macro_braces.rs` at rust-lang/rust
//! 12433c307b5a497388ffe2f4e49dbf1ab8c71fa0 (MIT or Apache-2.0).

use syntax::{
    NodeOrToken, SyntaxKind, TextRange,
    view::{self as ast, AstNode as _},
};

use crate::{check::Cx, diag::Applicability};

const LINT: super::Implemented = super::Implemented::NonstandardMacroBraces;

/// The brackets of the macro `name`: those `standard-macro-braces` gives it, the last entry
/// naming it winning, else the ones clippy knows.
fn braces_of(cx: &Cx<'_, '_>, name: &str) -> Option<(char, char)> {
    configured(cx, name).or_else(|| standard(name))
}

fn configured(cx: &Cx<'_, '_>, name: &str) -> Option<(char, char)> {
    let configured = cx.conf().value("standard-macro-braces")?.as_array()?;
    configured.iter().rev().find_map(|matcher| {
        let pair = match matcher.get("brace").and_then(|it| it.as_str()) {
            Some("(" | ")" | "()") => ('(', ')'),
            Some("[" | "]" | "[]") => ('[', ']'),
            Some("{" | "}" | "{}") => ('{', '}'),
            _ => return None,
        };
        (matcher.get("name").and_then(|it| it.as_str()) == Some(name)).then_some(pair)
    })
}

fn standard(name: &str) -> Option<(char, char)> {
    Some(match name {
        "assert_matches" | "debug_assert_matches" | "format" | "format_args" | "eprint"
        | "eprintln" | "print" | "println" | "write" | "writeln" | "matches" => ('(', ')'),
        "cfg_select" => ('{', '}'),
        "vec" => ('[', ']'),
        _ => return None,
    })
}

pub(super) fn check_call<'t>(cx: &mut Cx<'_, 't>, call: ast::MacroCall<'t>) {
    // clippy's `unnested_or_local` is meant to let a call inside a local macro through, but
    // stock clippy lints no call inside another's arguments, local or not (`vec!(..)` in
    // `assert!(..)`, `println!(..)` or a `macro_rules!` of the crate); what an attribute
    // macro such as `#[test]` hands back is linted.
    if !cx.on(LINT) || cx.in_bang_call() {
        return;
    }
    let (Some(path), Some(tt)) = (call.path(), call.token_tree()) else { return };
    let Some(segment) = path.segment() else { return };
    let Some(braces) = braces_of(cx, segment.syntax().text()) else { return };
    let Some(open) = tt.l_paren_token().or(tt.l_brack_token()).or(tt.l_curly_token()) else {
        return;
    };
    let open = open.text().chars().next().unwrap_or_default();
    if open == braces.0 {
        return;
    }
    let (Some(start), Some(end)) = (cx.range(path.syntax()), cx.range(tt.syntax())) else { return };
    let span = TextRange::new(start.start(), end.end());
    // A statement written with braces and no semicolon needs one once it has other brackets.
    let statement = call.syntax().parent().and_then(ast::MacroExpr::cast).and_then(|it| {
        let parent = it.syntax().parent()?;
        let bare = ast::ExprStmt::cast(parent).is_some_and(|it| it.semicolon_token().is_none())
            || ast::StmtList::cast(parent).is_some();
        Some(bare)
    });
    let add_semi = open == '{' && statement == Some(true);
    emit(cx, call.syntax(), span, braces, add_semi);
}

pub(super) fn check_rules<'t>(cx: &mut Cx<'_, 't>, rules: ast::MacroRules<'t>) {
    if !cx.on(LINT) {
        return;
    }
    let Some(body) = rules.token_tree() else { return };
    in_tokens(cx, rules.syntax(), body.syntax());
}

/// Reports the calls `ident ! (..)` in a macro's body whose brackets are not their macro's, in
/// order.
fn in_tokens<'t>(cx: &mut Cx<'_, 't>, rules: ast::Node<'t>, tree: ast::Node<'t>) {
    let mut children = tree.children_with_tokens().filter(|it| !it.kind().is_trivia());
    if children.next().is_none() {
        return;
    }
    while let Some(child) = children.next() {
        // The last child is the closing delimiter.
        let mut rest = children.clone();
        let Some(next) = rest.next() else { break };
        match child {
            NodeOrToken::Node(node) if node.kind() == SyntaxKind::TOKEN_TREE => {
                in_tokens(cx, rules, node);
            }
            NodeOrToken::Token(ident) if ident.kind() == SyntaxKind::IDENT => {
                let Some(NodeOrToken::Node(args)) = rest.next() else { continue };
                if next.kind() != SyntaxKind::BANG || args.kind() != SyntaxKind::TOKEN_TREE {
                    continue;
                }
                let Some(pair) = braces_of(cx, ident.text()) else { continue };
                let open = args.first_token().and_then(|it| it.text().chars().next());
                if matches!(open, Some('(' | '[' | '{')) && open != Some(pair.0) {
                    let span = TextRange::new(ident.text_range().start(), args.text_range().end());
                    emit(cx, rules, span, pair, false);
                }
            }
            _ => {}
        }
    }
}

fn emit<'t>(
    cx: &mut Cx<'_, 't>,
    at: ast::Node<'t>,
    span: TextRange,
    (open, close): (char, char),
    add_semi: bool,
) {
    let snippet = cx.snippet(span);
    let Some((name, args)) = snippet.split_once('!') else { return };
    let mut args = args.trim().chars();
    args.next_back();
    args.next();
    let args = args.as_str();
    let semi = if add_semi { ";" } else { "" };
    let sugg = format!("{name}!{open}{args}{close}{semi}");
    let span = cx.span(span);
    let message = format!("use of irregular braces for `{name}!` macro");
    cx.lint(LINT, at, span.clone(), message).suggestion(
        span,
        "consider writing",
        sugg,
        Applicability::MachineApplicable,
    );
}
