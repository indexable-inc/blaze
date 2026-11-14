//! The design doc's rules files, read from the doc itself, run over fixtures: an example the
//! language cannot run fails here rather than misleading a reader.

use super::rewrite::after;
use super::types::STD;
use super::view;
use crate::edit;

const SPEC: &str = include_str!("../../../../docs/v2/plan/design/astlog-sema.md");

/// The `n`th ```lisp block of the design doc.
fn example(n: usize) -> &'static str {
    let block = SPEC.split("```lisp\n").nth(n + 1).expect("the doc's lisp block");
    block.split("```").next().expect("a closed block")
}

/// `lib.rs` after `rules`' rewrites, over `fixture` whose first `outside` files are not the view's.
fn rewritten(fixture: &str, outside: usize, rules: &str) -> String {
    let (db, files) = view(fixture, outside, &["lib.rs"]);
    let applied = edit(&db, &files, rules, &[]).unwrap_or_else(|error| panic!("{error}"));
    after(&files, &applied.edited.ops).swap_remove(0).1
}

#[test]
fn the_docs_unwrap_to_try_rewrites_an_unwrap_on_a_result_in_a_result_fn_only() {
    let rules = example(0);
    assert!(rules.contains("(rewrite unwrap-to-try"), "{rules}");
    let fixture = "//- minicore: result, option\n//- /lib.rs crate:lib edition:2021\n\
        pub fn parse(r: Result<u8, u16>) -> Result<u8, u16> { let v = r.unwrap(); Ok(v) }\n\
        pub fn total(r: Result<u8, u16>) -> u8 { r.unwrap() }\n\
        pub fn maybe(o: Option<u8>) -> Result<u8, u16> { Ok(o.unwrap()) }\n";
    let text = rewritten(fixture, 0, rules);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(
        lines,
        [
            "pub fn parse(r: Result<u8, u16>) -> Result<u8, u16> { let v = r?; Ok(v) }",
            "pub fn total(r: Result<u8, u16>) -> u8 { r.unwrap() }",
            "pub fn maybe(o: Option<u8>) -> Result<u8, u16> { Ok(o.unwrap()) }",
        ],
    );
}

#[test]
fn the_docs_connect_timeout_adds_a_parameter_and_its_default_at_every_call() {
    let rules = example(2);
    assert!(rules.contains("(add-param d"), "{rules}");
    let lib = "pub fn connect(addr: &str) -> usize { addr.len() }\n\
        pub use self::connect as dial;\n\
        macro_rules! id { ($($t:tt)*) => { $($t)* }; }\n\
        pub fn direct() -> usize { connect(\"a\") + self::connect(\"b\") }\n\
        pub fn aliased() -> usize { dial(\"c\") + id!(crate::connect(\"d\")) }\n";
    let fixture = format!("//- /lib.rs crate:lib edition:2021\n{lib}");
    let text = rewritten(&fixture, 0, rules);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(
        lines,
        [
            "pub fn connect(addr: &str, timeout_secs: u64) -> usize { addr.len() }",
            "pub use self::connect as dial;",
            "macro_rules! id { ($($t:tt)*) => { $($t)* }; }",
            "pub fn direct() -> usize { connect(\"a\", 30) + self::connect(\"b\", 30) }",
            "pub fn aliased() -> usize { dial(\"c\", 30) + id!(crate::connect(\"d\", 30)) }",
        ],
    );
}

#[test]
fn the_docs_drop_legacy_prefix_renames_each_matching_fn_and_every_reference() {
    let rules = example(3);
    assert!(rules.contains("(rewrite drop-legacy-prefix"), "{rules}");
    let lib = "pub fn legacy_parse(x: u8) -> u8 { x }\n\
        pub fn not_legacy_load() -> u8 { 1 }\n\
        pub struct S;\n\
        impl S { pub fn legacy_make() -> S { S } }\n\
        macro_rules! id { ($($t:tt)*) => { $($t)* }; }\n\
        pub mod inner { pub use crate::legacy_parse as parse_old; }\n\
        pub fn uses() -> u8 { legacy_parse(1) + inner::parse_old(2) + id!(crate::legacy_parse(3)) }\n\
        pub fn made() -> S { S::legacy_make() }\n";
    let fixture = format!("//- /lib.rs crate:lib edition:2021\n{lib}");
    let text = rewritten(&fixture, 0, rules);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(
        lines,
        [
            "pub fn parse(x: u8) -> u8 { x }",
            "pub fn not_legacy_load() -> u8 { 1 }",
            "pub struct S;",
            "impl S { pub fn make() -> S { S } }",
            "macro_rules! id { ($($t:tt)*) => { $($t)* }; }",
            "pub mod inner { pub use crate::parse as parse_old; }",
            "pub fn uses() -> u8 { parse(1) + inner::parse_old(2) + id!(crate::parse(3)) }",
            "pub fn made() -> S { S::make() }",
        ],
    );
}

#[test]
fn a_dollar_variable_in_an_action_text_is_refused_saying_to_pass_the_variable() {
    let fixture = "//- /lib.rs crate:lib edition:2021\npub fn legacy_parse() {}\n";
    let (db, files) = view(fixture, 0, &["lib.rs"]);
    let rules = example(3).replace("(rename d new)", "(rename d \"$new\")");
    let Err(error) = edit(&db, &files, &rules, &[]) else { panic!("`\"$new\"` loaded") };
    let message = error.to_string();
    assert!(
        message.contains("`\"$new\"` is no template") && message.contains("`new` without quotes"),
        "{message}"
    );
}

#[test]
fn the_docs_is_empty_rewrites_vec_and_string_lengths_through_references_and_not_a_user_type() {
    let rules = example(1);
    assert!(rules.contains("(rewrite is-empty"), "{rules}");
    let lib = "pub struct Bag { items: Vec<u32> }\n\
        impl Bag { pub fn len(&self) -> usize { self.items.len() } }\n\
        macro_rules! id { ($($t:tt)*) => { $($t)* }; }\n\
        pub fn vec_empty(v: &Vec<u32>) -> bool { v.len() == 0 }\n\
        pub fn string_empty(s: String) -> bool { s.len() == 0 }\n\
        pub fn bag_empty(b: &Bag) -> bool { b.len() == 0 }\n\
        pub fn negated(a: Vec<u8>, b: &&Vec<u8>) -> bool { !(a.len() == 0) && b.len() == 0 }\n\
        pub fn through_macro(v: Vec<u8>) -> bool { id!(v.len() == 0) }\n";
    let fixture = format!("{STD}//- /lib.rs crate:lib deps:std edition:2021\n{lib}");
    let text = rewritten(&fixture, 1, rules);
    let lines: Vec<&str> = text.lines().skip(3).collect();
    assert_eq!(
        lines,
        [
            "pub fn vec_empty(v: &Vec<u32>) -> bool { v.is_empty() }",
            "pub fn string_empty(s: String) -> bool { s.is_empty() }",
            "pub fn bag_empty(b: &Bag) -> bool { b.len() == 0 }",
            "pub fn negated(a: Vec<u8>, b: &&Vec<u8>) -> bool { !(a.is_empty()) && b.is_empty() }",
            "pub fn through_macro(v: Vec<u8>) -> bool { id!(v.is_empty()) }",
        ],
    );
}
