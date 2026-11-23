//! A name missing because the call that would have written it FAILED reports that call, never
//! "not found" (rules 19 and 54; failed_call.rs). The Reference, "Macros By Example",
//! "Transcribing": "When a macro is invoked, the macro expander looks up macro invocations by
//! name, and tries each macro rule in turn. ... If no rule matches, an error is reported": the
//! call is an error, so what it would have declared is unknown because of it. And D104: a
//! blaze-edition binary's root `fn main` is the input of the implicit `#[::std::main]`
//! (task/lower.rs `IMPLICIT_MAIN`); when no arm of `std::main` takes that `fn main`, the lookup
//! of `main` must say so, naming the call, the item and the matcher's reason.
//!
//! CONTROL (rule 8): with resolve.rs `expand_decls` as before (an expansion holding an error
//! diagnostic noted as `ExpandNote::Diagnostic` and kept as an EMPTY expansion), the first two
//! tests fail with "got Err(PathError { .. why: NotFound })".

use std::format;

use blaze_expand::Diagnostic;
use blaze_own::Alloc;
use blaze_surface::{Namespace, PathKind, RefusedWhy};
use blaze_unit_graph::Implicit;

use super::entry::{Texts, input};
use crate::failed_call::FailedCall;
use crate::graph::Sysroot;
use crate::names::{PathError, Why, Written};
use crate::resolve::{CrateInput, Names, resolve_crates};
use crate::task::Serial;

/// `std` whose `main` takes only a `main` WITHOUT parameters (the shape the stale pin's bstd
/// lacked the other way round: main-not-found-1.md).
const STD: &str = "#![no_core]
#[macro_export]
macro_rules! main {
    attr() ($vis:vis fn main() $body:block) => {
        pub fn granted_entry() {}
    };
}
";

/// A root `fn main` with a parameter: no arm of `std::main` takes it.
const USER: &str = "#![no_core]
fn main(x: u8) {}
";

/// A macro that writes a fn from `fn NAME`, called with `struct foo`: no rule matched, so the
/// `foo` it might have written is not there.
const MAKER: &str = "#![no_core]
macro_rules! make {
    (fn $n:ident) => { pub fn $n() {} };
}
make!(struct foo);
pub fn here() {}
";

static TEXTS: Texts = Texts(&[&[("/user/main.rs", USER)], &[("/std/lib.rs", STD)], &[("/maker/lib.rs", MAKER)]]);

const DEPS: &[(&str, usize)] = &[("std", 1)];

fn names() -> Names<'static> {
    let mut user = input("user", "/user/main.rs", DEPS);
    user.implicit = Implicit { entry: true, ..user.implicit };
    let crates: [CrateInput<'static>; 3] = [user, input("std", "/std/lib.rs", &[]), input("maker", "/maker/lib.rs", &[])];
    resolve_crates(&crates, Sysroot::default(), &TEXTS, &Serial, super::SyncFrame::leaked()).unwrap_or_else(|e| panic!("resolve_crates: {e}"))
}

fn lookup(names: &Names<'static>, krate: usize, name: &'static str) -> Result<(), PathError> {
    let alloc: Alloc<'static> = blaze_own::testing::heap().alloc();
    let segments: &'static [&'static str] = match name {
        "main" => &["main"],
        "foo" => &["foo"],
        _ => &["nothing_here"],
    };
    names.path(names.root(krate), Namespace::Values, Written { kind: PathKind::Plain, segments }, alloc).map(|_| ())
}

/// The root's `fn main(x: u8)` matches no arm of `std::main`: the lookup of `main` is unknown
/// behind the implicit call, and its failure names `::std::main`, `fn main` and the matcher's
/// reason (the closest arm wanted `)` and found `x`). Passing prints
/// "test tests::failed_call::a_main_no_arm_of_std_main_takes_names_the_implicit_call ... ok".
#[test]
fn a_main_no_arm_of_std_main_takes_names_the_implicit_call() {
    let names = names();
    let error = match lookup(&names, 0, "main") {
        Err(e @ PathError { at: 0, why: Why::Refused { why: RefusedWhy::Expansion, .. }, .. }) => e,
        other => panic!("`main` must be unknown behind the failed implicit `::std::main`; got {other:?}"),
    };
    let failed = FailedCall::of(&names, &error, "main").unwrap_or_else(|| panic!("the refusal joins its failed call: {error}"));
    assert!(failed.implicit, "the call is the implicit one: {failed:?}");
    assert_eq!(failed.path, "::std::main");
    let Diagnostic::NoRuleMatched { rules: 1, token: Some(token), .. } = failed.failed.diagnostic else {
        panic!("the matcher's own reason, no rule of one matched: {failed:?}")
    };
    assert!(failed.failed.text(token).contains('x'), "the closest arm stopped at the parameter `x`: {}", failed.failed.text(token));
    let shown = format!("{failed}");
    assert!(shown.contains("`fn main`") && shown.contains("`#[::std::main]`") && shown.contains("no rule of the macro's 1 matched"), "{shown}");
}

/// A user's `make!(struct foo)` matches no rule: `foo` is unknown behind that call, which the
/// error names. Passing prints "test tests::failed_call::a_name_a_failed_call_could_have_written_names_the_call ... ok".
#[test]
fn a_name_a_failed_call_could_have_written_names_the_call() {
    let names = names();
    let error = match lookup(&names, 2, "foo") {
        Err(e @ PathError { why: Why::Refused { why: RefusedWhy::Expansion, .. }, .. }) => e,
        other => panic!("`foo` must be unknown behind the failed `make!`; got {other:?}"),
    };
    let failed = FailedCall::of(&names, &error, "foo").unwrap_or_else(|| panic!("the refusal joins its failed call: {error}"));
    assert!(!failed.implicit);
    assert_eq!(failed.path, "make");
    let shown = format!("{failed}");
    assert!(shown.contains("`make`") && shown.contains("`foo`") && shown.contains("no rule"), "{shown}");
}

/// CONTROL: a name missing from a module that holds no failed call is plain "not found", and
/// joins no failed call. Passing prints
/// "test tests::failed_call::a_name_with_no_failed_call_is_not_found ... ok".
#[test]
fn a_name_with_no_failed_call_is_not_found() {
    let names = names();
    match lookup(&names, 1, "nothing_here") {
        Err(e @ PathError { why: Why::NotFound, .. }) => assert_eq!(FailedCall::of(&names, &e, "nothing_here"), None),
        other => panic!("`std::nothing_here` is not found, behind no call; got {other:?}"),
    }
}
