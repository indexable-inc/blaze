//! The attribute walk the stability lint reads its own attributes with (`#[macro_use]`,
//! `#[derive]`, `#[deprecated]`'s place, the feature lists of `#[allow_internal_unstable]` and
//! `#[unstable_feature_bound]`); what `#[deprecated]`, `#[stable]` and `#[unstable]` say is
//! hir-def's (`lookup.rs`).
//!
//! The attributes are visited in place (`expand_cfg_attr` over the syntax), nothing collected:
//! a node with none costs one look at its children and allocates nothing.
//!
//! Follows `compiler/rustc_attr_parsing/src/attributes/allow_unstable.rs` (MIT or Apache-2.0).

use std::ops::ControlFlow;

use cfg::CfgOptions;
use hir_expand::attrs::expand_cfg_attr_view;
use intern::Symbol;
use syntax::{
    NodeOrToken, SyntaxKind, TextRange,
    view::{self as ast, AstNode as _, Node},
};

/// Calls `f` with each attribute of `node` once `cfg_attr` is expanded with `cfg`, outer then
/// inner, with the attribute it is written in, until `f` breaks.
pub(super) fn each<'t, T>(
    node: Node<'t>,
    cfg: &CfgOptions,
    f: impl FnMut(ast::Meta<'t>, ast::Attr<'t>) -> ControlFlow<T>,
) -> Option<T> {
    let owner = ast::AnyHasAttrs::cast(node)?;
    expand_cfg_attr_view(ast::attrs_including_inner(&owner), || cfg, f)
}

/// Whether `meta` is the attribute `name`, a path of one segment: a length check and a
/// compare of the token's bytes, no hashing.
pub(super) fn is(meta: &ast::Meta<'_>, name: &str) -> bool {
    meta.path().and_then(|it| it.as_single_name_ref()).is_some_and(|it| it.text() == name)
}

/// Where the first `#[stable(..)]` or `#[unstable(..)]` of `node` is written: the whole
/// `#[...]`, or what a `cfg_attr` expands to (`expand_cfg_attr_item` gives the attribute its
/// item's span).
pub(super) fn stability_range(node: Node<'_>, cfg: &CfgOptions) -> Option<TextRange> {
    each(node, cfg, |meta, written| {
        if !(is(&meta, "stable") || is(&meta, "unstable")) || !matches!(meta, ast::Meta::TokenTreeMeta(_)) {
            return ControlFlow::Continue(());
        }
        ControlFlow::Break(match meta.syntax().parent() {
            Some(parent) if parent == written.syntax() => written.syntax().text_range(),
            _ => meta.syntax().text_range(),
        })
    })
}

/// Whether `#[allow_internal_unstable(..)]` on `node` lets a macro's expansion use `feature`.
pub(super) fn allows_internal_unstable(node: Node<'_>, cfg: &CfgOptions, feature: &Symbol) -> bool {
    lists_feature(node, cfg, "allow_internal_unstable", feature)
}

/// Whether an `#[unstable_feature_bound(..)]` on `node` names `feature`.
pub(super) fn unstable_feature_bound(node: Node<'_>, cfg: &CfgOptions, feature: &Symbol) -> bool {
    lists_feature(node, cfg, "unstable_feature_bound", feature)
}

/// Whether an attribute `name(..)` on `node` lists `feature` among its identifiers.
fn lists_feature(node: Node<'_>, cfg: &CfgOptions, name: &str, feature: &Symbol) -> bool {
    let feature = feature.as_str();
    each(node, cfg, |meta, _| {
        if is(&meta, name)
            && let ast::Meta::TokenTreeMeta(meta) = &meta
            && let Some(tt) = meta.token_tree()
            && tt.syntax().children_with_tokens().any(|it| {
                matches!(it, NodeOrToken::Token(token) if token.kind() == SyntaxKind::IDENT
                    && token.text().trim_start_matches("r#") == feature)
            })
        {
            return ControlFlow::Break(());
        }
        ControlFlow::Continue(())
    })
    .is_some()
}
