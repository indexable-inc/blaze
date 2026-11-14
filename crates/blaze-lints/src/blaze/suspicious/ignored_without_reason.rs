//! `ignored_without_reason`: `#[ignore]` on a test with no reason (`#[ignore = ".."]`), also
//! inside a `cfg_attr`, and `#[ignore = ""]`. An ignored test is a check the suite no longer
//! runs; without a reason nobody can tell whether it is slow, broken, waiting on something or
//! forgotten, and `cargo test -- --ignored` prints nothing to say. The reason is shown next to
//! the test's name in every run. No fix: the reason is the author's to write.

use syntax::{
    SyntaxKind,
    view::{self as ast, AstNode as _, HasName as _},
};

use super::super::{Lint, std_mistakes::report};
use crate::check::Cx;

const LINT: Lint = Lint::IgnoredWithoutReason;

pub(super) fn check<'t>(cx: &mut Cx<'_, 't>, node: ast::Node<'t>) {
    if node.kind() != SyntaxKind::ATTR || !cx.on_blaze(LINT) {
        return;
    }
    let Some(attr) = ast::Attr::cast(node) else { return };
    let Some(f) = node.parent().and_then(ast::Fn::cast) else { return };
    let Some(meta) = attr.meta() else { return };
    let mut bare = None;
    match meta {
        ast::Meta::CfgAttrMeta(it) => {
            bare = it.metas().find(|it| unexplained(cx, *it));
        }
        it if unexplained(cx, it) => bare = Some(it),
        _ => {}
    }
    let Some(meta) = bare else { return };
    let Some(range) = cx.range(meta.syntax()) else { return };
    let name = f.name().map_or_else(|| "this test".to_owned(), |it| format!("`{}`", it.text()));
    let message = format!(
        "{name} is ignored with no reason: every run skips it and nobody can tell whether it is \
         slow, broken or waiting on something"
    );
    let note = "write why and what would let it run: `#[ignore = \"needs a network; run with \
                --ignored on a machine that has one\"]`";
    report(cx, LINT, node, range, message, note, None);
}

/// Whether `meta` is `ignore` with no reason: bare, or `= ""`.
fn unexplained(cx: &Cx<'_, '_>, meta: ast::Meta<'_>) -> bool {
    if meta.simple_name() != Some("ignore") {
        return false;
    }
    match meta {
        ast::Meta::PathMeta(_) => true,
        ast::Meta::KeyValueMeta(it) => it.expr().is_some_and(|expr| {
            cx.range(expr.syntax()).is_some_and(|range| {
                let text = cx.snippet(range);
                text.trim_matches('"').trim().is_empty()
            })
        }),
        _ => false,
    }
}
