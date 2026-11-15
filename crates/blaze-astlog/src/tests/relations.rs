//! One fixture per relation and action the spec lists: a row it must give and a look-alike it
//! must not.

use super::rewrite::after;
use super::{lines, texts, view};
use crate::{edit, query};

const DEP: &str = r#"
//- /dep.rs crate:dep edition:2021
pub fn helper() {}
"#;

const LIB: &str = r#"
macro_rules! twice { ($e:expr) => { $e + $e }; }
macro_rules! make { () => { 7 + 8 }; }
pub mod outer {
    pub struct Thing;
    impl Thing {
        pub fn method(&self, n: u32, s: &str) -> u32 { n }
    }
    pub(crate) fn shared() {}
    fn hidden() {}
    #[inline]
    pub fn marked() {}
    pub fn unmarked() {}
}
pub fn target() {}
pub fn target_user() { target(); target(); }
pub mod lookalike { pub fn target() {} pub fn user() { target() } }
pub fn raw(p: *const u32) -> u32 { unsafe { *p } }
pub fn safe(p: &u32) -> u32 { *p }
#[test]
fn a_test() { let _ = 1 + 2; }
pub fn not_a_test() { let _ = 3 + 4; }
#[cfg(any())]
pub fn gone() { let _ = 5 + 6; }
pub fn input() -> u32 { twice!(1 * 2) }
pub fn made() -> u32 { make!() }
pub fn uses_dep() { dep::helper() }
"#;

fn lib() -> (base_db::Db, super::View) {
    let fixture = format!("{DEP}//- /lib.rs crate:lib deps:dep edition:2021\n{LIB}");
    view(&fixture, 1, &["lib.rs"])
}

/// The 1-based line of lib.rs that holds `text`.
fn line(text: &str) -> u32 {
    let at = LIB.lines().position(|it| it.contains(text)).expect("a line of the fixture");
    u32::try_from(at + 1).expect("a short fixture")
}

/// lib.rs after the rules' rewrites.
fn rewritten(rules: &str) -> String {
    let (db, files) = lib();
    let applied = edit(&db, &files, rules, &[]).unwrap();
    after(&files, &applied.edited.ops).swap_remove(0).1
}

#[test]
fn references_are_the_names_that_resolve_to_the_def_and_not_its_namesakes() {
    let (db, files) = lib();
    let rules = r#"(rule (refs n) (def-path d "lib::target") (references d n))"#;
    let outcome = query(&db, &files, rules).unwrap();
    let user = line("pub fn target_user()");
    assert_eq!(lines(&outcome, "refs"), [user, user], "the two calls in target_user");
    assert!(!lines(&outcome, "refs").contains(&line("pub mod lookalike")), "lookalike's `target` is its own");
}

#[test]
fn references_to_a_tuple_field_are_its_numbered_accesses() {
    let fixture = "//- /lib.rs crate:lib edition:2021\n\
                   pub struct Pair(pub u32, pub u32);\n\
                   pub fn first(p: &Pair) -> u32 { p.0 }\n\
                   pub fn second(p: &Pair) -> u32 { p.1 + 10 }\n\
                   pub fn again(p: &Pair) -> u32 { p.0 + 20 }\n";
    let (db, files) = view(fixture, 0, &["lib.rs"]);
    let rules = r#"(rule (refs n) (expr e "$p.0") (resolves e d) (references d n))"#;
    let outcome = query(&db, &files, rules).unwrap();
    // `p.1` is the other field, and the `0` of `10` and `20` is no name.
    assert_eq!(lines(&outcome, "refs"), [2, 4], "{:?}", texts(&outcome, "refs"));
}

#[test]
fn in_unsafe_holds_inside_an_unsafe_block_only() {
    let (db, files) = lib();
    let rules = r#"(rule (deref e) (syntax e "PREFIX_EXPR") (in-unsafe e))"#;
    let outcome = query(&db, &files, rules).unwrap();
    assert_eq!(lines(&outcome, "deref"), [line("pub fn raw")], "safe's `*p` is the same text");
}

#[test]
fn in_test_holds_inside_a_test_fn_only() {
    let (db, files) = lib();
    let rules = r#"(rule (tested e) (syntax e "BIN_EXPR") (in-test e))"#;
    let outcome = query(&db, &files, rules).unwrap();
    assert_eq!(texts(&outcome, "tested"), [["1 + 2"]], "not_a_test's `3 + 4` is no test");
}

#[test]
fn in_test_reads_a_test_attributes_path_apart_from_its_arguments() {
    let fixture = "//- /lib.rs crate:lib edition:2021\n\
                   #[tokio::test(flavor = \"multi_thread\")]\n\
                   async fn threaded() { let _ = 1 + 2; }\n\
                   #[cfg(test)]\n\
                   mod tests { fn helper() { let _ = 3 + 4; } }\n\
                   #[testing::other(test)]\n\
                   fn lookalike() { let _ = 5 + 6; }\n\
                   #[cfg(not(test))]\n\
                   fn shipped() { let _ = 7 + 8; }\n";
    let (db, files) = view(fixture, 0, &["lib.rs"]);
    let rules = r#"(rule (tested e) (syntax e "BIN_EXPR") (in-test e))"#;
    let outcome = query(&db, &files, rules).unwrap();
    assert_eq!(texts(&outcome, "tested"), [["1 + 2"], ["3 + 4"]], "`test` as an argument names no test");
}

#[test]
fn cfg_off_holds_inside_an_item_its_cfg_removes_only() {
    let (db, files) = lib();
    let rules = r#"(rule (off e) (syntax e "BIN_EXPR") (cfg-off e))"#;
    let outcome = query(&db, &files, rules).unwrap();
    assert_eq!(texts(&outcome, "off"), [["5 + 6"]], "the #[test] fn's `1 + 2` is compiled");
}

#[test]
fn cfg_off_holds_inside_a_variant_or_a_field_its_cfg_removes() {
    let fixture = "//- /lib.rs crate:lib edition:2021\n\
                   pub enum E { On = 1 + 2, #[cfg(any())] Off = 3 + 4 }\n\
                   pub struct Named { pub on: [u8; 5 + 6], #[cfg(any())] pub off: [u8; 7 + 8] }\n\
                   pub struct Tuple(pub [u8; 9 + 1], #[cfg(any())] pub [u8; 2 + 3]);\n";
    let (db, files) = view(fixture, 0, &["lib.rs"]);
    let rules = r#"(rule (off e) (syntax e "BIN_EXPR") (cfg-off e))"#;
    let outcome = query(&db, &files, rules).unwrap();
    assert_eq!(texts(&outcome, "off"), [["3 + 4"], ["7 + 8"], ["2 + 3"]], "the compiled ones are no cfg-off");
    // A statement's cfg has no def to tell whether it is on: refused, not guessed.
    let fixture = "//- /lib.rs crate:lib edition:2021\npub fn f() { #[cfg(any())] let _ = 4 + 5; }\n";
    let (db, files) = view(fixture, 0, &["lib.rs"]);
    let error = match query(&db, &files, rules) {
        Ok(outcome) => panic!("a statement's cfg was read: {:?}", texts(&outcome, "off")),
        Err(error) => error.to_string(),
    };
    assert!(error.contains("rules:1") && error.contains("`#[cfg(any())]` at lib.rs:1"), "{error}");
}

#[test]
fn in_macro_input_holds_for_an_expansions_nodes_from_the_input_only() {
    let (db, files) = lib();
    let rules = r#"(rule (from-input e) (syntax e "BIN_EXPR") (in-macro-input e))"#;
    let outcome = query(&db, &files, rules).unwrap();
    let found = texts(&outcome, "from-input");
    assert!(!found.is_empty() && found.iter().all(|it| it == &["1 * 2"]), "{found:?}");
    assert!(!lines(&outcome, "from-input").contains(&line("pub fn not_a_test")), "written code is no expansion");
    let made = query(&db, &files, r#"(rule (made e) (syntax e "BIN_EXPR") (in-macro-expansion e))"#).unwrap();
    // A node the macro made has no text in the file: its cell carries the expansion's own.
    assert!(texts(&made, "made").contains(&vec!["7+8".to_owned()]), "make!'s sum is the macro's own");
    assert!(!found.contains(&vec!["7+8".to_owned()]), "{found:?}");
}

#[test]
fn a_pattern_finds_a_match_whose_literal_only_a_macro_expansion_holds() {
    let fixture = "//- /dep.rs crate:dep edition:2021\n\
                   #[macro_export]\n\
                   macro_rules! answer { () => { 40 + 2 }; }\n\
                   //- /lib.rs crate:lib deps:dep edition:2021\n\
                   pub fn asked() -> u32 { dep::answer!() }\n";
    let (db, files) = view(fixture, 1, &["lib.rs"]);
    let outcome = query(&db, &files, r#"(rule (sum e) (expr e "40 + 2"))"#).unwrap();
    let found = texts(&outcome, "sum");
    // lib.rs never spells `40`: only the expansion of `answer!()` does.
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0][0].replace(' ', ""), "40+2", "{found:?}");
}

#[test]
fn visibility_names_each_fns_own() {
    let (db, files) = lib();
    let rules = r#"(rule (vis d v) (syntax n "FN") (defines n d) (visibility d v))"#;
    let outcome = query(&db, &files, rules).unwrap();
    let found = texts(&outcome, "vis");
    let has = |path: &str, shown: &str| found.contains(&vec![path.to_owned(), shown.to_owned()]);
    assert!(has("lib::outer::shared", "pub(crate)") && has("lib::outer::hidden", "private"), "{found:?}");
    assert!(has("lib::outer::marked", "pub"), "{found:?}");
    assert!(!has("lib::outer::hidden", "pub") && !has("lib::outer::shared", "pub"), "{found:?}");
}

#[test]
fn attribute_holds_for_the_item_that_carries_it_only() {
    let (db, files) = lib();
    let rules = r#"(rule (inlined d) (syntax n "FN") (defines n d) (attribute d "inline"))"#;
    let outcome = query(&db, &files, rules).unwrap();
    assert_eq!(texts(&outcome, "inlined"), [["lib::outer::marked"]], "unmarked carries none");
    // A cfg'd-out fn has no def: its syntax carries the attribute, and `#[cfg(...)]` parses
    // with no path node, so the name is read from the attribute's text.
    let rules = r#"(rule (cfgd n) (syntax n "FN") (attribute n "cfg"))"#;
    let outcome = query(&db, &files, rules).unwrap();
    assert_eq!(lines(&outcome, "cfgd"), [line("#[cfg(any())]")], "the #[test] fn is no cfg");
}

#[test]
fn defines_maps_a_macro_rules_to_its_macro_and_back() {
    let (db, files) = lib();
    let rules = r#"
        (rule (macros d k) (syntax n "MACRO_RULES") (defines n d) (def-kind d k))
        (rule (back n) (syntax m "MACRO_RULES") (defines m d) (defines n d))
    "#;
    let outcome = query(&db, &files, rules).unwrap();
    let mut found = texts(&outcome, "macros");
    found.sort();
    assert_eq!(found, [["lib::make", "macro"], ["lib::twice", "macro"]]);
    assert_eq!(lines(&outcome, "back"), [line("macro_rules! twice"), line("macro_rules! make")]);
}

#[test]
fn fn_param_gives_each_parameters_type_by_index_with_self_first() {
    let (db, files) = lib();
    let rules = r#"
        (rule (param i t) (def-path d "lib::outer::Thing::method") (fn-param d i t))
        (rule (none i t) (def-path d "lib::outer::shared") (fn-param d i t))
    "#;
    let outcome = query(&db, &files, rules).unwrap();
    let mut found = texts(&outcome, "param");
    found.sort();
    assert_eq!(found, [["0", "&Thing"], ["1", "u32"], ["2", "&str"]]);
    assert!(texts(&outcome, "none").is_empty(), "shared takes nothing");
}

#[test]
fn def_kind_names_what_each_def_is() {
    let (db, files) = lib();
    let rules = r#"
        (rule (kinds d k) (def-path d "lib::outer::Thing") (def-kind d k))
        (rule (kinds d k) (def-path d "lib::outer") (def-kind d k))
        (rule (kinds d k) (def-path d "lib::target") (def-kind d k))
    "#;
    let outcome = query(&db, &files, rules).unwrap();
    let mut found = texts(&outcome, "kinds");
    found.sort();
    assert_eq!(found, [["lib::outer", "module"], ["lib::outer::Thing", "struct"], ["lib::target", "fn"]]);
}

#[test]
fn def_name_is_the_last_segment_and_an_impl_has_none() {
    let fixture = "//- /lib.rs crate:lib edition:2021\n\
                   pub struct S { pub field: u32 }\n\
                   pub struct P(pub u8);\n\
                   impl S { pub fn legacy_make() -> S { S { field: 0 } } }\n";
    let (db, files) = view(fixture, 0, &["lib.rs"]);
    let rules = r#"
        (rule (named d t) (syntax n "FN") (defines n d) (def-name d t))
        (rule (named d t) (def-path d "lib::S::field") (def-name d t))
        (rule (named d t) (def-path d "lib::P::0") (def-name d t))
        (rule (impls t) (syntax n "IMPL") (defines n d) (def-name d t))
    "#;
    let outcome = query(&db, &files, rules).unwrap();
    let mut found = texts(&outcome, "named");
    found.sort();
    assert_eq!(found, [["lib::P::0", "0"], ["lib::S::field", "field"], ["lib::S::legacy_make", "legacy_make"]]);
    assert!(texts(&outcome, "impls").is_empty());
}

#[test]
fn each_impl_has_a_path_of_its_own_that_names_no_def_back() {
    let fixture = "//- /lib.rs crate:lib edition:2021\n\
                   pub struct S;\n\
                   pub trait T {}\n\
                   pub trait G<X> {}\n\
                   impl S { pub fn f() {} }\n\
                   impl T for S {}\n\
                   impl G<u8> for S {}\n\
                   impl G<u16> for S {}\n";
    let (db, files) = view(fixture, 0, &["lib.rs"]);
    let rules = r#"(rule (impls p) (syntax n "IMPL") (defines n d) (def-path d p))"#;
    let mut found = texts(&query(&db, &files, rules).unwrap(), "impls");
    found.sort();
    assert_eq!(found, [["lib::<impl G<u16> for S>"], ["lib::<impl G<u8> for S>"], ["lib::<impl S>"], ["lib::<impl T for S>"]]);
    let refused = |rules: &str| match query(&db, &files, rules) {
        Ok(_) => panic!("an impl's path named a def: {rules}"),
        Err(error) => error.to_string(),
    };
    let error = refused(r#"(rule (named d) (def-path d "lib::<impl S>"))"#);
    assert!(error.contains("rules:1") && error.contains("an impl has no path"), "{error}");
    let error = refused(r#"(rule (named d) (syntax n "IMPL") (defines n e) (def-path e p) (def-path d p))"#);
    assert!(error.contains("rules:1") && error.contains("an impl has no path"), "{error}");
}

#[test]
fn crate_of_names_a_defs_crate_and_a_nodes_own() {
    let (db, files) = lib();
    let rules = r#"
        (rule (home c) (expr e "dep::helper()") (callee e d) (crate-of d c))
        (rule (here c) (expr e "dep::helper()") (crate-of e c))
    "#;
    let outcome = query(&db, &files, rules).unwrap();
    assert_eq!(texts(&outcome, "home"), [["dep"]]);
    assert_eq!(texts(&outcome, "here"), [["lib"]], "the call is written in lib");
}

#[test]
fn callee_sees_through_parentheses_around_the_called_path() {
    let fixture = "//- /lib.rs crate:lib edition:2021\n\
                   pub fn foo() {}\n\
                   pub fn bar() { (foo)(); ((foo))(); }\n";
    let (db, files) = view(fixture, 0, &["lib.rs"]);
    let rules = r#"(rule (called e d) (syntax e "CALL_EXPR") (callee e d))"#;
    let outcome = query(&db, &files, rules).unwrap();
    assert_eq!(texts(&outcome, "called"), [["(foo)()", "lib::foo"], ["((foo))()", "lib::foo"]]);
}

#[test]
fn a_callee_in_an_impl_on_a_type_no_path_names_has_a_def_path_through_the_impl() {
    let fixture = "//- /lib.rs crate:lib edition:2021\n\
                   pub trait Go { fn go(&self) -> u8; }\n\
                   impl Go for u32 { fn go(&self) -> u8 { 1 } }\n\
                   impl Go for &str { fn go(&self) -> u8 { 2 } }\n\
                   pub struct S;\n\
                   impl S { pub fn own(&self) -> u8 { 3 } }\n\
                   pub fn calls(s: S) -> u8 { 1u32.go() + \"a\".go() + s.own() }\n";
    let (db, files) = view(fixture, 0, &["lib.rs"]);
    let rules = r#"(rule (called e p) (syntax e "METHOD_CALL_EXPR") (callee e d) (def-path d p))"#;
    let outcome = query(&db, &files, rules).unwrap();
    assert_eq!(
        texts(&outcome, "called"),
        [
            ["1u32.go()", "lib::<impl Go for u32>::go"],
            ["\"a\".go()", "lib::<impl Go for &'_ str>::go"],
            ["s.own()", "lib::S::own"],
        ]
    );
}

#[test]
fn enclosing_impl_holds_for_code_in_an_impl_only() {
    let (db, files) = lib();
    let rules = r#"(rule (in-impl e) (syntax e "PATH_EXPR") (enclosing-impl e i))"#;
    let outcome = query(&db, &files, rules).unwrap();
    assert_eq!(lines(&outcome, "in-impl"), [line("pub fn method")], "target_user's paths are in no impl");
}

#[test]
fn enclosing_module_is_the_innermost_module() {
    let (db, files) = lib();
    let rules = r#"(rule (in-mod e m) (expr e "target()") (enclosing-module e m))"#;
    let outcome = query(&db, &files, rules).unwrap();
    let found = texts(&outcome, "in-mod");
    assert_eq!(found, [["target()", "lib"], ["target()", "lib"], ["target()", "lib::lookalike"]]);
}

#[test]
fn def_path_names_a_field_from_its_def_and_its_def_from_the_path() {
    let fixture = "//- /lib.rs crate:lib edition:2021\n\
                   pub struct S { pub field: u32 }\n\
                   pub enum E { V { inner: u8 } }\n\
                   pub struct P(pub u8);\n";
    let (db, files) = view(fixture, 0, &["lib.rs"]);
    let rules = r#"
        (rule (paths p) (syntax f "RECORD_FIELD") (defines f d) (def-path d p))
        (rule (named k p) (def-path d "lib::S::field") (def-kind d k) (def-path d p))
        (rule (named k p) (def-path d "lib::E::V::inner") (def-kind d k) (def-path d p))
        (rule (named k p) (def-path d "lib::P::0") (def-kind d k) (def-path d p))
    "#;
    let outcome = query(&db, &files, rules).unwrap();
    assert_eq!(texts(&outcome, "paths"), [["lib::E::V::inner"], ["lib::S::field"]], "a variant's field is under its enum");
    let mut named = texts(&outcome, "named");
    named.sort();
    assert_eq!(named, [["field", "lib::E::V::inner"], ["field", "lib::P::0"], ["field", "lib::S::field"]]);
}

const NOT_A_TEST: &str = r#"(rule (stmt-of s) (stmt s "let _ = 3 + 4;"))"#;

#[test]
fn delete_removes_the_matched_node_and_nothing_like_it() {
    let text = rewritten(&format!("{NOT_A_TEST}(rewrite dropped (stmt-of s) (delete s))"));
    assert!(text.contains("pub fn not_a_test() {  }"), "{text}");
    assert!(text.contains("fn a_test() { let _ = 1 + 2; }"), "{text}");
}

#[test]
fn insert_before_writes_ahead_of_the_matched_node_only() {
    let text = rewritten(&format!("{NOT_A_TEST}(rewrite ahead (stmt-of s) (insert-before s \"let _ = 0; \"))"));
    assert!(text.contains("pub fn not_a_test() { let _ = 0; let _ = 3 + 4; }"), "{text}");
    assert!(text.contains("fn a_test() { let _ = 1 + 2; }"), "{text}");
}

#[test]
fn insert_after_writes_behind_the_matched_node_only() {
    let text = rewritten(&format!("{NOT_A_TEST}(rewrite behind (stmt-of s) (insert-after s \" let _ = 9;\"))"));
    assert!(text.contains("pub fn not_a_test() { let _ = 3 + 4; let _ = 9; }"), "{text}");
    assert!(text.contains("fn a_test() { let _ = 1 + 2; }"), "{text}");
}

#[test]
fn add_use_imports_the_path_once_into_the_nodes_module() {
    let rules = r#"
        (rule (call e) (expr e "dep::helper()"))
        (rewrite imported (call e) (add-use e "dep::helper"))
    "#;
    let text = rewritten(rules);
    assert_eq!(text.matches("use dep::helper;").count(), 1, "{text}");
    let module = text.find("pub mod outer {").expect("outer");
    assert!(text.find("use dep::helper;").is_some_and(|at| at < module), "into the crate root, not outer: {text}");
}

const ARMS: &str = r#"
//- /lib.rs crate:lib edition:2021
pub enum E { A, B(u8) }
pub fn pick(n: u8) -> E {
    match n {
        0 => E::A,
        k if k > 9 => E::B(k),
        _ => E::B(1),
    }
}
pub fn other(n: u8) -> u8 { match n { 7 => 1, _ => 2 } }
"#;

/// `match-arm` gives each arm's match, pattern and expression from the match, from the arm, or
/// from neither; its guard is no part of the pattern.
#[test]
fn match_arm_gives_each_arms_match_pattern_and_expression_from_either_end() {
    let (db, files) = view(ARMS, 0, &["lib.rs"]);
    let rules = r#"
        (rule (every p b) (match-arm m a p b))
        (rule (of-match p b) (syntax m "MATCH_EXPR") (text-match m "^match n [{] 7") (match-arm m a p b))
        (rule (of-arm m) (syntax a "MATCH_ARM") (text-match a "^k if") (match-arm m a p b))
    "#;
    let outcome = query(&db, &files, rules).unwrap();
    let every = [["0", "E::A"], ["k", "E::B(k)"], ["_", "E::B(1)"], ["7", "1"], ["_", "2"]];
    assert_eq!(texts(&outcome, "every"), every);
    assert_eq!(texts(&outcome, "of-match"), [["7", "1"], ["_", "2"]]);
    let of_arm = texts(&outcome, "of-arm");
    assert_eq!(of_arm.len(), 1, "{of_arm:?}");
    assert!(of_arm[0][0].starts_with("match n {\n        0 => E::A,"), "{of_arm:?}");
}

/// `child` gives a node's children in order, and `descendant` every node below it, never the
/// node itself.
#[test]
fn child_and_descendant_walk_down_from_a_bound_node() {
    let (db, files) = view(ARMS, 0, &["lib.rs"]);
    let rules = r#"
        (rule (guarded a) (syntax a "MATCH_ARM") (text-match a "^k if"))
        (rule (kids k) (guarded a) (child a c) (kind c k))
        (rule (lits l) (syntax m "MATCH_EXPR") (text-match m "^match n [{] 7") (descendant m l) (kind l "LITERAL"))
        (rule (self-below m) (syntax m "MATCH_EXPR") (descendant m m))
    "#;
    let outcome = query(&db, &files, rules).unwrap();
    let mut kids = texts(&outcome, "kids");
    kids.sort();
    assert_eq!(kids, [["CALL_EXPR"], ["IDENT_PAT"], ["MATCH_GUARD"]]);
    assert_eq!(texts(&outcome, "lits"), [["7"], ["1"], ["2"]]);
    assert!(texts(&outcome, "self-below").is_empty());
}

/// `int-value` reads an integer literal in any radix, with `_`s and a suffix, and a negated one
/// in an expression or a pattern; one past `i64` has no row. `str-value` unescapes a string,
/// raw ones too.
#[test]
fn int_value_and_str_value_read_what_a_literal_means() {
    let fixture = r##"
//- /lib.rs crate:lib edition:2021
pub const A: i64 = 0x1F;
pub const B: u32 = 1_000u32;
pub const C: i64 = -7;
pub fn f(n: i64) -> &'static str { match n { -2 => "neg\ttab", 3 => r#"raw "q""#, _ => "x" } }
pub const BIG: u64 = 18446744073709551615;
"##;
    let (db, files) = view(fixture, 0, &["lib.rs"]);
    let rules = r#"
        (rule (ints n i) (syntax n "LITERAL") (int-value n i))
        (rule (negated n i) (syntax n "PREFIX_EXPR") (int-value n i))
        (rule (pats n i) (syntax n "LITERAL_PAT") (int-value n i))
        (rule (strs s) (syntax n "LITERAL") (str-value n s))
    "#;
    let outcome = query(&db, &files, rules).unwrap();
    let ints = [["0x1F", "31"], ["1_000u32", "1000"], ["7", "7"], ["2", "2"], ["3", "3"]];
    assert_eq!(texts(&outcome, "ints"), ints);
    assert_eq!(texts(&outcome, "negated"), [["-7", "-7"]]);
    assert_eq!(texts(&outcome, "pats"), [["-2", "-2"], ["3", "3"]]);
    assert_eq!(texts(&outcome, "strs"), [["neg\ttab"], ["raw \"q\""], ["x"]]);
}

const OWNED: &str = r#"
//- /lib.rs crate:lib edition:2021
pub enum E { A, B(u8), C { x: u8 } }
pub struct S { pub f: u8 }
pub trait T { fn t(&self); }
impl S { pub fn m(&self) {} }
impl T for S { fn t(&self) {} }
pub mod inner { pub fn g() {} }
"#;

/// `def-parent` goes up from a def to what owns it, and down from an owner to each def it owns;
/// `variant-count` counts an enum's variants and nothing else's.
#[test]
fn def_parent_joins_a_def_and_its_owner_both_ways_and_variant_count_counts_an_enum() {
    let (db, files) = view(OWNED, 0, &["lib.rs"]);
    let rules = r#"
        (rule (up d p) (def-path d "lib::E::B") (def-parent d p))
        (rule (up d p) (def-path d "lib::E::C::x") (def-parent d p))
        (rule (up d p) (def-path d "lib::S::m") (def-parent d p))
        (rule (up d p) (def-path d "lib::T::t") (def-parent d p))
        (rule (up d p) (def-path d "lib::inner::g") (def-parent d p))
        (rule (up d p) (def-path d "lib::inner") (def-parent d p))
        (rule (up d p) (syntax n "IMPL") (defines n d) (def-parent d p))
        (rule (root p) (def-path d "lib") (def-parent d p))
        (rule (down p d) (def-path p "lib::E") (def-parent d p))
        (rule (down p d) (def-path p "lib::S") (def-parent d p))
        (rule (down p d) (def-path p "lib::inner") (def-parent d p))
        (rule (down p d) (def-path p "lib::T") (def-parent d p))
        (rule (variants e n) (def-path e "lib::E") (variant-count e n))
        (rule (variants e n) (def-path e "lib::S") (variant-count e n))
    "#;
    let outcome = query(&db, &files, rules).unwrap();
    let mut up = texts(&outcome, "up");
    up.sort();
    let want = [
        ["lib::<impl S>", "lib"],
        ["lib::<impl T for S>", "lib"],
        ["lib::E::B", "lib::E"],
        ["lib::E::C::x", "lib::E::C"],
        ["lib::S::m", "lib::<impl S>"],
        ["lib::T::t", "lib::T"],
        ["lib::inner", "lib"],
        ["lib::inner::g", "lib::inner"],
    ];
    assert_eq!(up, want);
    assert!(texts(&outcome, "root").is_empty(), "a crate's root has no owner");
    let mut down = texts(&outcome, "down");
    down.sort();
    let want = [
        ["lib::E", "lib::E::A"],
        ["lib::E", "lib::E::B"],
        ["lib::E", "lib::E::C"],
        ["lib::S", "lib::S::f"],
        ["lib::T", "lib::T::t"],
        ["lib::inner", "lib::inner::g"],
    ];
    assert_eq!(down, want);
    assert_eq!(texts(&outcome, "variants"), [["lib::E", "3"]]);
}

/// `type-kind` names a type's shape, and `type-def` the ADT or the `dyn` trait it is; a
/// reference is its own kind until `peel-refs` strips it.
#[test]
fn type_kind_names_a_types_shape_and_type_def_its_adt_or_trait() {
    let fixture = r#"
//- /lib.rs crate:lib edition:2021
pub struct S;
pub enum E { A }
pub union U { a: u8 }
pub trait T {}
pub fn f(s: S, e: E, uu: U, r: &S, p: *const u8, sl: &[u8], a: [u8; 2], t: (u8, i8), st: &str, b: bool, c: char, i: i32, u: usize, fl: f64, d: &dyn T) {}
"#;
    let (db, files) = view(fixture, 0, &["lib.rs"]);
    let rules = r#"
        (rule (param p t) (syntax n "PARAM") (child n p) (kind p "IDENT_PAT") (type-of p t))
        (rule (kinds p k) (param p t) (type-kind t k))
        (rule (peeled p k) (param p t) (peel-refs t u) (type-kind u k))
        (rule (defs p d) (param p t) (type-def t d))
        (rule (peeled-defs p d) (param p t) (peel-refs t u) (type-def u d))
    "#;
    let outcome = query(&db, &files, rules).unwrap();
    let kinds = [
        "struct", "enum", "union", "ref", "ptr", "ref", "array", "tuple", "ref", "bool", "char", "int",
        "uint", "float", "ref",
    ];
    let found: Vec<String> = texts(&outcome, "kinds").into_iter().map(|it| it[1].clone()).collect();
    assert_eq!(found, kinds);
    let peeled = texts(&outcome, "peeled");
    let of = |name: &str| peeled.iter().find(|it| it[0] == name).map(|it| it[1].clone());
    assert_eq!([of("sl"), of("st"), of("d")], [Some("slice".to_owned()), Some("str".to_owned()), Some("dyn".to_owned())]);
    assert_eq!(texts(&outcome, "defs"), [["s", "lib::S"], ["e", "lib::E"], ["uu", "lib::U"]]);
    let peeled = [["s", "lib::S"], ["e", "lib::E"], ["uu", "lib::U"], ["r", "lib::S"], ["d", "lib::T"]];
    assert_eq!(texts(&outcome, "peeled-defs"), peeled);
}
