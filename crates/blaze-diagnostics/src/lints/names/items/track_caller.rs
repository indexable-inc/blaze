//! `ungated_async_fn_track_caller`: `#[track_caller]` on an `async fn` with a body, without
//! `#![feature(async_fn_track_caller)]`, where it has no effect.
//!
//! Follows `compiler/rustc_lint/src/builtin.rs` (`UngatedAsyncFnTrackCaller`) and
//! `compiler/rustc_lint/src/lints.rs` (`BuiltinUngatedAsyncFnTrackCaller`, whose notes are
//! `add_feature_diagnostics`') (MIT or Apache-2.0).

use blaze_items::Fact;
use syntax::view::{self as ast, AstNode};

use super::walk::{self, Item};
use crate::{
    cx::{Cx, item_range_view},
    lower::{Found, Phase, mutable_ref_binding::BUILT_ON},
    model::Diag,
};

const FEATURE: &str = "async_fn_track_caller";

/// rustc's `check_fn` of `item`, given whether the crate enables the feature.
pub(super) fn item<'db>(cx: &Cx<'_, 'db>, item: &Item<'db>, enabled: bool) -> Option<Found> {
    let ast::Item::Fn(f) = item.node.value else { return None };
    if enabled || f.async_token().is_none() || f.body().is_none() {
        return None;
    }
    let (_, attr) = walk::attr(cx, f, "track_caller")?;
    let file = item.node.file_id;
    let span = |range| cx.span(hir::InFile::new(file, range));
    let mut diag = Diag::lint("ungated_async_fn_track_caller", "`#[track_caller]` on async functions is a no-op", span(attr));
    diag.span_label(span(item_range_view(f.syntax())), "this function will not propagate the caller location");
    if let Some(issue) = crate::lints::syntax::attrs::features::table::issue(FEATURE) {
        // alloc: a lint that fires
        diag.note(format!("see issue #{issue} <https://github.com/rust-lang/rust/issues/{issue}> for more information"));
    }
    diag.help(format!("add `#![feature({FEATURE})]` to the crate attributes to enable")); // alloc: a lint that fires
    // alloc: a lint that fires
    diag.note(format!("this compiler was built on {BUILT_ON}; consider upgrading it if it is out of date"));
    if let Some((id, name)) = super::item_id(cx, item) {
        diag.fact(Fact::Signature(cx.key(id)), format!("the attributes of `{name}`")); // alloc: a lint that fires
    }
    Some(Found::lint(diag, Phase::Late, item.ptr()))
}
