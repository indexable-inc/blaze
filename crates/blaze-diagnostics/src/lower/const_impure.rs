//! A `const fn` whose result at run time depends on more than its arguments: blaze's lint
//! `blaze::const_impure`, not rustc's (docs/fn-cache.md). rustc says nothing of it, so it is
//! given only in a crate that opted into cached functions, which is one that depends on the
//! `blaze` crate (`#[blaze::cache]` calls into it, as `hir_ty::fn_cache::uses_fn_cache`
//! decides); in any other crate the output is rustc's.
//!
//! hir runs the purity analysis only where `uses_fn_cache` holds, so a crate that has not opted
//! in gives no `ConstImpure` and this costs it nothing. Per diagnostic: one perfect-hash lookup
//! of the lint, one bit test and one memo read of the same query.

use crate::{
    cx::Cx,
    lints::{code_fact, registry},
    lower::{Found, Phase},
    model::Diag,
};

const LINT: &str = "blaze::const_impure";

pub(crate) fn lower(cx: &Cx<'_, '_>, d: &hir::ConstImpure) -> Vec<Found> {
    let lint = registry::lint(LINT).expect("`blaze::const_impure` is in the registry's blaze table");
    if !cx.lint_may_fire(registry::LintId::of(lint)) || !hir_ty::fn_cache::uses_fn_cache(cx.db, cx.krate.into()) {
        // alloc: none, an empty `Vec` allocates nothing.
        return Vec::new();
    }
    // clone: the message hir owns, once per diagnostic (the edge).
    let mut diag = Diag::lint(LINT, d.message.clone(), cx.span_of(d.node));
    code_fact(cx, &mut diag, d.node);
    // alloc: `lower`'s one `Vec` per diagnostic (lower/mod.rs's interface, the edge).
    vec![Found::lint(diag, Phase::Mir, d.node)]
}