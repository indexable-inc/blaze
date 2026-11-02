//! `crate_shadows_primitive_type`: whether a primitive type's name can mean something else in a
//! crate.

use base_db::{Crate, all_crates};
use hir_expand::fixture;

use crate::nameres::crate_shadows_primitive_type;

fn shadows(#[rust_analyzer::rust_fixture] ra_fixture: &str) -> bool {
    let db = fixture::db(ra_fixture).into_db();
    let main: Crate = all_crates(&db)
        .iter()
        .copied()
        .find(|&krate| {
            krate.extra_data(&db).display_name.as_ref().map(|it| it.canonical_name().as_str())
                == Some("main")
        })
        .expect("a crate named `main`");
    crate_shadows_primitive_type(&db, main)
}

#[test]
fn a_crate_naming_no_primitive_shadows_none() {
    assert!(!shadows(
        r#"
//- /main.rs crate:main
struct S(u8, bool);
mod m { pub struct T(pub &'static str); }
use m::T;
"#,
    ));
}

#[test]
fn a_module_named_as_a_primitive_shadows_it() {
    assert!(shadows(
        r#"
//- /main.rs crate:main
mod a { mod u8 {} }
"#,
    ));
}

#[test]
fn a_named_import_shadows() {
    assert!(shadows(
        r#"
//- /main.rs crate:main
mod m { pub struct S; }
use m::S as bool;
"#,
    ));
}

#[test]
fn a_glob_import_shadows() {
    assert!(shadows(
        r#"
//- /main.rs crate:main
mod m { #[allow(non_camel_case_types)] pub struct str; }
mod n { use super::m::*; }
"#,
    ));
}

#[test]
fn an_extern_crate_shadows() {
    assert!(shadows(
        r#"
//- /main.rs crate:main deps:dep
extern crate dep as char;
//- /dep.rs crate:dep
pub struct S;
"#,
    ));
}

#[test]
fn a_value_of_a_primitive_name_does_not_shadow() {
    assert!(!shadows(
        r#"
//- /main.rs crate:main
#[allow(non_upper_case_globals)]
const u8: u32 = 0;
fn i32() {}
"#,
    ));
}

#[test]
fn a_block_scope_is_not_counted() {
    assert!(!shadows(
        r#"
//- /main.rs crate:main
fn f() {
    #[allow(non_camel_case_types)]
    struct u8;
}
"#,
    ));
}
