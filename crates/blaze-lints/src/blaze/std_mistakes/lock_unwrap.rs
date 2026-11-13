//! `lock_unwrap`: std's `Mutex::lock()`, `RwLock::read()` or `RwLock::write()` then
//! `.unwrap()` or `.expect(..)`. std's locks poison: a thread that panics holding the guard
//! marks the lock, and from then every `lock()` returns `Err`. Unwrapped, that `Err` panics the
//! next thread too, and the next: one bug becomes a panic of every thread that touches the
//! lock, far from the cause, with the first panic's message the only one that says what failed.
//!
//! Found: an `unwrap` or `expect` of `Result` (resolved to its own impl) whose receiver is a
//! call resolved to std's inherent `Mutex::lock`, `RwLock::read` or `RwLock::write`. Outside
//! test code, where a panic is the test's failure.
//!
//! What the ban misses, only: a workspace whose `clippy.toml` bans `std::sync::Mutex` or
//! `RwLock` (blaze's own does, for `blaze_core::lock`) has every use of the type reported by
//! that ban, so a crate whose configuration has such an entry on is left to it.
//!
//! The right forms: a lock that does not poison (`parking_lot::Mutex`, `blaze_core::lock::Lock`,
//! std's `nonpoison` locks), or the `PoisonError` handled: `unwrap_or_else(PoisonError::into_inner)`
//! where the data is whole after any panic inside the guard, a typed error naming the lock where
//! it may not be. No fix: which is right depends on what the guarded code can leave half done.

use syntax::view::{self as ast, AstNode as _};

use super::{in_test, inherent_adt, inherent_of, report, strip, unwrap_of};
use crate::{blaze::Lint, check::Cx, lints::disallowed};

const LINT: Lint = Lint::LockUnwrap;

pub(super) fn check<'t>(cx: &mut Cx<'_, 't>, node: ast::Node<'t>) {
    if !cx.on_blaze(LINT) {
        return;
    }
    let Some(unwrap) = ast::MethodCallExpr::cast(node) else { return };
    let Some(ast::Expr::MethodCallExpr(lock)) = unwrap.receiver().map(strip) else { return };
    if lock.name_ref().is_none_or(|it| !matches!(it.text(), "lock" | "read" | "write")) {
        return;
    }
    let Some(how) = unwrap_of(cx, unwrap) else { return };
    let Some(f) = cx.method(lock) else { return };
    let name = f.name(cx.db());
    let of = match name.as_str() {
        "lock" if inherent_of(cx, f, &["std", "sync", "Mutex"]) => "Mutex",
        "read" | "write" if inherent_of(cx, f, &["std", "sync", "RwLock"]) => "RwLock",
        _ => return,
    };
    let Some(adt) = inherent_adt(cx, f) else { return };
    if disallowed::banned(cx, hir::ModuleDef::Adt(adt)) || in_test(cx, node) {
        return;
    }
    let Some(range) = cx.range(node) else { return };
    report(
        cx,
        LINT,
        node,
        range,
        format!(
            "`{of}::{}(..).{how}(..)` panics once any thread panicked holding this lock, and so does every thread after it",
            name.as_str()
        ),
        "std's locks poison: the first panic inside the guard makes every later `lock()` an `Err`, and unwrapping it spreads the panic to each thread that takes the lock",
        None,
    );
}
