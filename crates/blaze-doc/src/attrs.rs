//! What an item's attributes say that the HIR does not keep: `#[deprecated]`'s `since` and
//! `note`, `#[doc(cfg(..))]`, the words of `#[doc(..)]` (`inline`, `no_inline`) and a crate's
//! `html_root_url`.

use syntax::{
    SyntaxKind, T,
    view::{self as ast, AstNode, AstToken, HasAttrs},
};

use crate::model::Deprecation;

/// The words directly inside `#[doc(..)]`: `inline`, `hidden`, `no_inline`.
pub(crate) fn doc_words(attr: ast::Attr<'_>) -> Vec<String> {
    let mut out = vec![];
    for meta in attr.skip_cfg_attrs() {
        let Some((name, tt)) = meta.as_simple_call() else { continue };
        if name != "doc" {
            continue;
        }
        for token in tt.syntax().children_with_tokens() {
            if let Some(token) = token.as_token()
                && token.kind() == SyntaxKind::IDENT
            {
                out.push(token.text().to_owned());
            }
        }
    }
    out
}

/// What `#[deprecated]` says, if the node has one.
pub(crate) fn deprecation<'t>(node: impl HasAttrs<'t>) -> Option<Deprecation> {
    for attr in node.attrs() {
        for meta in attr.skip_cfg_attrs() {
            match meta {
                ast::Meta::PathMeta(path) if path_is(path.path(), "deprecated") => {
                    return Some(Deprecation::default());
                }
                ast::Meta::KeyValueMeta(kv) if path_is(kv.path(), "deprecated") => {
                    let note = kv.expr().and_then(|it| string_of(it.syntax()));
                    return Some(Deprecation { since: None, note });
                }
                ast::Meta::TokenTreeMeta(tt) if path_is(tt.path(), "deprecated") => {
                    let mut out = Deprecation::default();
                    if let Some(tt) = tt.token_tree() {
                        for (key, value) in key_values(tt) {
                            match key.as_str() {
                                "since" => out.since = Some(value),
                                "note" => out.note = Some(value),
                                _ => {}
                            }
                        }
                    }
                    return Some(out);
                }
                _ => {}
            }
        }
    }
    None
}

/// `#[doc(cfg(..))]` as the condition rustdoc says the item is available on, written out.
pub(crate) fn doc_cfg<'t>(node: impl HasAttrs<'t>) -> Option<String> {
    for attr in node.attrs() {
        // `cfg_attr(docsrs, doc(cfg(..)))` is skipped: `docsrs` is not set when documenting.
        let Some(ast::Meta::TokenTreeMeta(meta)) = attr.meta() else { continue };
        if !path_is(meta.path(), "doc") {
            continue;
        }
        let Some(tt) = meta.token_tree() else { continue };
        let mut tokens = tt.syntax().children_with_tokens().peekable();
        while let Some(token) = tokens.next() {
            if token
                .as_token()
                .is_some_and(|it| it.kind() == SyntaxKind::IDENT && it.text() == "cfg")
                && let Some(inner) = tokens.peek().and_then(|it| it.as_node().copied())
                && let Some(inner) = ast::TokenTree::cast(inner)
            {
                return Some(crate::cfg::describe(inner));
            }
        }
    }
    None
}

/// `#![doc(html_root_url = "..")]` of a crate's root file, ending in `/`.
pub(crate) fn html_root_url(file: ast::SourceFile<'_>) -> Option<String> {
    for attr in file.attrs() {
        for meta in attr.skip_cfg_attrs() {
            let Some((name, tt)) = meta.as_simple_call() else { continue };
            if name != "doc" {
                continue;
            }
            if let Some((_, mut url)) =
                key_values(tt).into_iter().find(|(k, _)| k == "html_root_url")
            {
                if !url.ends_with('/') {
                    url.push('/');
                }
                return Some(url);
            }
        }
    }
    None
}

fn path_is(path: Option<ast::Path<'_>>, name: &str) -> bool {
    path.is_some_and(|it| it.syntax().text() == name)
}

fn string_of(node: ast::Node<'_>) -> Option<String> {
    node.descendants_with_tokens()
        .filter_map(|it| it.into_token())
        .find_map(ast::String::cast)
        .and_then(|it| it.value().ok().map(|it| it.into_owned()))
}

/// The `key = "value"` pairs of a token tree.
pub(crate) fn key_values(tt: ast::TokenTree<'_>) -> Vec<(String, String)> {
    let mut out = vec![];
    let (mut key, mut eq): (Option<ast::Token<'_>>, Option<ast::Token<'_>>) = (None, None);
    for token in tt
        .syntax()
        .children_with_tokens()
        .filter_map(|it| it.into_token())
        .filter(|it| !it.kind().is_trivia())
    {
        if let (Some(key), Some(eq)) = (key, eq)
            && key.kind() == SyntaxKind::IDENT
            && eq.kind() == T![=]
            && let Some(value) = ast::String::cast(token)
            && let Ok(value) = value.value()
        {
            out.push((key.text().to_owned(), value.into_owned()));
        }
        (key, eq) = (eq, Some(token));
    }
    out
}
