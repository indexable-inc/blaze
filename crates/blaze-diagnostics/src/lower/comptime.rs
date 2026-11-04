//! hir's `ComptimeUnsupported`: `#![feature(blaze_comptime)]` syntax (`for const`, `x.[f]`, a
//! bounded `for<F: ..>` binder; `docs/v2/plan/design/comptime.md`), either in a crate without
//! the feature, or with it where blaze's type checker does not check the form yet. blaze's own
//! extension, so no rustc text to follow.

use crate::{
    cx::Cx,
    lints::code_fact,
    lower::{Found, Phase},
    model::Diag,
};

pub(crate) fn lower(cx: &Cx<'_, '_>, d: &hir::ComptimeUnsupported) -> Vec<Found> {
    let form = d.form.syntax();
    // alloc: the message, for an error hir found
    let message = match d.feature_enabled {
        false => format!(
            "{form} is blaze's compile-time reflection, which needs `#![feature(blaze_comptime)]`"
        ),
        true => format!(
            "{form} (`#![feature(blaze_comptime)]`) is not yet supported by blaze's type checker"
        ),
    };
    let mut diag = Diag::error(None, message, cx.span_of(d.node));
    if !d.feature_enabled {
        diag.help("add `#![feature(blaze_comptime)]` to the crate root");
    }
    code_fact(cx, &mut diag, d.node);
    // alloc: the error's parts, for an error hir found
    vec![Found::new(diag, Phase::Lower)]
}
