//! `rc_mutex`: the type `Rc<Mutex<_>>`. An `Rc` never crosses a thread, so the lock guards
//! against no one: a `RefCell` does that job on one thread, an `Arc<Mutex<_>>` across them.
//! AGENTS.md rule 9: one choice per need.
//!
//! Follows clippy's `clippy_lints/src/types/rc_mutex.rs` (MIT or Apache-2.0), read from
//! indexable-inc's fork at c2308e38 (nightly-2026-05-27), the nearest checkout to the pinned
//! nightly's (rust-lang/rust 12433c30). As in clippy: a written path type naming std's `Rc`
//! whose first generic argument names std's `Mutex`, in the places clippy's `types` pass looks
//! (signatures, fields, statics, constants, type aliases), run from [`super::box_collection`]'s
//! walk of them, what another crate can name left alone under `avoid-breaking-exported-api`;
//! that walk stops there, as clippy's does. Unlike clippy, `lock_api`'s `Mutex`
//! (`parking_lot`'s) counts too (without stopping the walk, which clippy's types don't know),
//! and the message names the lock.

use hir::PathResolution;
use syntax::view::{self as ast, AstNode as _};

use super::{
    Implemented,
    box_collection::{Std, generic_tys, std_of},
};
use crate::{blaze::facts, check::Cx};

pub(super) const LINT: Implemented = Implemented::RcMutex;

/// Reports `path`, a path type naming the std type `outer`, if it is `Rc<Mutex<_>>`; whether
/// it is with std's `Mutex` (clippy's `rc_mutex::check` returning true).
pub(super) fn check<'t>(cx: &mut Cx<'_, 't>, path: ast::PathType<'t>, outer: Std) -> bool {
    if outer != Std::Rc {
        return false;
    }
    let Some(arg) = generic_tys(path).next() else { return false };
    let std = std_of(cx, arg) == Some(Std::Mutex);
    let lock = match std {
        true => "std::sync::Mutex",
        false if lock_api_mutex(cx, arg) => "lock_api::Mutex",
        false => return false,
    };
    if !cx.on(LINT) {
        return std;
    }
    let Some(range) = cx.range(path.syntax()) else { return std };
    let span = cx.span(range);
    let message = format!("`{}` locks a `{lock}` an `Rc` keeps on one thread", path.syntax().text());
    cx.lint(LINT, path.syntax(), span, message)
        .help("on one thread, `Rc<RefCell<_>>`; shared across threads, `Arc<Mutex<_>>`");
    std
}

/// Whether `ty` is a path naming `lock_api`'s `Mutex`, by itself or through an alias
/// (`parking_lot::Mutex`).
fn lock_api_mutex<'t>(cx: &Cx<'_, 't>, ty: ast::Type<'t>) -> bool {
    let ast::Type::PathType(path) = ty else { return false };
    let Some(path) = path.path() else { return false };
    let adt = match cx.path(path) {
        Some(PathResolution::Def(hir::ModuleDef::Adt(adt))) => Some(adt),
        Some(PathResolution::Def(hir::ModuleDef::TypeAlias(alias))) => alias.ty(cx.db()).as_adt(),
        _ => None,
    };
    adt.is_some_and(|adt| facts::adt_is(cx.db(), adt, "lock_api", "Mutex"))
}

#[cfg(test)]
mod tests {
    use super::super::more_rules_tests::lint_only;

    const LINT: &str = "rc_mutex";
    const FOUND: &str = "an `Rc` keeps on one thread";

    #[test]
    fn an_rc_of_a_mutex_is_found_and_an_arc_of_one_is_not() {
        let found = lint_only(
            LINT,
            r#"
use std::{rc::Rc, sync::{Arc, Mutex}};
struct Shared { one: Rc<Mutex<u8>>, many: Arc<Mutex<u8>> }
fn f(_: Rc<Mutex<bool>>) {}
fn g(_: Rc<u8>) {}
pub fn exported(_: Rc<Mutex<u8>>) {}
"#,
        );
        assert_eq!(found.matches(FOUND).count(), 2, "{found}");
        assert!(found.contains("`Rc<Mutex<u8>>` locks a `std::sync::Mutex`"), "{found}");
    }

    #[test]
    fn parking_lots_mutex_through_a_renamed_import_and_a_type_a_macro_is_handed_are_found() {
        let found = lint_only(
            LINT,
            r#"
use std::rc::Rc as Shared;
use parking_lot::Mutex;
macro_rules! ty { ($t:ty) => { $t }; }
struct S { a: Shared<Mutex<u8>>, b: ty!(Shared<std::sync::Mutex<u8>>) }
"#,
        );
        assert!(found.matches(FOUND).count() >= 1, "{found}");
        assert!(found.contains("locks a `lock_api::Mutex`"), "{found}");
    }
}
