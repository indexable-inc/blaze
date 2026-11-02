//! Name resolution's lints (`unused_imports`, `redundant_imports`, `unused_macros`,
//! `unused_macro_rules`, `unused_qualifications`, `ambiguous_glob_reexports`,
//! `hidden_glob_reexports`) against rustc's UI tests, and imports of another crate's items.

use crate::{
    Options,
    emit::Style,
    lints::CommandLine,
    tests::{Mode, assert_reports, checked_with, compiletest, error, normalized, ui},
};

/// What rustc writes compiling `fixture` (its files named `$DIR/<path>`) as a UI test that is
/// not `//@ run-pass`.
fn ui_fixture(fixture: &str) -> String {
    let checked = checked_with(fixture, "$DIR/", &compiletest(Mode::Other, &Options::default()));
    normalized(&checked.stderr_with(Style { ui_testing: true, ..Style::default() }))
}

/// The messages of `fixture`'s diagnostics checked with the lint flags `args`, those of `lint`.
fn messages(fixture: &str, args: &[&str], lint: &str) -> Vec<String> {
    let args: Vec<String> = args.iter().map(|it| (*it).to_owned()).collect();
    let options = Options { lints: CommandLine::parse(&args).unwrap(), ..Options::default() };
    let prefix = format!("{lint}: ");
    checked_with(fixture, "", &options).messages().into_iter().filter(|it| it.starts_with(&prefix)).collect()
}

#[test]
fn unused_macros() {
    let source = include_str!("imports/unused-macros.rs");
    let expected = include_str!("imports/unused-macros.stderr");
    assert_eq!(ui("unused-macros.rs", "2015", source), expected);
}

#[test]
fn unused_macros_decl() {
    let source = include_str!("imports/unused-macros-decl.rs");
    let expected = include_str!("imports/unused-macros-decl.stderr");
    assert_eq!(ui("unused-macros-decl.rs", "2015", source), expected);
}

#[test]
fn unused_macro_rules() {
    let source = include_str!("imports/unused-macro-rules.rs");
    let expected = include_str!("imports/unused-macro-rules.stderr");
    assert_eq!(ui("unused-macro-rules.rs", "2015", source), expected);
}

#[test]
fn unused_macro_rules_compile_error() {
    let source = include_str!("imports/unused-macro-rules-compile-error.rs");
    let expected = include_str!("imports/unused-macro-rules-compile-error.stderr");
    assert_eq!(ui("unused-macro-rules-compile-error.rs", "2015", source), expected);
}

#[test]
fn unused_import_through_a_glob_of_an_allowed_module() {
    let source = include_str!("imports/unused.rs");
    let expected = include_str!("imports/unused.stderr");
    assert_eq!(ui("unused.rs", "2015", source), expected);
}

#[test]
fn unused_macro_use() {
    let source = include_str!("imports/unused-macro-use.rs");
    let expected = include_str!("imports/unused-macro-use.stderr");
    let fixture = format!("//- minicore: panic\n//- /unused-macro-use.rs crate:other:main edition:2015\n{source}");
    assert_eq!(ui_fixture(&fixture), expected);
}

#[test]
fn unused_import_with_attributes() {
    let source = include_str!("imports/unused-import-issue-87973.rs");
    let fixture = format!(
        "//- /std.rs crate:std edition:2021 library\npub mod fs {{}}\npub mod prelude {{ pub mod rust_2015 {{}} }}\n\
         //- /unused-import-issue-87973.rs crate:other:main edition:2015 deps:std\n{source}"
    );
    assert_reports(&ui_fixture(&fixture), &[error(8, &["`std::fs`"])]);
}

#[test]
fn glob_reexporting_nothing_public_enough() {
    let source = include_str!("imports/no-pub-reexports-but-used.rs");
    let expected = include_str!("imports/no-pub-reexports-but-used.stderr");
    assert_eq!(ui("no-pub-reexports-but-used.rs", "2015", source), expected);
}

#[test]
fn unused_qualifications_suggestion() {
    let source = include_str!("imports/unused-qualifications-suggestion.rs");
    let expected = include_str!("imports/unused-qualifications-suggestion.stderr");
    assert_eq!(ui("unused-qualifications-suggestion.rs", "2015", source), expected);
}

/// `unused-qualifications-global-paths.rs`, which passes with no output: a `::` path is not
/// linted.
#[test]
fn unused_qualifications_not_of_global_paths() {
    let source = include_str!("imports/unused-qualifications-global-paths.rs");
    let fixture = format!(
        "//- minicore: default\n//- /std.rs crate:std edition:2021 library\npub use core::default;\npub mod prelude {{ pub mod rust_2015 {{}} }}\n\
         //- /unused-qualifications-global-paths.rs crate:other:main edition:2015 deps:std\n{source}"
    );
    let stderr = ui_fixture(&fixture);
    assert!(!stderr.contains("unnecessary qualification"), "{stderr}");
}

#[test]
fn use_redundant() {
    let source = include_str!("imports/use-redundant.rs");
    let expected = include_str!("imports/use-redundant.stderr");
    assert_eq!(ui("use-redundant.rs", "2015", source), expected);
}

#[test]
fn use_redundant_glob_parent() {
    let source = include_str!("imports/use-redundant-glob-parent.rs");
    let expected = include_str!("imports/use-redundant-glob-parent.stderr");
    assert_eq!(ui("use-redundant-glob-parent.rs", "2015", source), expected);
}

#[test]
fn use_redundant_not_parent() {
    let source = include_str!("imports/use-redundant-not-parent.rs");
    assert_eq!(ui("use-redundant-not-parent.rs", "2015", source), "");
}

#[test]
fn use_redundant_multiple_namespaces() {
    let source = include_str!("imports/use-redundant-multiple-namespaces.rs");
    assert_eq!(ui("use-redundant-multiple-namespaces.rs", "2015", source), "");
}

#[test]
fn hidden_glob_reexports() {
    let source = include_str!("imports/hidden_glob_reexports.rs");
    let expected = include_str!("imports/hidden_glob_reexports.stderr");
    let fixture = format!(
        "//- /std.rs crate:std edition:2021 library\npub mod primitive {{ pub use u8; }}\n\
         pub mod prelude {{ pub mod rust_2015 {{}} pub mod rust_2018 {{}} pub mod rust_2021 {{}} pub mod rust_2024 {{}} }}\n\
         //- /hidden_glob_reexports.rs crate:other:main edition:2015 deps:std\n{source}"
    );
    assert_eq!(ui_fixture(&fixture), expected);
}

/// `issue-107563-ambiguous-glob-reexports.rs`'s `ambiguous_glob_reexports`, as its `.stderr`
/// has them; its `E0659` is name resolution's error, not a lint.
#[test]
fn ambiguous_glob_reexports() {
    let source = include_str!("imports/issue-107563-ambiguous-glob-reexports.rs");
    let stderr = ui("issue-107563-ambiguous-glob-reexports.rs", "2015", source);
    let first = "error: ambiguous glob re-exports
  --> $DIR/issue-107563-ambiguous-glob-reexports.rs:13:9
   |
LL | pub use foo::*;
   |         ^^^^^^ the name `X` in the type namespace is first re-exported here
LL |
LL | pub use bar::*;
   |         ------ but the name `X` in the type namespace is also re-exported here
   |
note: the lint level is defined here
  --> $DIR/issue-107563-ambiguous-glob-reexports.rs:2:9
   |
LL | #![deny(ambiguous_glob_reexports)]
   |         ^^^^^^^^^^^^^^^^^^^^^^^^
";
    let second = "error: ambiguous glob re-exports
  --> $DIR/issue-107563-ambiguous-glob-reexports.rs:20:13
   |
LL |     pub use self::m1::*;
   |             ^^^^^^^^^^^ the name `A` in the type namespace is first re-exported here
LL |
LL |     pub use self::m2::*;
   |             ----------- but the name `A` in the type namespace is also re-exported here
";
    assert!(stderr.contains(first), "{stderr}");
    assert!(stderr.contains(second), "{stderr}");
    assert!(stderr.find(first) < stderr.find(second), "{stderr}");
}

const DEP: &str = "//- /dep.rs crate:dep edition:2021 library
pub fn f() {}
pub fn g() {}
pub mod m {
    pub fn h() {}
}
";

#[test]
fn unused_import_of_a_dependency_s_item() {
    let fixture = format!(
        "{DEP}//- /main.rs crate:other:main edition:2021 deps:dep
use dep::f;
use dep::g;
fn main() {{
    g();
}}
"
    );
    assert_eq!(messages(&fixture, &[], "unused_imports"), ["unused_imports: unused import: `dep::f`"]);
}

#[test]
fn used_import_of_a_dependency_s_item() {
    let fixture = format!(
        "{DEP}//- /main.rs crate:other:main edition:2021 deps:dep
use dep::{{f, g}};
fn main() {{
    f();
    g();
}}
"
    );
    assert_eq!(messages(&fixture, &[], "unused_imports"), Vec::<String>::new());
}

#[test]
fn glob_of_a_dependency_s_module() {
    let used = format!(
        "{DEP}//- /main.rs crate:other:main edition:2021 deps:dep
use dep::m::*;
fn main() {{
    h();
}}
"
    );
    assert_eq!(messages(&used, &[], "unused_imports"), Vec::<String>::new());
    let unused = format!(
        "{DEP}//- /main.rs crate:other:main edition:2021 deps:dep
use dep::m::*;
fn main() {{}}
"
    );
    assert_eq!(messages(&unused, &[], "unused_imports"), ["unused_imports: unused import: `dep::m::*`"]);
}

#[test]
fn unused_imports_of_one_use_item_are_one_diagnostic() {
    let fixture = format!(
        "{DEP}//- /main.rs crate:other:main edition:2021 deps:dep
use dep::{{f, g, m::h}};
fn main() {{
    g();
}}
"
    );
    assert_eq!(
        messages(&fixture, &[], "unused_imports"),
        ["unused_imports: unused imports: `f` and `m::h`"]
    );
}

#[test]
fn qualification_of_a_dependency_s_item_found_unqualified() {
    let fixture = format!(
        "{DEP}//- /main.rs crate:other:main edition:2021 deps:dep
use dep::g;
fn main() {{
    g();
    dep::g();
    dep::f();
}}
"
    );
    assert_eq!(
        messages(&fixture, &["-W", "unused_qualifications"], "unused_qualifications"),
        ["unused_qualifications: unnecessary qualification"]
    );
}

#[test]
fn qualification_whose_name_alone_is_a_local_is_not_linted() {
    let fixture = "//- /main.rs crate:other:main edition:2021
mod a {
    pub fn f() {}
}
use a::f;
fn main() {
    f();
    let f = 1;
    a::f();
    let _ = f;
}
";
    assert_eq!(messages(fixture, &["-W", "unused_qualifications"], "unused_qualifications"), Vec::<String>::new());
}

#[test]
fn qualification_after_generic_args_is_not_linted() {
    let fixture = "//- /main.rs crate:other:main edition:2021
mod a {
    pub struct S<T>(pub T);
    impl<T> S<T> {
        pub fn new(t: T) -> S<T> { S(t) }
    }
}
use a::S;
fn main() {
    let _ = S::<u8>::new(0);
    let _: S<u8> = S(0);
}
";
    assert_eq!(messages(fixture, &["-W", "unused_qualifications"], "unused_qualifications"), Vec::<String>::new());
}

#[test]
fn qualification_through_an_unused_import_is_not_linted() {
    let fixture = "//- /main.rs crate:other:main edition:2021
mod a {
    pub fn f() {}
}
use a::f;
fn main() {
    a::f();
}
";
    assert_eq!(messages(fixture, &["-W", "unused_qualifications"], "unused_qualifications"), Vec::<String>::new());
    assert_eq!(messages(fixture, &[], "unused_imports"), ["unused_imports: unused import: `a::f`"]);
}

#[test]
fn trait_import_used_by_a_method_call_is_used() {
    let fixture = "//- /main.rs crate:other:main edition:2021
mod a {
    pub trait Tr {
        fn m(&self) {}
    }
    impl Tr for u8 {}
    pub trait Unused {
        fn n(&self) {}
    }
}
use a::Tr;
use a::Unused;
fn main() {
    0u8.m();
}
";
    assert_eq!(messages(fixture, &[], "unused_imports"), ["unused_imports: unused import: `a::Unused`"]);
}

/// A crate built for the host and for the target is two crates with one root file: each sees
/// its own uses, so neither reports the other's items and imports unused.
#[test]
fn the_second_crate_of_a_shared_root_file_sees_its_own_uses() {
    use blaze_query::db::Durability;
    let fixture = "//- /lib.rs crate:twice
mod a {
    pub struct S;
    fn helper() {}
    pub fn f() {
        helper()
    }
}
use a::S;
pub fn g(_: S) {
    a::f()
}
";
    let mut fx = hir_expand::fixture::db(fixture);
    let file = fx.file(0).file_id(fx.db());
    let db = fx.db_mut();
    let all = base_db::all_crates(db);
    let first =
        *all.iter().find(|it| it.data(db).root_file_id == file).expect("the fixture's crate");
    let mut cfg = first.cfg_options(db).clone();
    cfg.insert_atom(intern::Symbol::intern("twin"));
    let (data, extra) = (first.data(db).clone(), first.extra_data(db).clone());
    let (workspace, env) = (first.workspace_data(db).clone(), first.env(db).clone());
    let second = base_db::Crate::create(db, data, extra, workspace, cfg, env, Durability::LOW);
    let both = all.iter().copied().chain([second]);
    base_db::set_all_crates_with_durability(db, both, Durability::LOW);
    let db = fx.into_db();
    let names = std::collections::HashMap::from([(file, "lib.rs".to_owned())]);
    let options = Options::default();
    for krate in [first, second] {
        let files = crate::Files::new(&names, "");
        let diags = crate::check::check_crate(&db, hir::Crate::from(krate), files, &options);
        let lints: Vec<_> = diags.iter().filter_map(|it| it.lint_name()).collect();
        assert!(lints.is_empty(), "{krate:?}: {lints:?}");
    }
}
