//! Type patterns (`type-matches`) and `peel-refs`, each test named for the property it pins:
//! generic arguments bind and nest, a single name means what the std prelude names by it, and
//! a refusal says what to write instead.

use super::{texts, view};
use crate::query;

const OWN: &str = r#"
//- /lib.rs crate:lib edition:2021
pub struct Wrapper<T>(pub T);
pub struct Pair<A, B>(pub A, pub B);
pub fn value(_: Wrapper<u8>) {}
pub fn reference(_: &Wrapper<u16>) {}
pub fn nested(_: Wrapper<Wrapper<u32>>) {}
pub fn same(_: Pair<u8, u8>) {}
pub fn differ(_: Pair<u8, u16>) {}
pub fn plain(_: u8) {}
pub fn twice(_: &&mut Wrapper<u8>) {}
"#;

/// Each fn of `fixture`'s `lib.rs` whose first parameter's type matches `pattern`, as its def
/// path and, after it, the types `pattern`'s metavariables `vars` bind.
fn params(fixture: &str, outside: usize, pattern: &str, vars: &[&str]) -> Vec<Vec<String>> {
    let (db, files) = view(fixture, outside, &["lib.rs"]);
    let head = std::iter::once("p").chain(vars.iter().copied()).collect::<Vec<_>>().join(" ");
    let rules = format!(
        r#"(rule (hit {head}) (syntax s "FN") (defines s d) (fn-param d 0 t) (type-matches t "{pattern}") (def-path d p))"#
    );
    let outcome = query(&db, &files, &rules).unwrap_or_else(|error| panic!("{pattern}: {error}"));
    let mut found = texts(&outcome, "hit");
    found.sort();
    found
}

#[test]
fn a_metavariable_among_a_def_paths_arguments_binds_the_argument() {
    assert_eq!(params(OWN, 0, "lib::Wrapper<$a>", &["a"]), [["lib::nested", "Wrapper<u32>"], ["lib::value", "u8"]]);
}

#[test]
fn a_wildcard_argument_matches_any_argument() {
    assert_eq!(params(OWN, 0, "lib::Wrapper<_>", &[]), [["lib::nested"], ["lib::value"]]);
}

#[test]
fn a_def_path_among_the_arguments_nests() {
    assert_eq!(params(OWN, 0, "lib::Wrapper<lib::Wrapper<$a>>", &["a"]), [["lib::nested", "u32"]]);
}

#[test]
fn a_reference_pattern_matches_the_reference_and_not_the_value() {
    assert_eq!(params(OWN, 0, "&lib::Wrapper<$a>", &["a"]), [["lib::reference", "u16"]]);
}

#[test]
fn a_metavariable_used_twice_binds_equal_types() {
    assert_eq!(params(OWN, 0, "(lib::Pair<$a, $a>)", &["a"]), [["lib::same", "u8"]]);
    assert_eq!(params(OWN, 0, "lib::Pair<$a, $b>", &["a", "b"]), [["lib::differ", "u8", "u16"], ["lib::same", "u8", "u8"]]);
}

#[test]
fn a_single_name_no_prelude_names_matches_a_type_of_that_name_with_its_arguments() {
    assert_eq!(params(OWN, 0, "Wrapper<$a>", &["a"]), [["lib::nested", "Wrapper<u32>"], ["lib::value", "u8"]]);
    assert_eq!(params(OWN, 0, "u8", &[]), [["lib::plain"]]);
}

#[test]
fn peel_refs_strips_every_leading_reference_and_keeps_a_type_with_none() {
    let (db, files) = view(OWN, 0, &["lib.rs"]);
    let rules = r#"(rule (peeled p u) (syntax s "FN") (defines s d) (fn-param d 0 t) (peel-refs t u) (def-path d p))"#;
    let mut found = texts(&query(&db, &files, rules).unwrap(), "peeled");
    found.sort();
    assert_eq!(
        found,
        [
            ["lib::differ", "Pair<u8, u16>"],
            ["lib::nested", "Wrapper<Wrapper<u32>>"],
            ["lib::plain", "u8"],
            ["lib::reference", "Wrapper<u16>"],
            ["lib::same", "Pair<u8, u8>"],
            ["lib::twice", "Wrapper<u8>"],
            ["lib::value", "Wrapper<u8>"],
        ]
    );
}

/// A std of its own: `Vec`, `String`, `Option` and `Result` in their modules and its prelude,
/// as the real one lays them out (`test-utils`' minicore has `Option` and `Result` and no
/// `Vec` or `String`).
pub(super) const STD: &str = r#"
//- /std.rs crate:std edition:2021
pub mod vec {
    pub struct Vec<T>(T);
    impl<T> Vec<T> {
        pub fn len(&self) -> usize { 0 }
        pub fn is_empty(&self) -> bool { true }
    }
}
pub mod string {
    pub struct String(u8);
    impl String {
        pub fn len(&self) -> usize { 0 }
        pub fn is_empty(&self) -> bool { true }
    }
}
pub mod option { pub enum Option<T> { None, Some(T) } }
pub mod result { pub enum Result<T, E> { Ok(T), Err(E) } }
pub mod prelude {
    pub mod rust_2021 {
        pub use crate::option::Option::{self, None, Some};
        pub use crate::result::Result::{self, Err, Ok};
        pub use crate::string::String;
        pub use crate::vec::Vec;
    }
}
"#;

fn with_std(lib: &str) -> String {
    format!("{STD}//- /lib.rs crate:lib deps:std edition:2021\n{lib}")
}

#[test]
fn a_single_name_the_prelude_names_is_that_item() {
    let lib = with_std(
        "pub fn list(_: Vec<u8>) {}\n\
         pub fn text(_: String) {}\n\
         pub fn maybe(_: Option<u16>) {}\n\
         pub fn out(_: Result<u8, String>) {}\n",
    );
    assert_eq!(params(&lib, 1, "Vec<$a>", &["a"]), [["lib::list", "u8"]]);
    assert_eq!(params(&lib, 1, "String", &[]), [["lib::text"]]);
    assert_eq!(params(&lib, 1, "Option<$a>", &["a"]), [["lib::maybe", "u16"]]);
    assert_eq!(params(&lib, 1, "Result<_, String>", &[]), [["lib::out"]]);
    assert_eq!(params(&lib, 1, "std::vec::Vec<$a>", &["a"]), [["lib::list", "u8"]]);
}

#[test]
fn a_crates_own_type_named_as_a_prelude_item_does_not_match_the_prelude_name() {
    let lib = with_std(
        "pub struct Vec<T>(pub T);\n\
         pub fn own(_: Vec<u8>) {}\n\
         pub fn real(_: std::vec::Vec<u8>) {}\n",
    );
    assert_eq!(params(&lib, 1, "Vec<$a>", &["a"]), [["lib::real", "u8"]]);
    assert_eq!(params(&lib, 1, "lib::Vec<$a>", &["a"]), [["lib::own", "u8"]]);
}

#[test]
fn the_fixture_cores_option_and_result_match_by_their_prelude_names() {
    let lib = "//- minicore: option, result\n//- /lib.rs crate:lib edition:2021\n\
               pub fn maybe(_: Option<u8>) {}\n\
               pub fn out(_: Result<u8, u16>) {}\n";
    assert_eq!(params(lib, 0, "Option<$a>", &["a"]), [["lib::maybe", "u8"]]);
    assert_eq!(params(lib, 0, "Result<$a, $e>", &["a", "e"]), [["lib::out", "u8", "u16"]]);
    assert_eq!(params(lib, 0, "core::option::Option<_>", &[]), [["lib::maybe"]]);
}

/// The load error of a rules file whose one type pattern is `pattern`.
fn refusal(pattern: &str) -> String {
    let (db, files) = view(OWN, 0, &["lib.rs"]);
    let rules = format!(r#"(rule (hit t) (syntax s "FN") (defines s d) (fn-return d t) (type-matches t "{pattern}"))"#);
    match query(&db, &files, &rules) {
        Ok(_) => panic!("`{pattern}` loaded"),
        Err(error) => error.to_string(),
    }
}

#[test]
fn a_refused_type_pattern_says_what_to_write_instead() {
    let cases = [
        ("crate::Wrapper<$a>", "write the def path from the crate's name"),
        ("$t<u8>", "write `$t` alone"),
        ("u8<u16>", "write `u8`"),
        ("impl Fn()", "add `(implements t"),
        ("*const u8", "match it whole with `$t` or `_`"),
        ("lib::Wrapper<N = u8>", "write a type, `_` or `$name` for each type argument"),
    ];
    for (pattern, advice) in cases {
        let said = refusal(pattern);
        assert!(said.contains(advice), "`{pattern}`: {said}");
        assert!(!said.contains("PATH_TYPE") && !said.contains("_TYPE"), "`{pattern}` names a syntax kind: {said}");
    }
}
