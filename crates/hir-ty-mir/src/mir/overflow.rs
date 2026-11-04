//! Whether arithmetic panics when it overflows: what `-C overflow-checks` says for the crate
//! a body is written in, or for the crate that uses it.
//!
//! rustc decides it in two places, and so does blaze:
//!
//! - Building the MIR, in the crate the body is written in
//!   (`rustc_mir_build/src/builder/mod.rs`, `Builder::new`): the overflow checks are kept
//!   where that crate has `-C overflow-checks` or the body has
//!   `#[rustc_inherit_overflow_checks]` ([`body_written_checked`]).
//! - Compiling the MIR to code, in the crate that compiles the instance
//!   (`rustc_codegen_ssa/src/mir/block.rs`, `codegen_assert_terminator`): a kept check whose
//!   `AssertKind::is_optional_overflow_check` holds runs only where that crate's
//!   `sess.overflow_checks()` says so, and `intrinsics::overflow_checks()` reads the same.
//!   That crate is the user's for an instance rustc compiles into each crate that uses it
//!   (`home::compiled_by_user`: a generic, a closure, an `#[inline]` function), else the
//!   body's own: the code generator compiles the instance for that crate, under its profile.

use base_db::{Crate, Db};
use hir_def::HasModule;

use crate::mir::MirBody;

/// Whether the crate is built with `-C overflow-checks`, when cargo builds it. The standard
/// library is built already, without.
pub fn crate_checks_overflow(db: &Db, krate: Crate) -> Option<bool> {
    if krate.data(db).origin.is_lang() {
        return None;
    }
    let flag = cfg::CfgAtom::Flag(intern::sym::overflow_checks);
    Some(krate.cfg_options(db).check_atom(&flag))
}

/// Whether the crate `body` is written in keeps the overflow checks in its MIR, as rustc's
/// `Builder::new` decides (`rustc_mir_build/src/builder/mod.rs`): when the body has
/// `#[rustc_inherit_overflow_checks]`, or its crate has `-C overflow-checks` (none for the
/// standard library's, built without). Whether the checks then run is up to the crate that
/// compiles the body.
pub fn body_written_checked(db: &Db, body: &MirBody<'_>) -> bool {
    body.inherits_overflow_checks
        || crate_checks_overflow(db, body.owner.module(db).krate(db)).unwrap_or(false)
}

/// Whether arithmetic of `body` panics when it overflows in a constant, where it is known:
/// it is not for a body that takes the setting of whichever crate uses it.
pub fn body_checks_overflow(db: &Db, body: &MirBody<'_>) -> Option<bool> {
    if !body_written_checked(db, body) {
        return Some(false);
    }
    crate_checks_overflow(db, body.owner.module(db).krate(db))
}
