//! `only_used_in_tests`: an exported function production code never calls or names, that tests
//! do. A helper written for its test and kept public: the test proves a function nothing runs,
//! and the function's API must be kept for no caller.
//!
//! Read off the use rows of the whole workspace ([`super::rows`]): calls and names (as a value,
//! a callback) by role, in every crate that can name the function; a crate's tests, benches
//! and examples and code under `#[cfg(test)]` are tests. It adds to `blaze::test_only`, which
//! finds the same of a private item within its crate by liveness, the exported functions, by
//! their uses across the workspace. Not found: a trait's function or an impl's of one (its
//! callers call the trait), `main`, an `extern` function, test code itself, a function with an
//! attribute (an `#[no_mangle]`, a `#[export_name]`, a registration a macro reads), and a
//! function no code uses at all (that is `blaze::dead`'s or an API's).
//!
//! No fix: move the function into the tests' module, or call it where it was meant to be.

use syntax::view::{self as ast, AstNode as _, HasName as _};

use super::{
    Lint, facts, fn_subject, has_attrs, judged_fn,
    rows::{Role, Way},
};
use crate::check::Cx;

const LINT: Lint = Lint::OnlyUsedInTests;

pub(super) fn check<'t>(cx: &mut Cx<'_, 't>, f: ast::Fn<'t>) {
    if !cx.on_blaze(LINT) || has_attrs(f) || !cx.is_exported(f.syntax()) {
        return;
    }
    let Some(func) = judged_fn(cx, f) else { return };
    let Some(subject) = fn_subject(cx, func) else { return };
    let seen = facts(cx, subject);
    let production = seen.get(Role::Plain, Way::Call)
        + seen.get(Role::Plain, Way::Named)
        + seen.get(Role::Stored, Way::Call)
        + seen.get(Role::Stored, Way::Named);
    let tests = seen.get(Role::Test, Way::Call) + seen.get(Role::Test, Way::Named);
    if production > 0 || tests == 0 {
        return;
    }
    let Some(name) = f.name() else { return };
    let Some(range) = cx.range(name.syntax()) else { return };
    let message = format!(
        "`fn {}` is used only by tests: no production code of the workspace calls it",
        name.text()
    );
    super::report(
        cx,
        LINT,
        f.syntax(),
        range,
        message,
        "a test of a function nothing runs proves nothing about the program; move it into the tests' \
         module, or call it where it was meant to be",
        None,
    );
}
