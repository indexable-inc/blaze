//! `dead_code` and `dead_code_pub_in_binary`: rustc's dead-code pass. The definitions of the
//! crate (`defs`), what is reachable from outside it (`reach`, read from the crate's effective
//! visibilities), where liveness starts (`seed`),
//! what is live from there (`mark`, with `res` for what a name resolves to), and what is
//! reported dead (`report`), each at the level of `dead_code` where it is (`level`).
//!
//! Where the front end can't say what rustc's pass would (a `global_asm!` naming a symbol, a
//! `pub macro` whose expansion reaches private items), nothing is linted.
//!
//! The crate's syntax is walked once (`defs`), numbering every definition; each later part
//! reads that numbering, and what each asks of a definition (its level, its attributes, what
//! its impl is of) is computed once.
//!
//! Follows `compiler/rustc_passes/src/dead.rs` (`live_symbols_and_ignored_derived_traits`,
//! `check_mod_deathness`) (MIT or Apache-2.0).

mod defs;
mod level;
mod mark;
mod reach;
mod report;
mod res;
mod seed;

use super::effective::Effective;
use crate::{
    cx::Cx,
    lints::registry::{LintId, LintSet},
    lower::Found,
};

pub(super) fn lints(cx: &Cx<'_, '_>, effective: &Effective) -> Vec<Found> {
    const GIVES: LintSet = LintSet::of(&[LintId::DEAD_CODE, LintId::named("dead_code_pub_in_binary")]);
    if !cx.any_may_fire(&GIVES) {
        return Vec::new(); // alloc: empty, allocates nothing
    }
    let defs = defs::Defs::of(cx);
    if defs.unported {
        return Vec::new(); // alloc: empty, allocates nothing
    }
    let levels = level::Levels::new();
    let reach = reach::Reach::of(cx, &defs, effective);
    let k = seed::Crate::new(cx, &defs, &levels, &reach);
    let seeds = seed::seeds(&k);
    let (liveness, mut found) = mark::live(&k, seeds);
    found.extend(report::report(&k, &liveness));
    found
}
