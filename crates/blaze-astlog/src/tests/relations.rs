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
