//! E0046: a trait impl that leaves an item to a default body the trait's crate gates behind an
//! unstable library feature its crate does not enable (hir's `TraitImplUnstableDefaultBody`).
//!
//! Follows rustc's `default_body_is_unstable` (`compiler/rustc_hir_analysis/src/check/mod.rs`),
//! the struct `MissingTraitItemUnstable` of `compiler/rustc_hir_analysis/src/diagnostics.rs`
//! and `add_feature_diagnostics_for_issue` (MIT or Apache-2.0).

use hir::InFile;
use syntax::view::AstNode as _;

use crate::{
    checks::features::{allows_unstable, lowered_nightly},
    cx::{Cx, item_range_view as item_range},
    lower::{
        Found, Phase,
        incoherent_impl::{ImplAt, unresolved},
        mutable_ref_binding::BUILT_ON,
    },
    model::Diag,
};

pub(crate) fn lower(cx: &Cx<'_, '_>, d: &hir::TraitImplUnstableDefaultBody) -> Vec<Found> {
    let Some(at) = ImplAt::of_ptr(cx, d.file_id, d.impl_) else {
        return unresolved(cx, d.file_id, d.impl_, "an unstable default body error");
    };
    let feature = d.feature.as_str();
    // rustc's `eval_default_body_stability` at the impl's span with its body: a macro allowed
    // the feature may write the impl.
    let range = item_range(at.node.value.syntax());
    if allows_unstable(cx, at.node.file_id, range, feature) {
        // alloc: empty, allocates nothing
        return Vec::new();
    }
    let item = d.item.as_str();
    let span = cx.span(InFile::new(at.node.file_id, range));
    // alloc: the error's text, for an error hir found
    let mut diag = Diag::error(
        Some("E0046"),
        format!("not all trait items implemented, missing: `{item}`"),
        span,
    );
    // alloc: the error's text, for an error hir found
    diag.note(format!("default implementation of `{item}` is unstable"));
    diag.note(match d.reason.to_opt_reason() {
        // alloc: the error's text, for an error hir found
        Some(reason) => format!("use of unstable library feature `{feature}`: {reason}"),
        // alloc: the error's text, for an error hir found
        None => format!("use of unstable library feature `{feature}`"),
    });
    if let Some(issue) = d.issue {
        // alloc: the error's text, for an error hir found
        diag.note(format!("see issue #{issue} <https://github.com/rust-lang/rust/issues/{issue}> for more information"));
    }
    if lowered_nightly(cx) {
        // alloc: the error's text, for an error hir found
        diag.help(format!("add `#![feature({feature})]` to the crate attributes to enable"));
        // alloc: the error's text, for an error hir found
        diag.note(format!(
            "this compiler was built on {BUILT_ON}; consider upgrading it if it is out of date"
        ));
    }
    diag.fact(at.coherence(cx), "the items of the impl");
    // alloc: the error's parts, for an error hir found
    vec![Found::new(diag, Phase::Items)]
}
