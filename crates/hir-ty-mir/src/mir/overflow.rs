//! Whether arithmetic panics when it overflows: what `-C overflow-checks` says for the crate
//! a body is written in, or for the crate that uses it.

use base_db::{Db, Crate};
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

/// Whether the crate `body` is written in keeps the overflow checks in what it keeps of the
/// body: it does when it has `-C overflow-checks`, or when it is not a crate of the build and
/// the body takes the setting of its callers. Whether the checks then run is up to the crate
/// that compiles the body.
pub fn body_written_checked(db: &Db, body: &MirBody<'_>) -> bool {
    match crate_checks_overflow(db, body.owner.module(db).krate(db)) {
        Some(written_with) => written_with,
        None => body.inherits_overflow_checks,
    }
}

/// Whether arithmetic of `body` panics when it overflows in a constant, where it is known:
/// it is not for a body that takes the setting of whichever crate uses it.
pub fn body_checks_overflow(db: &Db, body: &MirBody<'_>) -> Option<bool> {
    if !body_written_checked(db, body) {
        return Some(false);
    }
    crate_checks_overflow(db, body.owner.module(db).krate(db))
}
