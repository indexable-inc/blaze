//! Queries over one crate of the view beside two outside it, each test named for the property
//! it pins: paths match by what they resolve to, types and traits select by type, recursion
//! and negation reach their fixpoint.

use super::{lines, texts, view};
use crate::query;

const OUTSIDE: &str = r#"
//- /std.rs crate:std edition:2021
pub mod sync {
    mod mutex {
        pub struct Mutex(pub u32);
        impl Mutex {
            pub fn new(value: u32) -> Mutex { Mutex(value) }
        }
    }
    pub use self::mutex::Mutex;
}
pub mod alias {
    pub use crate::sync::Mutex as Lock;
}
//- /other.rs crate:other edition:2021
pub struct Mutex(pub u32);
impl Mutex {
    pub fn new(value: u32) -> Mutex { Mutex(value) }
}
"#;

const LIB: &str = r#"
mod direct { pub fn f(_: std::sync::Mutex) {} }
mod imported { use std::sync::Mutex; pub fn f(_: Mutex) {} }
mod aliased { use std::alias::Lock; pub fn f(_: Lock) {} }
mod globbed { use std::sync::*; pub fn f(_: Mutex) {} }
pub mod reexported { pub use std::sync::Mutex as Again; }
mod through { pub fn f(_: crate::reexported::Again) {} }
mod lookalike { use other::Mutex; pub fn f(_: Mutex) {} }
mod made {
    pub fn std_lock() { let _ = std::alias::Lock::new(1); }
    pub fn other_lock() { let _ = other::Mutex::new(2); }
}
"#;

fn locks() -> (base_db::Db, super::View) {
    let fixture = format!("{OUTSIDE}//- /lib.rs crate:lib deps:std,other edition:2021\n{LIB}");
    view(&fixture, 2, &["lib.rs"])
}

#[test]
fn an_alias_a_glob_and_a_reexport_match_the_item_they_name() {
    let (db, files) = locks();
    let outcome = query(&db, &files, r#"(rule (lock t) (type t "std::sync::Mutex"))"#).unwrap();
    assert_eq!(
        texts(&outcome, "lock"),
        [["std::sync::Mutex"], ["Mutex"], ["Lock"], ["Mutex"], ["crate::reexported::Again"]],
    );
    assert_eq!(lines(&outcome, "lock"), [2, 3, 4, 5, 7]);
}

#[test]
fn the_same_text_naming_another_crates_item_does_not_match() {
    let (db, files) = locks();
    let outcome = query(&db, &files, r#"(rule (lock t) (type t "std::sync::Mutex"))"#).unwrap();
    assert!(!lines(&outcome, "lock").contains(&8), "lookalike's `Mutex` is other's");
    let outcome = query(&db, &files, r#"(rule (lock t) (type t "other::Mutex"))"#).unwrap();
    assert_eq!(lines(&outcome, "lock"), [8]);
}

#[test]
fn an_associated_fn_matches_through_its_types_alias() {
    let (db, files) = locks();
    let rules = r#"(rule (made e x) (expr e "std::sync::Mutex::new($x)"))"#;
    let outcome = query(&db, &files, rules).unwrap();
    assert_eq!(texts(&outcome, "made"), [["std::alias::Lock::new(1)", "1"]]);
}

const TYPED: &str = r#"
//- /lib.rs crate:lib edition:2021
pub trait Shown {}
pub struct Meters(pub u32);
pub struct Feet(pub u32);
impl Shown for Meters {}
impl Meters { pub fn get(&self) -> u32 { self.0 } }
impl Feet { pub fn get(&self) -> u32 { self.0 } }
pub fn both(m: Meters, f: Feet) -> u32 {
    m.get() + f.get()
}
pub fn pair(p: (Meters, Feet)) -> u32 {
    p.0.get()
}
"#;

#[test]
fn a_type_constraint_selects_the_receiver_by_its_type() {
    let (db, files) = view(TYPED, 0, &["lib.rs"]);
    let rules = r#"
        (rule (meters e) (expr e "$r.get()") (type-of r t) (type-matches t "lib::Meters"))
        (rule (pairs p) (syntax p "IDENT_PAT") (type-of p t) (type-matches t "(lib::Meters, $b)"))
    "#;
    let outcome = query(&db, &files, rules).unwrap();
    assert_eq!(texts(&outcome, "meters"), [["m.get()"], ["p.0.get()"]]);
    assert_eq!(texts(&outcome, "pairs"), [["p"]]);
}

#[test]
fn a_trait_constraint_selects_the_types_that_implement_it() {
    let (db, files) = view(TYPED, 0, &["lib.rs"]);
    let rules = r#"(rule (shown e) (expr e "$r.get()") (type-of r t) (implements t "lib::Shown"))"#;
    let outcome = query(&db, &files, rules).unwrap();
    assert_eq!(texts(&outcome, "shown"), [["m.get()"], ["p.0.get()"]]);
}

const CALLS: &str = r#"
//- /lib.rs crate:lib edition:2021
pub fn sink() {}
pub fn near() { sink() }
pub fn far() { near() }
pub fn farther() { far(); helper() }
pub fn helper() {}
pub fn alone() {}
pub struct S;
impl S { pub fn method(&self) { far() } }
pub fn by_method(s: S) { s.method() }
"#;

const CALL_RULES: &str = r#"
(rule (calls f g) (syntax c "CALL_EXPR") (callee c g) (enclosing-fn c f))
(rule (calls f g) (syntax c "METHOD_CALL_EXPR") (callee c g) (enclosing-fn c f))
(rule (reaches f g) (calls f g))
(rule (reaches f g) (calls f h) (reaches h g))
(rule (to-sink f) (def-path s "lib::sink") (reaches f s))
(rule (called g) (calls f g))
(rule (fns f) (syntax n "FN") (defines n f))
(rule (uncalled f) (fns f) (not (called f)))
"#;

#[test]
fn recursion_finds_every_transitive_caller() {
    let (db, files) = view(CALLS, 0, &["lib.rs"]);
    let outcome = query(&db, &files, CALL_RULES).unwrap();
    assert_eq!(
        texts(&outcome, "to-sink"),
        [["lib::S::method"], ["lib::by_method"], ["lib::far"], ["lib::farther"], ["lib::near"]],
    );
}

#[test]
fn negation_finds_the_functions_nothing_calls() {
    let (db, files) = view(CALLS, 0, &["lib.rs"]);
    let outcome = query(&db, &files, CALL_RULES).unwrap();
    assert_eq!(texts(&outcome, "uncalled"), [["lib::alone"], ["lib::by_method"], ["lib::farther"]]);
}

#[test]
fn a_tree_sitter_match_is_a_load_error_naming_the_pattern_forms() {
    let (db, files) = view(CALLS, 0, &["lib.rs"]);
    let error = query(&db, &files, r#"(rule (x n) (match n "(call_expression)"))"#).unwrap_err();
    let shown = error.to_string();
    assert!(shown.contains("(expr n \"...\")") && shown.contains("(path n \"...\")"), "{shown}");
}

#[test]
fn a_pattern_that_reads_as_other_code_in_a_files_edition_is_refused_naming_it() {
    // In 2015 `async` is a name, so `async {}` is a struct literal; from 2018 it is a block.
    let fixture = "//- /lib.rs crate:lib edition:2015\n\
                   #[allow(non_camel_case_types)]\n\
                   pub struct async {}\n\
                   pub fn made() -> async { async {} }\n\
                   pub fn sum() -> u32 { 1 + 2 }\n";
    let (db, files) = view(fixture, 0, &["lib.rs"]);
    let error = match query(&db, &files, r#"(rule (made e) (expr e "async {}"))"#) {
        Ok(outcome) => panic!("matched as one edition reads it: {:?}", texts(&outcome, "made")),
        Err(error) => error.to_string(),
    };
    assert!(error.contains("rules:1") && error.contains("2015") && error.contains("lib.rs"), "{error}");
    // A pattern every edition reads alike matches a 2015 file.
    let outcome = query(&db, &files, r#"(rule (sum e) (expr e "1 + 2"))"#).unwrap();
    assert_eq!(texts(&outcome, "sum"), [["1 + 2"]]);
}

#[test]
fn a_metavariable_used_twice_binds_equal_code() {
    let fixture = r#"
//- /lib.rs crate:lib edition:2021
pub fn f(a: u32, b: u32) -> (u32, u32) {
    (a + a, a + b)
}
"#;
    let (db, files) = view(fixture, 0, &["lib.rs"]);
    let outcome = query(&db, &files, r#"(rule (double e x) (expr e "$x + $x"))"#).unwrap();
    assert_eq!(texts(&outcome, "double"), [["a + a", "a"]]);
}

// Causal: each test below takes the semantic constraint out of a rule above and shows the
// look-alike the constraint excluded now matches, so the constraint is what excluded it.

#[test]
fn without_resolution_the_lookalike_matches() {
    let (db, files) = locks();
    // A one-segment path matches by its text, the way a text search would.
    let outcome = query(&db, &files, r#"(rule (lock t) (type t "Mutex"))"#).unwrap();
    assert!(lines(&outcome, "lock").contains(&8), "other's `Mutex` is a look-alike by text");
    // The aliased and re-exported names are invisible to text.
    assert!(!lines(&outcome, "lock").contains(&4) && !lines(&outcome, "lock").contains(&7));
}

#[test]
fn without_the_type_constraint_the_other_receiver_matches() {
    let (db, files) = view(TYPED, 0, &["lib.rs"]);
    let outcome = query(&db, &files, r#"(rule (gets e) (expr e "$r.get()"))"#).unwrap();
    assert_eq!(texts(&outcome, "gets"), [["m.get()"], ["f.get()"], ["p.0.get()"]]);
}

#[test]
fn without_negation_the_called_functions_come_back() {
    let (db, files) = view(CALLS, 0, &["lib.rs"]);
    let outcome = query(&db, &files, CALL_RULES).unwrap();
    assert_eq!(texts(&outcome, "fns").len(), 8, "every fn, called or not");
}
