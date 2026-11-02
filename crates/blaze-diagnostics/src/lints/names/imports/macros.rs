//! `unused_macros` and `unused_macro_rules`: the crate's macros no call invokes, and the
//! rules of an invoked `macro_rules!` no call matched.
//!
//! A macro counts: a `macro_rules!` without `#[macro_export]`, or a `macro` that is not `pub`,
//! whose name does not start with `_`. A macro is used when a call, attribute or derive
//! resolves to it; one a call of its name that does not resolve may invoke is not linted, nor
//! are the rules of a macro some call of which matched a rule that can't be told.
//!
//! Follows rustc's `compiler/rustc_resolve/src/build_reduced_graph.rs` (`define_macro`,
//! `insert_unused_macro`), `compiler/rustc_resolve/src/macros.rs` (`check_unused_macros`,
//! `record_macro_rule_usage`, `resolve_macro_or_delegation_path`) and
//! `compiler/rustc_expand/src/mbe/macro_rules.rs` (`get_unused_rule`,
//! `has_compile_error_macro`) (MIT or Apache-2.0).

use hir::InFile;
use hir_def::MacroId;
use hir_expand::name::AsName;
use smallvec::SmallVec;
use syntax::{
    NodeOrToken, SyntaxKind, T, TextRange,
    view::{self as ast, AstNode as _, HasName, HasVisibility, Node},
};

use super::used::Uses;
use crate::{
    cx::Cx,
    lints::{registry::LintId, syntax::Walk},
    lower::{Found, Phase},
    model::Diag,
};

/// rustc's `unused_macros` and `unused_macro_rules`, each at the macro's item.
pub(super) fn lints<'db>(cx: &Cx<'_, 'db>, walk: &Walk<'db>, uses: &Uses<'_, '_>) -> Vec<Found> {
    // alloc: holds only diagnostics that fire; empty, it allocates nothing
    let mut found = Vec::new();
    let (macros, arms) =
        (cx.lint_may_fire(LintId::named("unused_macros")), cx.lint_may_fire(LintId::named("unused_macro_rules")));
    if !macros && !arms {
        return found;
    }
    for tree in &walk.trees {
        for node in tree.of_kind(|it| matches!(it, SyntaxKind::MACRO_RULES | SyntaxKind::MACRO_DEF)) {
            let Some(def) = ast::Macro::cast(node) else { continue };
            let Some(name) = def.name() else { continue };
            let text = name.text();
            if text.starts_with('_') || uses.macros_unknown.contains(&name.as_name()) || exported(walk, def) {
                continue;
            }
            let Some(it) = cx.vsema.to_def(def) else { continue };
            let id = MacroId::from(it);
            let at = tree.ptr(node);
            if !uses.macros.contains(&id) {
                if !macros {
                    continue;
                }
                let span = cx.span(InFile::new(tree.file, name.syntax().text_range()));
                // alloc: the message of a lint that fires
                let mut diag = Diag::lint("unused_macros", format!("unused macro definition: `{text}`"), span);
                // alloc: the fact of a lint that fires
                diag.fact(cx.resolves_fact(it.module(cx.db), text), format!("what the calls of `{text}` resolve to"));
                found.push(Found::lint(diag, Phase::Resolve, at));
                continue;
            }
            if !arms || uses.arms_unknown.contains(&id) {
                continue;
            }
            let matched = uses.arms.get(&id);
            let mut fact = None;
            for (n, rule) in rules(def).enumerate() {
                if rule.compile_error || matched.is_some_and(|it| it.contains(n)) {
                    continue;
                }
                let mut spans = rule.spans.into_iter().map(|it| cx.span(InFile::new(tree.file, it)));
                let Some(first) = spans.next() else { continue };
                // alloc: the message of a lint that fires
                let message = format!("rule #{} of macro `{text}` is never used", n + 1);
                let mut diag = Diag::lint("unused_macro_rules", message, first);
                for span in spans {
                    diag.primary(span);
                }
                let fact = fact.get_or_insert_with(|| cx.resolves_fact(it.module(cx.db), text));
                // clone: alloc: the lint fires; each unused rule of the macro carries the one fact
                diag.fact(fact.clone(), format!("which rules the calls of `{text}` match"));
                found.push(Found::lint(diag, Phase::Resolve, at));
            }
        }
    }
    found
}

/// Whether `def` is exported, so never counted: a `macro_rules!` with `#[macro_export]`, a
/// `pub macro`.
fn exported(walk: &Walk<'_>, def: ast::Macro<'_>) -> bool {
    match def {
        ast::Macro::MacroRules(it) => walk.attrs(ast::AnyHasAttrs::new(it)).any(|(meta, _)| {
            let path = match meta {
                ast::Meta::PathMeta(it) => it.path(),
                ast::Meta::TokenTreeMeta(it) => it.path(),
                ast::Meta::KeyValueMeta(it) => it.path(),
                _ => None,
            };
            path.and_then(|it| it.as_single_name_ref()).is_some_and(|it| it.text() == "macro_export")
        }),
        ast::Macro::MacroDef(it) => it.visibility().is_some_and(|it| matches!(it.kind(), ast::VisibilityKind::Pub)),
    }
}

/// A rule of a macro: where rustc points at it (the matcher; an attribute rule's arguments
/// and body), and whether its expansion calls `compile_error!`.
struct Rule {
    spans: SmallVec<[TextRange; 2]>,
    compile_error: bool,
}

/// The rules of `def`, in order: `matcher => expansion` separated by `;` in a `macro_rules!`,
/// by `,` in a `macro`, whose `macro m(..) { .. }` form is one rule.
///
/// The body's delimiters are walked with the rest: neither is a `=`, `=>` or token tree, so
/// neither changes a rule.
fn rules(def: ast::Macro<'_>) -> impl Iterator<Item = Rule> {
    let (single, body) = match def {
        ast::Macro::MacroRules(it) => (None, it.token_tree()),
        ast::Macro::MacroDef(it) => match it.args() {
            Some(args) => {
                let mut spans = SmallVec::new();
                spans.push(args.syntax().text_range());
                let compile_error = it.body().is_some_and(|it| has_compile_error(it.syntax()));
                (Some(Rule { spans, compile_error }), None)
            }
            None => (None, it.body()),
        },
    };
    let mut elements = body
        .into_iter()
        .flat_map(|it| it.syntax().children_with_tokens())
        .filter(|it| !it.kind().is_trivia())
        .peekable();
    let mut spans: SmallVec<[TextRange; 2]> = SmallVec::new();
    let mut after_arrow = false;
    single.into_iter().chain(std::iter::from_fn(move || {
        while let Some(element) = elements.next() {
            match element {
                NodeOrToken::Token(token) if token.kind() == T![=] => {
                    elements.next_if(|it| it.kind() == T![>]);
                    after_arrow = true;
                }
                NodeOrToken::Token(token) if token.kind() == T![=>] => after_arrow = true,
                NodeOrToken::Node(node) if node.kind() == SyntaxKind::TOKEN_TREE => {
                    if after_arrow {
                        after_arrow = false;
                        return Some(Rule { spans: std::mem::take(&mut spans), compile_error: has_compile_error(node) });
                    }
                    spans.push(node.text_range());
                }
                _ => {}
            }
        }
        None
    }))
}

/// Whether a token tree has `compile_error!` followed by a delimited group in it, at any
/// depth: rustc's `has_compile_error_macro`.
fn has_compile_error(tt: Node<'_>) -> bool {
    // How much of `compile_error ! (..)` the elements so far end with: nothing, the name, the `!`.
    let mut seen = 0u8;
    for element in tt.children_with_tokens() {
        match element {
            NodeOrToken::Token(token) => match token.kind() {
                kind if kind.is_trivia() => {}
                SyntaxKind::IDENT if token.text() == "compile_error" => seen = 1,
                T![!] if seen == 1 => seen = 2,
                _ => seen = 0,
            },
            NodeOrToken::Node(node) => {
                if node.kind() == SyntaxKind::TOKEN_TREE && (seen == 2 || has_compile_error(node)) {
                    return true;
                }
                seen = 0;
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use syntax::{
        SourceFile,
        view::{self as ast, AstNode as _},
    };

    use super::rules;

    fn spans(text: &str) -> Vec<(Vec<String>, bool)> {
        let parse = SourceFile::parse(text, span::Edition::CURRENT);
        let def = parse.root().descendants().find_map(ast::Macro::cast).unwrap();
        rules(def)
            .into_iter()
            .map(|rule| (rule.spans.iter().map(|it| text[*it].to_owned()).collect(), rule.compile_error))
            .collect()
    }

    #[test]
    fn rules_of_a_macro_rules_are_its_matchers() {
        let found = spans("macro_rules! m { (a) => {}; [b $x:expr] => { compile_error!(\"no\") }; {} => (()) }");
        assert_eq!(
            found,
            vec![
                (vec!["(a)".to_owned()], false),
                (vec!["[b $x:expr]".to_owned()], true),
                (vec!["{}".to_owned()], false),
            ]
        );
    }

    #[test]
    fn rules_of_a_macro_are_separated_by_commas() {
        let found = spans("macro m { (a) => {}, (b) => { x::compile_error! {} } }");
        assert_eq!(found, vec![(vec!["(a)".to_owned()], false), (vec!["(b)".to_owned()], true)]);
        assert_eq!(spans("macro m($x:expr) { $x }"), vec![(vec!["($x:expr)".to_owned()], false)]);
    }
}
