//! `variant_size_differences` (allowed by default): an enum with a direct tag whose largest
//! variant, less the tag, is more than three times as large as the next largest, at that
//! variant. An enum whose layout fails (a generic one) is not linted, as rustc's `layout_of`
//! of its identity type fails.
//!
//! Follows `compiler/rustc_lint/src/types.rs` (`VariantSizeDifferences`) and
//! `compiler/rustc_lint/src/lints.rs` (`VariantSizeDifferencesDiag`) (MIT or Apache-2.0).

use blaze_items::Fact;
use syntax::view::{self as ast, AstNode as _};

use super::walk::Item;
use crate::{
    cx::{Cx, item_range_view},
    lower::{Found, Phase},
    model::Diag,
};

pub(super) fn item<'db>(cx: &Cx<'_, 'db>, item: &Item<'db>) -> Vec<Found> {
    let node = item.node;
    let ast::Item::Enum(it) = node.value else { return Vec::new() }; // alloc: empty, allocates nothing
    let Some(enum_) = cx.vsema.to_def2(node.with_value(it)) else { return Vec::new() }; // alloc: as above
    let db = cx.db;
    let Ok(layout) = enum_.layout(db) else { return Vec::new() }; // alloc: as above
    let Some((tag, sizes)) = layout.direct_tag_variant_sizes() else { return Vec::new() }; // alloc: as above
    let (largest, second, at) =
        sizes.map(|it| it.saturating_sub(tag)).enumerate().fold((0, 0, 0), |(l, s, li), (idx, size)| {
            if size > l {
                (size, l, idx)
            } else if size > s {
                (l, size, li)
            } else {
                (l, s, li)
            }
        });
    if largest <= second * 3 || second == 0 {
        return Vec::new(); // alloc: empty, allocates nothing
    }
    // alloc: the firing lint's message
    let message = format!("enum variant is more than three times larger ({largest} bytes) than the next largest");
    // The layout's variants are the enum's, in order: one without a source is hir's bug.
    let Some(variant) = enum_.variants_iter(db).nth(at).and_then(|it| cx.vsema.source(it)) else {
        let span = cx.span(node.map(|it| item_range_view(it.syntax())));
        // alloc: the error's message, on the error path
        let why = format!("blaze can't place `variant_size_differences`: the layout's variant {at} has no source");
        return vec![Found::new(Diag::error(None, why, span), Phase::Late)]; // alloc: the one error
    };
    let span = cx.span(variant.map(|it| item_range_view(it.syntax())));
    let mut diag = Diag::lint("variant_size_differences", message, span);
    if let Some((id, name)) = super::item_id(cx, item) {
        diag.fact(Fact::Layout(cx.key(id)), format!("the layout of `{name}`")); // alloc: the firing lint's fact
    }
    vec![Found::lint(diag, Phase::Late, item.ptr())] // alloc: the one lint that fires
}
