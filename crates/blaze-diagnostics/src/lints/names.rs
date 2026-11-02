//! What rustc lints of names and items: name resolution's lints, the stability checks, the
//! dead-code pass and the late lint passes over items, each found as rustc's pass finds it.
//!
//! - `imports`: what `rustc_resolve` lints (`unused_imports`, `unused_macros`,
//!   `unused_qualifications`, `ambiguous_glob_imports`, ..).
//! - `stability`: `deprecated` and the other stability lints (`rustc_middle/src/middle/
//!   stability.rs`, `rustc_passes/src/stability.rs`, typeck's method calls).
//! - `dead`: `dead_code` (`rustc_passes/src/dead.rs`).
//! - `items`: the lints of items `rustc_hir_analysis` gives, and `rustc_lint`'s late passes
//!   over items (`non_upper_case_globals`, `missing_docs`, `improper_ctypes`, ..).
//!
//! `effective` gives the crate's effective visibilities, computed once for `dead` and `items`.
//!
//! Follows `compiler/rustc_interface/src/passes.rs` (`run_required_analyses`, `analysis`,
//! `misc_checking_3`) for when each is given (MIT or Apache-2.0).

mod dead;
mod effective;
mod imports;
mod items;
mod stability;

pub(crate) use self::{
    effective::{Def as EffectiveDef, Effective},
    items::{Walk as ItemWalk, def_span},
};
use crate::{cx::Cx, lower::Found};

/// What name resolution and the stability checks lint, given whatever else failed.
pub(crate) fn before_abort(cx: &Cx<'_, '_>) -> Vec<Found> {
    let mut found = imports::lints(cx);
    found.extend(stability::lints(cx));
    found.extend(items::before_abort(cx));
    found
}

/// What rustc's dead-code pass and its late lint passes of items lint, run only when nothing
/// failed, in `misc_checking_3`'s order: private types in public interfaces, deathness, the
/// late lint passes, then the extern declarations that clash.
pub(crate) fn after_abort(cx: &Cx<'_, '_>) -> Vec<Found> {
    let walk = cx.items();
    let effective = cx.effective();
    let mut found = items::private_in_public(cx, walk, effective);
    found.extend(dead::lints(cx, effective));
    found.extend(items::after_abort(cx, walk, effective));
    found.extend(items::clashing_extern_declarations(cx, walk));
    found
}

#[cfg(test)]
mod tests {
    use blaze_alloc_count::assert_at_most;

    use crate::lints::{quiet_crate, with_cx};

    /// Under `-A dead_code` the dead-code pass is not run: nothing is numbered, at any size.
    #[test]
    fn dead_code_allowed_everywhere_costs_nothing() {
        for fns in [4, 400] {
            with_cx(&quiet_crate(fns), &["-A", "dead-code"], |cx| {
                let effective = super::effective::Effective::of(cx);
                cx.lint_may_fire(crate::lints::registry::LintId::DEAD_CODE);
                let found = assert_at_most(0, "dead_code, allowed", || super::dead::lints(cx, &effective));
                assert!(found.is_empty());
            });
        }
    }

    /// Under `--cap-lints allow` no item lint before the abort may fire: the items are not
    /// walked, at any size.
    #[test]
    fn item_lints_capped_cost_nothing() {
        for fns in [4, 400] {
            with_cx(&quiet_crate(fns), &["--cap-lints", "allow"], |cx| {
                cx.lint_may_fire(crate::lints::registry::LintId::DEAD_CODE);
                let found = assert_at_most(0, "the item lints, capped", || super::items::before_abort(cx));
                assert!(found.is_empty());
            });
        }
    }
}
