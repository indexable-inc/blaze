//! The weight lints, each alone on the command line, on a crate `main` built with `cfg(test)`
//! (so its `#[cfg(test)]` modules are the tests the use rows weigh) and, where a lint reads the
//! workspace, a second crate `user` that depends on it: where each fires, where it does not,
//! and, for `swapped_arguments`, that its fix lints clean. `try` and `from` give `Result` its
//! `Try`, without which a `?` lowers to no `return` of the residual.

use crate::tests::{lint, lint_both_builds, offered_in};

const MINICORE: &str = "//- minicore: sized, copy, clone, option, result, try, from, fmt, derive, eq, ord, hash, phantom_data, fn, add, mul\n";

const MAIN: &str = "//- /src/lib.rs crate:main cfg:test\n";

fn flags(lint_name: &str) -> String {
    format!("-A clippy::all --warn=blaze::{lint_name}")
}

/// What `blaze::<lint_name>` alone says of `main`.
fn found(lint_name: &str, main: &str) -> String {
    lint(&format!("{MINICORE}{MAIN}{}", main.trim_start_matches('\n')), &flags(lint_name))
}

/// What `blaze::<lint_name>` alone says of `main`, with a crate `user` of the workspace that
/// depends on it.
fn found_with_user(lint_name: &str, user: &str, main: &str) -> String {
    let fixture = format!(
        "{MINICORE}//- /user/lib.rs crate:user deps:main\n{}\n{MAIN}{}",
        user.trim_start_matches('\n'),
        main.trim_start_matches('\n')
    );
    lint(&fixture, &flags(lint_name))
}

#[test]
fn swapped_arguments_finds_a_swap_the_names_say_and_swaps_it_back() {
    const SWAPPED: &str = r#"
pub fn copy(src_path: u32, dst_path: u32) -> u32 { src_path + dst_path }
pub fn run(src_path: u32, dst_path: u32) -> u32 {
    copy(dst_path, src_path)
}
"#;
    let out = found(
        "swapped_arguments",
        &format!(
            "{SWAPPED}{}",
            r#"
pub fn kept(src_path: u32, dst_path: u32) -> u32 { copy(src_path, dst_path) }
pub fn unnamed(a: u32, b: u32) -> u32 { copy(b, a) }
pub fn half(dst: u32, other: u32) -> u32 { copy(other, dst) }
"#
        ),
    );
    assert_eq!(out.matches("the names say they are swapped").count(), 1, "{out}");
    assert!(out.contains("`dst_path` is passed as `src_path`"), "{out}");
    let after = offered_in(&format!("{MINICORE}{MAIN}"), SWAPPED.trim_start_matches('\n'), &flags("swapped_arguments"));
    assert!(after.contains("copy(src_path, dst_path)"), "{after}");
}

#[test]
fn write_only_field_finds_fields_only_tests_or_derives_read() {
    let out = found_with_user(
        "write_only_field",
        r#"
pub fn reads(job: &main::Job) -> u32 { job.id }
"#,
        r#"
#[derive(Debug, Clone, PartialEq)]
pub struct Job { pub id: u32, pub note: u32, pub _kept: u32 }
pub fn make() -> Job { Job { id: 1, note: 2, _kept: 3 } }
#[derive(Debug, PartialEq)]
struct Compared { seen: u32 }
pub fn compare() -> bool { Compared { seen: 1 } == Compared { seen: 2 } }
#[derive(Hash, PartialEq, Eq)]
struct Key(u32);
pub fn key() -> bool { Key(1) == Key(2) }
#[cfg(test)]
mod tests {
    fn note(job: &super::Job) -> u32 { job.note }
}
"#,
    );
    assert_eq!(out.matches("is written and").count(), 2, "{out}");
    assert!(out.contains("field `note` is written and only tests read it"), "{out}");
    assert!(out.contains("field `seen` is written and read only by derived"), "{out}");
}

#[test]
fn only_used_in_tests_finds_an_exported_helper_only_tests_call() {
    let out = found_with_user(
        "only_used_in_tests",
        r#"
pub fn run() -> u32 { main::used() }
"#,
        r#"
pub fn helper() -> u32 { 1 }
pub fn used() -> u32 { 2 }
pub fn uncalled() -> u32 { 3 }
#[cfg(test)]
mod tests {
    fn t() -> u32 { super::helper() + super::used() }
}
"#,
    );
    assert_eq!(out.matches("is used only by tests").count(), 1, "{out}");
    assert!(out.contains("`fn helper` is used only by tests"), "{out}");
}

#[test]
fn single_impl_trait_finds_one_impl_and_counts_mocks_strategies_and_extensions_as_more() {
    let out = found(
        "single_impl_trait",
        r#"
trait Store { fn get(&self) -> u32; }
struct Disk;
impl Store for Disk { fn get(&self) -> u32 { 0 } }
trait Two { fn f(&self); }
impl Two for Disk { fn f(&self) {} }
struct Mem;
impl Two for Mem { fn f(&self) {} }
trait Strategy { fn g(&self); }
pub struct Driver<S: Strategy> { pub s: S }
struct Real;
impl Strategy for Real { fn g(&self) {} }
trait Ext { fn h(&self); }
impl Ext for u32 { fn h(&self) {} }
trait Mocked { fn m(&self); }
impl Mocked for Disk { fn m(&self) {} }
#[cfg(test)]
mod tests {
    struct Mock;
    impl super::Mocked for Mock { fn m(&self) {} }
}
"#,
    );
    assert_eq!(out.matches("has one impl in the workspace").count(), 1, "{out}");
    assert!(out.contains("trait `Store` has one impl in the workspace, for `Disk`"), "{out}");
}

#[test]
fn forwarding_fn_finds_a_second_name_for_one_function() {
    let out = found(
        "forwarding_fn",
        r#"
fn read_config(p: u32) -> u32 { p + 1 }
fn add(a: u32, b: u32) -> u32 { a + b }
fn load(p: u32) -> u32 { read_config(p) }
fn changed(p: u32) -> u32 { read_config(p + 1) }
fn reordered(a: u32, b: u32) -> u32 { add(b, a) }
pub fn api(p: u32) -> u32 { read_config(p) }
struct S { inner: Inner }
struct Inner;
impl Inner { fn m(&self, x: u32) -> u32 { x } }
impl S {
    fn own(&self, x: u32) -> u32 { x }
    fn fwd(&self, x: u32) -> u32 { self.own(x) }
    fn delegated(&self, x: u32) -> u32 { self.inner.m(x) }
}
pub fn run(s: &S) -> u32 { load(1) + changed(2) + reordered(1, 2) + s.fwd(3) + s.delegated(4) }
"#,
    );
    assert_eq!(out.matches("only forwards its parameters").count(), 2, "{out}");
    assert!(out.contains("`fn load` only forwards its parameters to `read_config`"), "{out}");
    assert!(out.contains("`fn fwd` only forwards its parameters to `own`"), "{out}");
}

#[test]
fn constant_argument_finds_a_parameter_every_call_passes_one_value() {
    let out = found(
        "constant_argument",
        r#"
fn scale(x: u32, factor: u32) -> u32 { x * factor }
fn once(x: u32) -> u32 { x }
pub fn run(a: u32) -> u32 { scale(a, 3) + scale(a + 1, 3) + once(4) }
"#,
    );
    assert_eq!(out.matches("calls of `fn").count(), 1, "{out}");
    assert!(out.contains("every one of the 2 calls of `fn scale` passes `3` for `factor`"), "{out}");
}

/// What `blaze::<lint_name>` alone says of `main` in its plain build and in its `--test` build.
fn found_in_both_builds(lint_name: &str, main: &str) -> [String; 2] {
    lint_both_builds(&format!("{MINICORE}{MAIN}{}", main.trim_start_matches('\n')), &flags(lint_name))
}

/// The invariant: a library's `--test` build judges its production code as its plain build
/// does, so an `#[expect]` that silences a finding there is fulfilled in both builds, and one
/// on an item the lint does not fire on is unfulfilled in both. The `--test` build once took
/// all of a library for test code and ran no `write_only_field`, and said the expectation its
/// plain build had met unfulfilled.
#[test]
fn an_expected_write_only_field_is_fulfilled_in_the_test_build_too() {
    let fires = r#"
pub struct Job { pub id: u32, pub note: u32 }
pub fn make() -> Job { Job { id: 1, note: 2 } }
pub fn read(job: &Job) -> u32 { job.id }
"#;
    let expected = r#"
pub struct Job {
    pub id: u32,
    #[expect(
        blaze::write_only_field,
        reason = "kept for the format"
    )]
    pub note: u32,
}
pub fn make() -> Job { Job { id: 1, note: 2 } }
pub fn read(job: &Job) -> u32 { job.id }
"#;
    let missed = r#"
pub struct Job {
    #[expect(
        blaze::write_only_field,
        reason = "read below"
    )]
    pub id: u32,
    pub note: u32,
}
pub fn make() -> Job { Job { id: 1, note: 2 } }
pub fn read(job: &Job) -> u32 { job.id + job.note }
"#;
    for out in found_in_both_builds("write_only_field", fires) {
        assert!(out.contains("field `note` is written and never read in the workspace"), "{out}");
    }
    for out in found_in_both_builds("write_only_field", expected) {
        assert!(!out.contains("is written and"), "{out}");
        assert!(!out.contains("lint expectation is unfulfilled"), "the expectation is met:\n{out}");
    }
    for out in found_in_both_builds("write_only_field", missed) {
        assert_eq!(out.matches("lint expectation is unfulfilled").count(), 1, "{out}");
    }
}

/// The invariant of [`an_expected_write_only_field_is_fulfilled_in_the_test_build_too`], for
/// `constant_argument`, whose finding is on a parameter inside the function the expectation is on.
#[test]
fn an_expected_constant_argument_is_fulfilled_in_the_test_build_too() {
    let fires = r#"
fn scale(x: u32, factor: u32) -> u32 { x * factor }
pub fn run(a: u32) -> u32 { scale(a, 3) + scale(a + 1, 3) }
"#;
    let expected = r#"
#[expect(
    blaze::constant_argument,
    reason = "the factor is the caller's to choose"
)]
fn scale(x: u32, factor: u32) -> u32 { x * factor }
pub fn run(a: u32) -> u32 { scale(a, 3) + scale(a + 1, 3) }
"#;
    let missed = r#"
#[expect(
    blaze::constant_argument,
    reason = "the factor is the caller's to choose"
)]
fn scale(x: u32, factor: u32) -> u32 { x * factor }
pub fn run(a: u32) -> u32 { scale(a, 3) + scale(a + 1, 4) }
"#;
    for out in found_in_both_builds("constant_argument", fires) {
        assert!(out.contains("every one of the 2 calls of `fn scale` passes `3` for `factor`"), "{out}");
    }
    for out in found_in_both_builds("constant_argument", expected) {
        assert!(!out.contains("calls of `fn"), "{out}");
        assert!(!out.contains("lint expectation is unfulfilled"), "the expectation is met:\n{out}");
    }
    for out in found_in_both_builds("constant_argument", missed) {
        assert!(!out.contains("calls of `fn"), "{out}");
        assert_eq!(out.matches("lint expectation is unfulfilled").count(), 1, "{out}");
    }
}

#[test]
fn always_same_result_finds_a_result_never_err_an_option_never_none_and_a_constant() {
    let out = found(
        "always_same_result",
        r#"
fn never_err(x: u32) -> Result<u32, ()> { if x > 1 { return Ok(x); } Ok(0) }
fn can_err(x: u32) -> Result<u32, ()> { if x > 1 { Ok(x) } else { Err(()) } }
fn same(x: u32) -> u32 { if x > 1 { 7 } else { 7 } }
fn varies(x: u32) -> u32 { if x > 1 { 7 } else { 8 } }
fn never_none(x: u32) -> Option<u32> { Some(x) }
fn passes_on(x: u32) -> Result<u32, ()> { let y = can_err(x)?; Ok(y) }
pub fn run(x: u32) -> u32 {
    let _ = never_err(x);
    let _ = passes_on(x);
    let _ = never_none(x);
    same(x) + varies(x)
}
"#,
    );
    assert!(out.contains("`fn never_err` returns a `Result` that is never `Err`"), "{out}");
    assert!(out.contains("`fn never_none` returns an `Option` that is never `None`"), "{out}");
    assert!(out.contains("`fn same` returns the same constant at every return"), "{out}");
    assert_eq!(out.matches("`fn ").count(), 3, "{out}");
}

#[test]
fn never_constructed_and_never_matched_find_unused_variants() {
    const ENUMS: &str = r#"
pub enum LoadError { Missing, Corrupt }
pub fn load(x: u32) -> Result<u32, LoadError> { if x == 0 { return Err(LoadError::Missing); } Ok(x) }
pub enum Mode { Fast, Slow, Odd }
pub fn mode(x: u32) -> Mode { if x == 0 { Mode::Fast } else if x == 1 { Mode::Slow } else { Mode::Odd } }
pub fn speed(m: Mode) -> u32 { match m { Mode::Fast => 1, _ => 2 } }
"#;
    let out = found("never_constructed", ENUMS);
    assert_eq!(out.matches("never constructed").count(), 1, "{out}");
    assert!(out.contains("error variant `LoadError::Corrupt` is never constructed"), "{out}");
    let out = found("never_matched", ENUMS);
    assert_eq!(out.matches("never matched by name").count(), 2, "{out}");
    assert!(out.contains("`Mode::Slow`") && out.contains("`Mode::Odd`"), "{out}");
}

#[test]
fn swappable_parameters_finds_adjacent_same_typed_parameters() {
    let out = found(
        "swappable_parameters",
        r#"
pub fn resize(width: u32, height: u32) {}
pub fn mixed(width: u32, flag: bool) {}
pub fn flags(a: bool, b: bool) {}
fn private_once(a: u32, b: u32) {}
fn private_twice(a: u32, b: u32) {}
pub fn unused(_a: u32, _b: u32) {}
pub fn run() { private_once(1, 2); private_twice(1, 2); private_twice(3, 4); }
"#,
    );
    assert_eq!(out.matches("are adjacent parameters").count(), 2, "{out}");
    assert!(out.contains("of `fn resize`") && out.contains("of `fn private_twice`"), "{out}");
}

#[test]
fn names_split_into_words_and_compare_by_share() {
    use super::swapped_arguments::{alike, words};
    assert_eq!(words("srcPath"), Some(vec!["path".to_owned(), "src".to_owned()]));
    assert_eq!(words("dst_path"), Some(vec!["dst".to_owned(), "path".to_owned()]));
    assert_eq!(words("ab"), None);
    assert_eq!(words("arg0"), None);
    let (src, dst) = (words("src_path").unwrap_or_default(), words("dst_path").unwrap_or_default());
    assert_eq!(alike(&src, &src), 100);
    assert_eq!(alike(&src, &dst), 33);
}
