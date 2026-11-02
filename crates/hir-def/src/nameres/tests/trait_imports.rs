//! `use Trait::item` and `use Trait::*` bind the trait's associated items, as rustc's
//! `rustc_resolve` reads a trait as a module to `use`; without `import_trait_associated_functions`
//! they still bind, and each import is reported (E0658) as in rustc's `finalize_imports`.

use super::*;
use crate::nameres::diagnostics::DefDiagnosticKind;

/// The test crate's def map and what it reported: `unresolved` for an unresolved import, the
/// reason for an import of trait items, `other` for the rest.
fn check_trait_imports(
    #[rust_analyzer::rust_fixture] ra_fixture: &str,
    expect: Expect,
    diagnostics: &[&str],
) {
    compute_crate_def_map(ra_fixture, |db, def_map| {
        expect.assert_eq(&def_map.dump(db));
        let reported: Vec<String> = (def_map.diagnostics(db).iter())
            .map(|it| match &it.kind {
                DefDiagnosticKind::UnresolvedImport { .. } => "unresolved".to_owned(),
                DefDiagnosticKind::TraitItemImport { why, .. } => format!("{why:?}"),
                _ => "other".to_owned(),
            })
            .collect();
        assert_eq!(reported, diagnostics);
    });
}

#[test]
fn glob_of_a_trait_with_the_feature() {
    check_trait_imports(
        r#"
#![feature(import_trait_associated_functions)]
trait Tr {
    fn f();
    const C: u8;
    type T;
}
use self::Tr::*;
"#,
        expect![[r#"
            crate
            - C : value (glob)
            - T : type (glob)
            - Tr : type
            - f : value (glob)
        "#]],
        &[],
    );
}

#[test]
fn glob_of_a_trait_without_the_feature() {
    check_trait_imports(
        r#"
trait Tr {
    fn f();
    const C: u8;
    type T;
}
use self::Tr::*;
"#,
        expect![[r#"
            crate
            - C : value (glob)
            - T : type (glob)
            - Tr : type
            - f : value (glob)
        "#]],
        &["Unstable"],
    );
}

#[test]
fn single_imports_from_a_trait_with_the_feature() {
    check_trait_imports(
        r#"
#![feature(import_trait_associated_functions)]
trait Tr {
    fn f();
    const C: u8;
    type T;
}
use self::Tr::{f, C, T};
"#,
        expect![[r#"
            crate
            - C : value (import)
            - T : type (import)
            - Tr : type
            - f : value (import)
        "#]],
        &[],
    );
}

#[test]
fn single_imports_from_a_trait_without_the_feature() {
    check_trait_imports(
        r#"
trait Tr {
    fn f();
    const C: u8;
    type T;
}
use self::Tr::{f, C, T};
"#,
        expect![[r#"
            crate
            - C : value (import)
            - T : type (import)
            - Tr : type
            - f : value (import)
        "#]],
        &["Unstable", "Unstable", "Unstable"],
    );
}

#[test]
fn a_name_in_both_namespaces_is_reported_in_each() {
    check_trait_imports(
        r#"
trait Tr {
    fn X();
    type X;
}
use self::Tr::X;
"#,
        expect![[r#"
            crate
            - Tr : type
            - X : type (import) value (import)
        "#]],
        &["Unstable", "Unstable"],
    );
}

#[test]
fn imports_from_a_trait_of_a_dependency() {
    check_trait_imports(
        r#"
//- /main.rs crate:main deps:dep
use dep::Tr::*;
use dep::Tr::g;
//- /lib.rs crate:dep
macro_rules! m { () => { fn g(); } }
pub trait Tr {
    fn f();
    type T;
    m!();
}
"#,
        expect![[r#"
            crate
            - T : type (glob)
            - f : value (glob)
            - g : value (import)
        "#]],
        &["Unstable", "Unstable"],
    );
}

#[test]
fn a_trait_item_from_a_macro_in_the_crate_being_collected_is_reported() {
    check_trait_imports(
        r#"
#![feature(import_trait_associated_functions)]
macro_rules! m { () => { fn g(); } }
trait Tr {
    fn f();
    m!();
}
use self::Tr::*;
use self::Tr::f as h;
"#,
        expect![[r#"
            crate
            - Tr : type
            - h : _
            - (legacy) m : macro!
        "#]],
        &["unresolved", "ItemsFromMacros"],
    );
}

#[test]
fn a_configured_out_trait_item_is_not_imported() {
    check_trait_imports(
        r#"
#![feature(import_trait_associated_functions)]
trait Tr {
    #[cfg(any())]
    fn gone();
    fn f();
}
use self::Tr::*;
"#,
        expect![[r#"
            crate
            - Tr : type
            - f : value (glob)
        "#]],
        &[],
    );
}
