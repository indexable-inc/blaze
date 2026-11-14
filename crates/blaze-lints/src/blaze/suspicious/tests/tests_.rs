//! The lints about tests and docs that claim more than they check: `test_asserts_nothing`,
//! `assertion_cannot_fail`, `test_misses_its_subject`, `ignored_without_reason`,
//! `placeholder_body` and `doc_names_nothing`, each where it fires, where it does not and, for
//! the two with a fix, what the fix writes.

use super::{fixed, found, found_with, offered};

const NO_WAY: &str = "has no way to fail";

#[test]
fn a_test_with_no_failing_path_is_found_and_one_with_any_is_not() {
    let out = found(
        "test_asserts_nothing",
        r#"
pub fn double(n: u32) -> u32 { n * 2 }
#[cfg(test)]
mod tests {
    use super::double;
    fn check(n: u32) { assert!(double(n) == n * 2); }
    fn deeper(n: u32) { check(n) }
    #[test]
    fn runs_only() { let _ = double(2); }
    #[test]
    fn two_levels_down() { deeper(2); }
    #[test]
    fn asserts() { assert!(double(2) == 4); }
    #[test]
    fn asserts_eq() { assert_eq!(double(2), 4); }
    #[test]
    fn unwraps() { let _ = Some(double(2)).unwrap(); }
    #[test]
    fn panics() { if double(2) != 4 { panic!("wrong"); } }
    #[test]
    #[should_panic]
    fn should_panic_is_the_check() { let _ = double(2); }
    #[test]
    fn helper_asserts() { check(2); }
    #[test]
    fn returns_an_error() -> Result<(), u32> {
        if double(2) != 4 { return Err(1); }
        Ok(())
    }
    #[test]
    fn returns_ok_only() -> Result<(), u32> { let _ = double(2); Ok(()) }
}
"#,
    );
    assert!(out.contains("the test `runs_only` has no way to fail"), "{out}");
    assert!(out.contains("the test `two_levels_down` has no way to fail"), "{out}");
    assert!(out.contains("the test `returns_ok_only` has no way to fail"), "{out}");
    assert_eq!(out.matches(NO_WAY).count(), 3, "{out}");
}

#[test]
fn an_assertion_that_cannot_fail_is_found() {
    let out = found(
        "assertion_cannot_fail",
        r#"
const READY: bool = true;
pub fn f(x: u32, y: u32) {
    assert!(true);
    assert!(READY);
    assert_eq!(x, x);
    assert_eq!(1, 1);
    assert!(x > 0);
    assert_eq!(x, y);
    assert!(false, "unreachable");
}
pub fn g(x: u32) {
    return;
    assert!(x > 0);
}
"#,
    );
    assert!(out.contains("its condition is `true`"), "{out}");
    assert!(out.contains("its condition is a `const`"), "{out}");
    assert!(out.contains("both sides are `x`, the same place"), "{out}");
    assert!(out.contains("both sides are literals"), "{out}");
    assert!(out.contains("is never reached"), "{out}");
    assert_eq!(out.matches("cannot fail").count(), 4, "{out}");
    assert_eq!(out.matches("never reached").count(), 1, "{out}");
}

#[test]
fn a_test_that_reaches_none_of_its_crate_is_found() {
    let out = found(
        "test_misses_its_subject",
        r#"
pub struct Counter { n: u32 }
impl Counter {
    pub fn new() -> Counter { Counter { n: 0 } }
}
pub fn double(n: u32) -> u32 { n * 2 }
#[cfg(test)]
mod tests {
    fn copy_of_double(n: u32) -> u32 { n * 2 }
    fn through_helper() -> u32 { super::double(1) }
    #[test]
    fn checks_a_copy() { assert!(copy_of_double(2) == 4); }
    #[test]
    fn checks_std() { let v: Vec<u8> = Vec::new(); assert!(v.is_empty()); }
    #[test]
    fn calls_the_subject() { assert!(super::double(2) == 4); }
    #[test]
    fn makes_a_subject_value() { let c = super::Counter::new(); let _ = c; }
    #[test]
    fn through_a_helper() { assert!(through_helper() == 2); }
}
"#,
    );
    assert!(out.contains("the test `checks_a_copy` reaches none of the crate's own code"), "{out}");
    assert!(out.contains("the test `checks_std` reaches none of the crate's own code"), "{out}");
    assert_eq!(out.matches("reaches none of").count(), 2, "{out}");
}

#[test]
fn an_integration_test_is_judged_by_the_workspace_crates_it_uses() {
    let lib = "//- /lib/lib.rs crate:mylib deps:std\npub fn double(n: u32) -> u32 { n * 2 }\n";
    let out = found_with(
        "test_misses_its_subject",
        lib,
        r#"
//- /src/lib.rs crate:test:main deps:std,mylib
#[test]
fn uses_the_library() { assert!(mylib::double(2) == 4); }
#[test]
fn uses_nothing() { assert!(2 * 2 == 4); }
"#,
    );
    assert!(out.contains("the test `uses_nothing` reaches none"), "{out}");
    assert_eq!(out.matches("reaches none of").count(), 1, "{out}");
}

#[test]
fn an_ignore_with_no_reason_is_found() {
    let out = found(
        "ignored_without_reason",
        r#"
#[cfg(test)]
mod tests {
    #[test]
    #[ignore]
    fn bare() {}
    #[test]
    #[ignore = ""]
    fn empty() {}
    #[test]
    #[cfg_attr(test, ignore)]
    fn in_cfg_attr() {}
    #[test]
    #[ignore = "needs a network"]
    fn explained() {}
}
"#,
    );
    assert!(out.contains("`bare` is ignored with no reason"), "{out}");
    assert!(out.contains("`empty` is ignored with no reason"), "{out}");
    assert!(out.contains("`in_cfg_attr` is ignored with no reason"), "{out}");
    assert_eq!(out.matches("ignored with no reason").count(), 3, "{out}");
}

#[test]
fn a_body_that_only_says_success_is_found() {
    let out = found(
        "placeholder_body",
        r#"
pub struct Store { path: u32 }
pub struct Unit;
impl Store {
    pub fn save(&self, key: &str) -> Result<(), u32> { Ok(()) }
    pub fn real(&self) -> u32 { self.path }
}
impl Unit {
    pub fn ready(&self) -> bool { true }
}
/// Removes every entry of the cache.
pub fn clear_cache() {}
pub fn answer() -> u32 { 42 }
pub fn with_capacity(n: usize) -> Store { Store::new() }
impl Store {
    pub fn new() -> Store { Store { path: 0 } }
}
pub trait Hook {
    fn on_event(&self, n: u32) -> bool { true }
}
impl Hook for Store {
    fn on_event(&self, n: u32) -> bool { n > 0 }
}
"#,
    );
    assert!(out.contains("the whole body of `save` is `Ok(())`"), "{out}");
    assert!(out.contains("the whole body of `clear_cache` is `{}`"), "{out}");
    assert!(out.contains("while its doc says it removes"), "{out}");
    assert!(out.contains("the whole body of `with_capacity` is `new()`"), "{out}");
    assert_eq!(out.matches("a success that does nothing").count(), 3, "{out}");
}

#[test]
fn an_override_equal_to_the_trait_default_is_deleted() {
    let after = fixed(
        "placeholder_body",
        r#"
pub trait Hook {
    fn ready(&self) -> bool { true }
}
pub struct S { n: u32 }
impl Hook for S {
    fn ready(&self) -> bool { true }
}
"#,
    );
    assert!(after.contains("impl Hook for S {\n    \n}"), "{after}");
}

#[test]
fn a_doc_path_that_resolves_to_nothing_is_found() {
    let out = found(
        "doc_names_nothing",
        r#"
pub mod inner {
    pub struct Thing;
    pub struct Twin;
}
pub mod other {
    pub struct Twin;
}
pub struct Here;
impl Here {
    /// Like `Self::run`, and `Self::gone`.
    pub fn run(&self) {}
}
/// Calls `crate::removed()`, builds a `Here`, a `Vec<u8>` and a `Thing`, never a
/// `NothingAnywhere`, a `Twin` (two of them) or `len`.
///
/// ```
/// let x = `NotInACodeBlock`;
/// ```
pub fn uses() {}
"#,
    );
    assert!(out.contains("names `crate::removed`"), "{out}");
    assert!(out.contains("names `Self::gone`"), "{out}");
    assert!(out.contains("names `Thing`"), "{out}");
    assert!(out.contains("the one item of that name in the crate is `crate::inner::Thing`"), "{out}");
    assert!(out.contains("names `NothingAnywhere`"), "{out}");
    assert_eq!(out.matches("resolves to nothing").count(), 4, "{out}");
}

/// A doc names a private item of another module as rustdoc reads it, with no visibility
/// asked: a test in `mod tests` and a private struct of `inner` resolve; a missing one does not.
#[test]
fn a_doc_path_to_a_private_item_of_another_module_resolves() {
    let out = found(
        "doc_names_nothing",
        r#"
pub mod inner {
    struct Hidden;
    pub fn make() {}
}
/// Checked by `tests::builds_it`, built from `inner::Hidden`; `inner::Gone` is not there.
pub fn build() {}
#[cfg(test)]
mod tests {
    #[test]
    fn builds_it() { super::build(); }
}
"#,
    );
    assert!(!out.contains("names `tests::builds_it`"), "{out}");
    assert!(!out.contains("names `inner::Hidden`"), "{out}");
    assert!(out.contains("names `inner::Gone`"), "{out}");
    assert_eq!(out.matches("resolves to nothing").count(), 1, "{out}");
}

/// The fix is `MaybeIncorrect` (the doc may mean another item), so `--fix` leaves it and the
/// suggestion is what is checked.
#[test]
fn the_fix_writes_the_one_item_s_full_path() {
    let after = offered(
        "doc_names_nothing",
        r#"
pub mod inner {
    pub struct Thing;
}
/// Builds a `Thing`.
pub fn build() {}
"#,
    );
    assert!(after.contains("/// Builds a `crate::inner::Thing`."), "{after}");
}
