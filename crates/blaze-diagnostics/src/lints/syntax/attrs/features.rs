//! The feature lints: `incomplete_features` and `internal_features` of the features the crate's
//! `#![feature(..)]` enables, as the early lint pass gives them at the crate, and
//! `unstable_features` of every `#[feature(..)]`, as the late lint pass gives it at the node the
//! attribute is on.
//!
//! - [`table`]: rustc's language features, which are incomplete or internal, their tracking
//!   issues.
//!
//! Follows rustc's `compiler/rustc_lint/src/builtin.rs` (`IncompleteInternalFeatures`:
//! `check_crate`, `HAS_MIN_FEATURES`; `UnstableFeatures`: `check_attributes`),
//! `compiler/rustc_lint/src/lints.rs` (`BuiltinIncompleteFeatures`, `BuiltinInternalFeatures`,
//! `BuiltinIncompleteFeaturesHelp`, `BuiltinFeatureIssueNote`, `BuiltinUnstableFeatures`),
//! `compiler/rustc_expand/src/config.rs` (`features`: removed features are not enabled,
//! language features come before library ones), `compiler/rustc_feature/src/unstable.rs`
//! (`enabled_features_iter_stable_order`) and `compiler/rustc_attr_parsing/src/attributes/
//! crate_level.rs` (`FeatureParser`: which items of the list are features) (MIT or Apache-2.0).

pub(crate) mod table;

use syntax::{NodeOrToken, SyntaxKind, T, TextRange, view as ast};

use super::{Args, Attr, Owner, Target};
use crate::{
    cx::Cx,
    lints::{
        registry::LintId,
        syntax::{Emitted, Pass, Walk, source_fact},
    },
    lower::Phase,
    model::Diag,
};

/// The incomplete features rustc suggests a `min_` feature in place of (`HAS_MIN_FEATURES`).
const HAS_MIN_FEATURES: &[&str] = &["specialization"];

/// A feature `#[feature(..)]` names: its name and where it is written in the attribute's file.
pub(crate) struct Named<'a> {
    pub(crate) attr: &'a Attr<'a>,
    pub(crate) name: &'a str,
    pub(crate) range: TextRange,
}

/// The feature lints of the crate.
pub(crate) fn lints(cx: &Cx<'_, '_>, _walk: &Walk<'_>, owners: &[Owner<'_>]) -> Vec<Emitted> {
    // alloc: empty, allocates nothing until a lint
    let mut emitted = Vec::new();
    if let Some(krate) = owners.iter().find(|it| it.target == Target::Crate) {
        incomplete_or_internal(cx, krate, &mut emitted);
    }
    // A macro call's attributes are gone by the time HIR is built, so the late pass never sees
    // them.
    if cx.lint_may_fire(LintId::named("unstable_features")) {
        for owner in owners.iter().filter(|it| it.target != Target::MacroCall) {
            unstable(cx, owner, &mut emitted);
        }
    }
    emitted
}

/// `IncompleteInternalFeatures::check_crate`: each feature the crate enables that is
/// incomplete or internal, the language ones first, each in the order written.
fn incomplete_or_internal(cx: &Cx<'_, '_>, krate: &Owner<'_>, emitted: &mut Vec<Emitted>) {
    // alloc: setup, once per crate: the features of its root
    let enabled: Vec<Named<'_>> = named(krate).filter(|it| table::removed(it.name).is_none()).collect();
    let (lang, lib): (Vec<&Named<'_>>, Vec<&Named<'_>>) =
        enabled.iter().partition(|it| table::is_lang(it.name));
    for feature in lang.into_iter().chain(lib) {
        let name = feature.name;
        let span = cx.span(feature.attr.in_file(feature.range));
        let mut diag = if table::incomplete(name) {
            let mut diag = Diag::lint(
                "incomplete_features",
                // alloc: the diagnostic's text, as is each below
                format!("the feature `{name}` is incomplete and may not be safe to use and/or cause compiler crashes"),
                span,
            );
            if let Some(n) = table::issue(name) {
                // alloc: as above
                diag.note(format!("see issue #{n} <https://github.com/rust-lang/rust/issues/{n}> for more information"));
            }
            if HAS_MIN_FEATURES.contains(&name) {
                // alloc: as above
                diag.help(format!("consider using `min_{name}` instead, which is more stable and complete"));
            }
            diag
        } else if table::internal(name) {
            let mut diag = Diag::lint(
                "internal_features",
                // alloc: as above
                format!("the feature `{name}` is internal to the compiler or standard library"),
                span,
            );
            diag.note("using it is strongly discouraged");
            diag
        } else {
            continue;
        };
        source_fact(cx, &mut diag, krate.at());
        emitted.push(Emitted::new(diag, Phase::Resolve, Pass::Early, krate.at()));
    }
}

/// `UnstableFeatures::check_attributes`: each feature `owner`'s `#[feature(..)]`s name, a
/// removed one included.
fn unstable(cx: &Cx<'_, '_>, owner: &Owner<'_>, emitted: &mut Vec<Emitted>) {
    for feature in named(owner) {
        let span = cx.span(feature.attr.in_file(feature.range));
        let mut diag = Diag::lint("unstable_features", "use of an unstable feature", span);
        source_fact(cx, &mut diag, owner.at());
        emitted.push(Emitted::new(diag, Phase::Late, Pass::Early, owner.at()));
    }
}

/// The features `owner`'s `#[feature(..)]`s name, in order: rustc's `FeatureParser` combines
/// them into one list.
pub(crate) fn named<'a>(owner: &'a Owner<'a>) -> impl Iterator<Item = Named<'a>> {
    owner.attrs.iter().filter(|it| is_feature(it)).flat_map(|attr| {
        // alloc: empty, allocates nothing
        let Args::List(list) = attr.args() else { return Vec::new() };
        // alloc: setup, the features of a `#[feature(..)]`, read once per check that asks
        features(list).into_iter().map(|(name, range)| Named { attr, name, range }).collect()
    })
}

/// Whether `attr` is `#[feature..]`.
fn is_feature(attr: &Attr<'_>) -> bool {
    attr.meta()
        .and_then(|it| it.path())
        .and_then(|it| it.as_single_name_ref())
        .is_some_and(|it| it.text() == "feature")
}

/// The features a `feature(..)` list names, as rustc's `FeatureParser::extend` takes them: each
/// item that is a single identifier with no arguments. A list in other delimiters, or one that
/// is not a list of meta items, rustc does not parse: it names none.
fn features(list: ast::TokenTree<'_>) -> Vec<(&str, TextRange)> {
    if list.left_delimiter_token().is_none_or(|it| it.kind() != T!['(']) {
        // alloc: empty, allocates nothing
        return Vec::new();
    }
    // alloc: setup, as `named`: a `#[feature(..)]`'s items
    let mut items: Vec<Vec<NodeOrToken<ast::TokenTree<'_>, ast::Token<'_>>>> = Vec::new();
    // alloc: as above
    let mut item = Vec::new();
    for it in list.token_trees_and_tokens() {
        match it {
            NodeOrToken::Token(token) if token.kind().is_trivia() || is_delimiter(list, token) => {}
            NodeOrToken::Token(token) if token.kind() == T![,] => items.push(std::mem::take(&mut item)),
            _ => item.push(it),
        }
    }
    items.push(item);
    if items.last().is_some_and(Vec::is_empty) {
        items.pop();
    }
    if !items.iter().all(|it| is_meta_item(it)) {
        // alloc: empty, allocates nothing
        return Vec::new();
    }
    items
        .iter()
        .filter_map(|item| match item.as_slice() {
            [NodeOrToken::Token(token)] if is_ident(token.kind()) => {
                let text = token.text();
                // alloc: as above, a feature's name
                Some((text.strip_prefix("r#").unwrap_or(text), token.text_range()))
            }
            _ => None,
        })
        // alloc: as above
        .collect()
}

/// Whether `token` is one of the delimiters of `list` itself, not a token in it.
fn is_delimiter(list: ast::TokenTree<'_>, token: ast::Token<'_>) -> bool {
    list.left_delimiter_token() == Some(token) || list.right_delimiter_token() == Some(token)
}

/// Whether rustc parses `item` as an item of a meta item list: a literal, or a path with no
/// arguments, a delimited list of them, or `= value`.
fn is_meta_item(item: &[NodeOrToken<ast::TokenTree<'_>, ast::Token<'_>>]) -> bool {
    match item {
        [] => false,
        [NodeOrToken::Token(token)] if token.kind().is_literal() || matches!(token.kind(), T![true] | T![false]) => true,
        _ => {
            let is_path = |it: &&NodeOrToken<ast::TokenTree<'_>, ast::Token<'_>>| {
                matches!(it, NodeOrToken::Token(it) if is_ident(it.kind()) || matches!(it.kind(), T![:] | T![::]))
            };
            let path = item.iter().take_while(is_path).count();
            let ends_in_ident = matches!(item[..path].last(), Some(NodeOrToken::Token(it)) if is_ident(it.kind()));
            if path == 0 || !ends_in_ident {
                return false;
            }
            match &item[path..] {
                [] | [NodeOrToken::Node(_)] => true,
                [NodeOrToken::Token(eq), value @ ..] => eq.kind() == T![=] && !value.is_empty(),
                _ => false,
            }
        }
    }
}

/// Whether rustc takes a token of this kind for an identifier in a path: an identifier, raw or
/// not, or one of the keywords a path may start with.
fn is_ident(kind: SyntaxKind) -> bool {
    matches!(kind, SyntaxKind::IDENT | T![self] | T![super] | T![crate] | T![Self])
}
