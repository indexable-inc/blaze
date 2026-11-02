//! `unreachable_pub`: an item, a foreign item or an item of an inherent impl written `pub`
//! that no other crate can reach, with the visibility it has in effect as the suggestion:
//! `pub(super)` when that is its module's parent (not the crate's root), else `pub(crate)`.
//! A `use` is linted at each of its leaves, as rustc's HIR splits it; fields never.
//!
//! The effective visibility of a `use`, an `extern crate` and an item in a body is its
//! modules': a `pub use` another `use` re-exports is taken as its module leaves it.
//!
//! Follows `compiler/rustc_lint/src/builtin.rs` (`UnreachablePub`) (MIT or Apache-2.0).

use syntax::view::{self as ast, AstNode, HasVisibility};

use super::{
    def_span, visibility,
    walk::{Container, Item},
};
use crate::lints::names::effective::Effective;
use crate::{
    cx::Cx,
    lower::{Found, Phase},
    model::{Applicability, Diag},
};

pub(super) fn item<'db>(cx: &Cx<'_, 'db>, item: &Item<'db>, effective: &Effective) -> Vec<Found> {
    let node = item.node.value;
    let Some(vis) = public(node) else { return Vec::new() }; // alloc: empty, no allocation
    let exportable = match item.container {
        Container::Module | Container::Extern(_) => true,
        Container::Impl(imp) if imp.value.trait_().is_none() => false,
        Container::Impl(_) | Container::Trait(_) => return Vec::new(), // alloc: empty, no allocation
    };
    // Where its modules restrict it: a `use`'s reachability, else read only for a lint.
    let (reachable, restricted) = match node {
        ast::Item::Use(_) | ast::Item::ExternCrate(_) => {
            let restricted = visibility::restricted_to(cx, item);
            (item.in_body.is_none() && restricted.is_none(), Some(restricted))
        }
        _ => match visibility::def(cx, item) {
            Some(def) => (effective.is_reachable(def), None),
            None => return Vec::new(), // alloc: empty, no allocation
        },
    };
    if reachable {
        return Vec::new(); // alloc: empty, no allocation
    }
    let restricted = restricted.unwrap_or_else(|| visibility::restricted_to(cx, item));
    let grandparent = item.module.parent(cx.db);
    let new_vis = match restricted {
        Some(it) if item.in_body.is_none() && Some(it) == grandparent && !it.is_crate_root(cx.db) => "pub(super)",
        _ => "pub(crate)",
    };
    let vis_span = cx.span(item.node.with_value(vis.syntax().text_range()));
    let applicability = match vis_span.expansion.is_some() {
        true => Applicability::MaybeIncorrect,
        false => Applicability::MachineApplicable,
    };
    // alloc: the spans of a lint that fires, one per `use` leaf
    let mut spans = Vec::new();
    match node {
        ast::Item::Use(it) => it.use_tree().into_iter().for_each(|it| leaves(it, &mut spans)),
        _ => spans.push(def_span(node.syntax())),
    }
    let at = item.ptr();
    spans
        .into_iter()
        .map(|range| {
            let mut diag = Diag::lint("unreachable_pub", "unreachable `pub` item", cx.span(item.node.with_value(range)));
            // clone: the span of a lint that fires, once per leaf
            diag.suggestion(vis_span.clone(), "consider restricting its visibility", new_vis, applicability);
            if exportable {
                diag.help("or consider exporting it for use by other crates");
            }
            crate::lints::code_fact(cx, &mut diag, at);
            Found::lint(diag, Phase::Late, at)
        })
        .collect() // alloc: the lints that fire, returned
}

/// The visibility of an item written plain `pub`.
fn public(node: ast::Item<'_>) -> Option<ast::Visibility<'_>> {
    let vis = match node {
        ast::Item::Const(it) => it.visibility(),
        ast::Item::Enum(it) => it.visibility(),
        ast::Item::ExternCrate(it) => it.visibility(),
        ast::Item::Fn(it) => it.visibility(),
        ast::Item::MacroDef(it) => it.visibility(),
        ast::Item::Module(it) => it.visibility(),
        ast::Item::Static(it) => it.visibility(),
        ast::Item::Struct(it) => it.visibility(),
        ast::Item::Trait(it) => it.visibility(),
        ast::Item::TypeAlias(it) => it.visibility(),
        ast::Item::Union(it) => it.visibility(),
        ast::Item::Use(it) => it.visibility(),
        _ => None,
    }?;
    (vis.pub_token().is_some() && vis.visibility_inner().is_none()).then_some(vis)
}

/// The ranges of the leaves of a use tree, each one of HIR's `use` items, appended to `out`.
fn leaves(tree: ast::UseTree<'_>, out: &mut Vec<syntax::TextRange>) {
    match tree.use_tree_list() {
        Some(list) => list.use_trees().for_each(|it| leaves(it, out)),
        None => out.push(tree.syntax().text_range()),
    }
}