//! The seven lints of [`super`] over fixtures: where each fires and where it does not, and the
//! cases the analysis is for (a derived `PartialEq` observes every field, a hand-written one
//! that skips a field leaves it unobserved, a helper that asserts is followed, a value only
//! printed is not observed, a `Result` matched only on `Ok` leaves its errors untested).

use crate::tests::{CLIPPY_OFF, lint};

const MINICORE: &str = "//- minicore: sized, copy, clone, option, result, reach, eq, derive, fn, deref, index, slice, try, panic, builtin_impls\n";

/// A `std` whose assertions fail by calling a function that never returns, as std's do.
const STD: &str = r#"
//- /std/lib.rs crate:std
pub mod prelude {
    pub mod v1 {
        pub use crate::{assert, assert_eq, test};
        pub use core::prelude::v1::*;
    }
    pub mod rust_2015 { pub use super::v1::*; }
    pub mod rust_2018 { pub use super::v1::*; }
    pub mod rust_2021 { pub use super::v1::*; }
    pub mod rust_2024 { pub use super::v1::*; }
}
pub fn fail() -> ! { loop {} }
pub fn print<T>(value: &T) {}
#[macro_export]
macro_rules! assert {
    ($cond:expr $(,)?) => { if !$cond { $crate::fail() } };
}
#[macro_export]
macro_rules! assert_eq {
    ($left:expr, $right:expr $(,)?) => {
        match (&$left, &$right) {
            (left_val, right_val) => {
                if !(*left_val == *right_val) {
                    $crate::fail()
                }
            }
        }
    };
}
#[rustc_builtin_macro]
pub macro test($item:item) {}
//- /hegel/lib.rs crate:hegel
pub fn draw() -> u8 { 0 }
//- /blaze_sim/lib.rs crate:blaze_sim
pub struct Config { pub cases: u32 }
impl Config {
    pub const fn cases(cases: u32) -> Config { Config { cases } }
}
pub struct Tape;
impl Tape {
    pub fn int(&self, label: &'static str) -> u8 { 0 }
}
pub fn check<F: FnMut(&Tape)>(config: Config, mut body: F) { body(&Tape) }
"#;

const MAIN: &str = "//- /src/lib.rs crate:main deps:std,hegel,blaze_sim cfg:test\n";

/// What `blaze::<lint_name>` alone says of `main`.
fn found(lint_name: &str, main: &str) -> String {
    let fixture = format!("{MINICORE}{STD}{MAIN}{}", main.trim_start_matches('\n'));
    lint(&fixture, &format!("{CLIPPY_OFF} --warn=blaze::{lint_name}"))
}

const CONFIG: &str = r#"
pub struct Config { pub port: u16, pub host: u8 }
pub fn parse(n: u16) -> Config { Config { port: n, host: 1 } }
"#;

#[test]
fn result_part_unobserved_names_the_field_no_test_reads() {
    let out = found(
        "result_part_unobserved",
        &format!(
            "{CONFIG}{}",
            r#"
#[test]
fn parses_port() { let c = parse(80); assert!(c.port == 80); }
"#
        ),
    );
    assert!(out.contains("never look at `Config.host` of what it returns"), "{out}");
    assert!(!out.contains("Config.port"), "the asserted field is observed:\n{out}");
}

#[test]
fn a_derived_partial_eq_observes_every_field() {
    let out = found(
        "result_part_unobserved",
        r#"
#[derive(PartialEq)]
pub struct Config { pub port: u16, pub host: u8 }
pub fn parse(n: u16) -> Config { Config { port: n, host: 1 } }
#[test]
fn parses() { assert_eq!(parse(80), Config { port: 80, host: 1 }); }
"#,
    );
    assert!(!out.contains("result_part_unobserved"), "{out}");
}

#[test]
fn a_hand_partial_eq_that_skips_a_field_leaves_it_unobserved() {
    let out = found(
        "result_part_unobserved",
        &format!(
            "{CONFIG}{}",
            r#"
impl PartialEq for Config {
    fn eq(&self, other: &Config) -> bool { self.port == other.port }
}
#[test]
fn parses() { assert_eq!(parse(80), Config { port: 80, host: 9 }); }
"#
        ),
    );
    assert!(out.contains("`Config.host`"), "{out}");
    assert!(!out.contains("`Config.port`"), "{out}");
}

#[test]
fn a_helper_that_asserts_is_followed() {
    let out = found(
        "result_part_unobserved",
        &format!(
            "{CONFIG}{}",
            r#"
#[cfg(test)]
mod tests {
    use super::*;
    fn check_port(c: &Config, port: u16) { assert!(c.port == port); }
    #[test]
    fn parses_port() { let c = parse(80); check_port(&c, 80); }
}
"#
        ),
    );
    assert!(out.contains("never look at `Config.host`"), "{out}");
    assert!(!out.contains("`Config.port`"), "the helper's assertion observes the port:\n{out}");
}

#[test]
fn a_value_only_printed_is_not_observed() {
    let out = found(
        "result_part_unobserved",
        &format!(
            "{CONFIG}{}",
            r#"
#[test]
fn parses_port() { let c = parse(80); std::print(&c); assert!(c.port == 80); }
"#
        ),
    );
    assert!(out.contains("never look at `Config.host`"), "printing can't fail a test:\n{out}");
}

const READ: &str = r#"
pub enum Error { Empty, Long }
pub fn read(n: u8) -> Result<u8, Error> {
    if n == 0 {
        return Err(Error::Empty);
    }
    Ok(n)
}
"#;

#[test]
fn error_path_untested_fires_where_a_result_is_matched_only_on_ok() {
    let out = found(
        "error_path_untested",
        &format!(
            "{READ}{}",
            r#"
#[test]
fn reads() {
    match read(3) {
        Ok(v) => assert!(v == 3),
        Err(_) => std::fail(),
    }
}
"#
        ),
    );
    assert!(out.contains("never which error it returns"), "{out}");
    assert!(out.contains("`Err(Error::Empty)`"), "the note names where it fails:\n{out}");
}

#[test]
fn error_path_untested_is_quiet_where_a_test_names_the_error() {
    let out = found(
        "error_path_untested",
        &format!(
            "{READ}{}",
            r#"
#[test]
fn reads() { assert!(read(3).unwrap_or(0) == 3); }
#[test]
fn rejects_empty() {
    match read(0) {
        Err(Error::Empty) => {}
        _ => std::fail(),
    }
}
"#
        ),
    );
    assert!(!out.contains("error_path_untested"), "{out}");
}

#[test]
fn weak_assertion_fires_on_is_some_of_a_value_with_more_in_it() {
    let out = found(
        "weak_assertion",
        &format!(
            "{CONFIG}{}",
            r#"
pub fn find(n: u16) -> Option<Config> { Some(parse(n)) }
pub fn save(n: u8) -> Result<(), u8> { Ok(()) }
#[test]
fn finds() { assert!(find(1).is_some()); }
#[test]
fn finds_the_port() { assert!(find(1).unwrap().port == 1); }
#[test]
fn saves() { assert!(save(1).is_ok()); }
#[test]
fn finds_nothing() { assert!(find(9).is_none()); }
"#
        ),
    );
    assert_eq!(out.matches("checks only `is_some()` of what `find` returns").count(), 1, "{out}");
    assert!(!out.contains("is_none"), "`is_none()` that holds pins the whole `Option`:\n{out}");
    assert!(!out.contains("`save`"), "`is_ok()` of a `Result<(), _>` is all of it:\n{out}");
}

#[test]
fn mirror_assertion_fires_where_the_expected_value_is_computed_by_the_subject() {
    let out = found(
        "mirror_assertion",
        r#"
pub fn double(n: u32) -> u32 { n * 2 }
#[test]
fn doubles() { let expected = double(4); assert_eq!(double(4), expected); }
#[test]
fn doubles_to_a_literal() { assert_eq!(double(4), 8); }
#[test]
fn doubles_apart() { assert_eq!(double(4), double(2) * 2); }
"#,
    );
    assert_eq!(out.matches("both sides of the assertion are `double(4)`").count(), 1, "{out}");
}

#[test]
fn pseudo_tested_fires_where_a_test_calls_and_looks_at_nothing() {
    let out = found(
        "pseudo_tested",
        r#"
pub fn area(w: u32, h: u32) -> u32 { w * h }
pub fn perimeter(w: u32, h: u32) -> u32 { 2 * (w + h) }
#[test]
fn computes_area() { let _ = area(2, 3); }
#[test]
fn computes_perimeter() { assert_eq!(perimeter(2, 3), 10); }
"#,
    );
    assert!(out.contains("`area` is called by tests (`computes_area`)"), "{out}");
    assert!(!out.contains("`perimeter`"), "{out}");
}

/// The invariant: taking every assertion out of a test leaves what it calls pseudo-tested.
#[test]
fn removing_every_assertion_makes_the_subject_pseudo_tested() {
    let checked = found(
        "pseudo_tested",
        r#"
pub fn area(w: u32, h: u32) -> u32 { w * h }
#[test]
fn computes_area() { assert_eq!(area(2, 3), 6); }
"#,
    );
    let unchecked = found(
        "pseudo_tested",
        r#"
pub fn area(w: u32, h: u32) -> u32 { w * h }
#[test]
fn computes_area() { let _ = area(2, 3) == 6; }
"#,
    );
    assert!(!checked.contains("pseudo_tested"), "{checked}");
    assert!(unchecked.contains("`area` is called by tests"), "{unchecked}");
}

#[test]
fn unreached_by_tests_names_the_public_function_no_test_reaches() {
    let out = found(
        "unreached_by_tests",
        r#"
pub fn used() -> u32 { helper() }
fn helper() -> u32 { 1 }
pub fn orphan() -> u32 { 2 }
#[test]
fn uses() { assert_eq!(used(), 1); }
"#,
    );
    assert_eq!(out.matches("no test reaches").count(), 1, "{out}");
    assert!(out.contains("no test reaches `orphan`"), "{out}");
}

#[test]
fn property_over_examples_asks_for_the_round_trip() {
    let out = found(
        "property_over_examples",
        r#"
pub fn encode(n: u8) -> u16 { n as u16 }
pub fn decode(n: u16) -> u8 { n as u8 }
#[test]
fn encodes() { assert_eq!(encode(1), 1); }
"#,
    );
    assert!(out.contains("`encode` has an inverse, `decode`"), "{out}");
    let out = found(
        "property_over_examples",
        r#"
pub fn encode(n: u8) -> u16 { n as u16 }
pub fn decode(n: u16) -> u8 { n as u8 }
#[test]
fn encodes() { assert_eq!(encode(1), 1); }
#[test]
fn round_trips() { let x = hegel::draw(); assert_eq!(decode(encode(x)), x); }
"#,
    );
    assert!(!out.contains("property_over_examples"), "{out}");
}

/// A plain `#[test]` whose body is a call of `blaze_sim::check` with a closure is a property
/// test by that call alone, with no attribute of its own: the round trip it checks inside the
/// closure answers `property_over_examples`.
#[test]
fn a_blaze_sim_check_is_a_property_test() {
    let out = found(
        "property_over_examples",
        r#"
pub fn encode(n: u8) -> u16 { n as u16 }
pub fn decode(n: u16) -> u8 { n as u8 }
#[test]
fn encodes() { assert_eq!(encode(1), 1); }
#[test]
fn round_trips() {
    blaze_sim::check(blaze_sim::Config::cases(200), |tape| {
        let x = tape.int("x");
        assert_eq!(decode(encode(x)), x);
    });
}
"#,
    );
    assert!(!out.contains("property_over_examples"), "{out}");
}
