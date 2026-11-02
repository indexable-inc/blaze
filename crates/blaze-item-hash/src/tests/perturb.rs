//! Section 9: a body's identity changes with each input of it alone and with nothing else.
//!
//! Each test builds two databases from two fixtures and compares the identity of the body of
//! the one function named `f`.

use base_db::Db;
use blaze_alloc_count::assert_at_most;
use blaze_ids::ContentId;
use hir_def::{
    AssocItemId, DefWithBodyId, FunctionId, ModuleDefId,
    expr_store::Body,
    nameres::{DefMap, crate_def_map},
    signatures::FunctionSignature,
};
use hir_expand::fixture;

use crate::identity::{identity, set_build};

mod properties;

pub(super) fn db(fixture: &str) -> Db {
    let mut db = fixture::db(fixture).into_db();
    set_build(&mut db, ContentId([7; 32]));
    db
}

/// The body of the one function named `name` in any crate, in a module, an impl or a block.
#[track_caller]
pub(super) fn body(db: &Db, name: &str) -> DefWithBodyId {
    let mut found = Vec::new();
    for &krate in base_db::all_crates(db).iter() {
        functions(db, crate_def_map(db, krate), &mut found);
    }
    found.retain(|&it| FunctionSignature::of(db, it).name.as_str() == name);
    match found[..] {
        [one] => one.into(),
        _ => panic!("{} functions named `{name}`: {found:?}", found.len()),
    }
}

fn functions(db: &Db, def_map: &DefMap, out: &mut Vec<FunctionId>) {
    for (_, module) in def_map.modules(db) {
        let scope = module.scope(db);
        for def in scope.declarations() {
            if let ModuleDefId::FunctionId(it) = def {
                out.push(it);
                for (_, block) in Body::of(db, it.into()).blocks(db) {
                    functions(db, block, out);
                }
            }
        }
        for it in scope.impls() {
            for &(_, item) in it.impl_items(db).items.iter() {
                if let AssocItemId::FunctionId(it) = item {
                    out.push(it);
                }
            }
        }
    }
}

#[track_caller]
fn identity_in(fixture: &str) -> ContentId {
    let db = db(fixture);
    let def = body(&db, "f");
    identity(&db, def).unwrap_or_else(|it| panic!("{it:?}"))
}

#[track_caller]
fn differ(before: &str, after: &str) {
    assert_ne!(identity_in(before), identity_in(after), "{before}\n---\n{after}");
}

#[track_caller]
fn same(before: &str, after: &str) {
    assert_eq!(identity_in(before), identity_in(after), "{before}\n---\n{after}");
}

const DEP: &str = r#"
//- /dep.rs crate:dep
pub struct S(pub u8);
pub enum E { X, Y }
pub trait Tr { fn m(&self) -> u8 { 0 } }
impl Tr for u8 {}
pub fn g() -> u8 { 0 }
#[macro_export]
macro_rules! mac { () => { 1u8 } }
"#;

pub(super) fn with_dep(main: &str) -> String {
    format!("//- /main.rs crate:main deps:dep\n{main}\n{DEP}")
}

#[test]
fn a_change_to_each_crate_input_changes_it() {
    let body = "pub fn f() -> u8 { dep::g() }";
    let base = with_dep(body);
    differ(
        &base,
        &format!("//- /main.rs crate:main deps:dep edition:2018\n{body}\n{DEP}"),
    );
    differ(&base, &format!("//- /main.rs crate:main deps:dep cfg:x\n{body}\n{DEP}"));
    differ(&base, &with_dep(&format!("#![feature(never_type)]\n{body}")));
    differ(&base, &with_dep(&format!("#![recursion_limit = \"64\"]\n{body}")));
    differ(&base, &with_dep(&format!("#![no_std]\n{body}")));
}

#[test]
fn the_front_end_build_changes_it() {
    let fixture = with_dep("pub fn f() -> u8 { dep::g() }");
    let mut db = db(&fixture);
    let def = body(&db, "f");
    let before = identity(&db, def).unwrap();
    set_build(&mut db, ContentId([8; 32]));
    assert_ne!(identity(&db, def).unwrap(), before);
}

#[test]
fn the_owners_signature_changes_it() {
    differ(&with_dep("pub fn f() -> u8 { 0 }"), &with_dep("pub fn f() -> u16 { 0 }"));
    differ(&with_dep("pub fn f(_: u8) {}"), &with_dep("pub fn f(_: u16) {}"));
}

#[test]
fn what_a_named_item_leaks_does_not_change_it() {
    // Inference reads a hidden type or a constant's value as a fact of its own, not through the
    // identity: the identity of a body naming them is computed inside its inference and reads
    // no body.
    same(
        &with_dep("pub fn g() -> impl Copy { 1u8 }\npub fn f() { let _ = g(); }"),
        &with_dep("pub fn g() -> impl Copy { 1u16 }\npub fn f() { let _ = g(); }"),
    );
    same(
        &with_dep("pub const C: u8 = 1;\npub fn f() -> u8 { C }"),
        &with_dep("pub const C: u8 = 2;\npub fn f() -> u8 { C }"),
    );
}

#[test]
fn an_async_fn_and_a_const_fn_have_identities() {
    identity_in(&with_dep("pub async fn f() -> u8 { dep::g() }"));
    identity_in(&with_dep("pub const fn f() -> u8 { 1 }"));
    identity_in(&with_dep("pub fn f() -> impl Copy { dep::g() }"));
}

#[test]
fn the_parent_impls_generics_change_it() {
    differ(
        &with_dep("pub struct W<T>(T);\nimpl<T> W<T> { pub fn f() {} }"),
        &with_dep("pub struct W<T>(T);\nimpl<T: dep::Tr> W<T> { pub fn f() {} }"),
    );
}

#[test]
fn one_token_changes_it() {
    differ(&with_dep("pub fn f() -> u8 { 1 }"), &with_dep("pub fn f() -> u8 { 2 }"));
    differ(&with_dep("pub fn f() -> u8 { 1 + 1 }"), &with_dep("pub fn f() -> u8 { 1 - 1 }"));
}

#[test]
fn a_change_to_each_kind_of_entry_changes_it() {
    let dep = |s: &str, e: &str, mac: &str| {
        r#"
//- /main.rs crate:main deps:dep
pub fn f() { let _ = dep::S(0); let _ = dep::E::X; let _ = dep::mac!(); { struct L; } }
//- /dep.rs crate:dep
STRUCT
ENUM
#[macro_export]
macro_rules! mac { () => { EXPANSION } }
"#
        .replace("STRUCT", s)
        .replace("ENUM", e)
        .replace("EXPANSION", mac)
    };
    let base = dep("pub struct S(pub u8);", "pub enum E { X, Y }", "1u8");
    // An item: the struct's signature.
    differ(&base, &dep("pub struct S(pub u16);", "pub enum E { X, Y }", "1u8"));
    // A variant: its enum's signature.
    differ(&base, &dep("pub struct S(pub u8);", "pub enum E { X, Y, Z }", "1u8"));
    // A macro call: its definition.
    differ(&base, &dep("pub struct S(pub u8);", "pub enum E { X, Y }", "2u8"));
    // A crate: the dependency's key.
    let renamed = (base.replace("deps:dep", "deps:dep2").replace("dep::", "dep2::"))
        .replace("crate:dep\n", "crate:dep2\n");
    differ(&base, &renamed);
}

#[test]
fn a_path_resolving_otherwise_in_each_namespace_changes_it() {
    let dep = |ty: &str, value: &str| {
        r#"
//- /main.rs crate:main deps:dep
pub fn f() { let _: dep::T = dep::v(); }
//- /dep.rs crate:dep
pub mod a { pub struct T; pub fn v() -> super::T { loop {} } }
pub mod b { pub struct T; pub fn v() -> super::T { loop {} } }
pub use TY::T;
pub use VALUE::v;
"#
        .replace("TY", ty)
        .replace("VALUE", value)
    };
    // The type namespace.
    differ(&dep("a", "a"), &dep("b", "a"));
    // The value namespace.
    differ(&dep("a", "a"), &dep("a", "b"));
}

#[test]
fn a_glob_import_shadowed_changes_it() {
    let fixture = |shadow: &str| {
        let main = r#"
//- /main.rs crate:main deps:dep,other
use dep::*;
SHADOW
pub fn f() -> u8 { g() }
//- /other.rs crate:other
pub fn g() -> u8 { 1 }
"#;
        format!("{}{DEP}", main.replace("SHADOW", shadow))
    };
    differ(&fixture(""), &fixture("use other::g;"));
}

#[test]
fn a_trait_added_in_a_block_changes_it() {
    let fixture = |import: &str| {
        with_dep(&"pub fn outer() { IMPORT fn f() -> u8 { 1u8.m() } }".replace("IMPORT", import))
    };
    differ(&fixture(""), &fixture("use dep::Tr;"));
}

#[test]
fn the_body_moved_to_another_module_changes_it() {
    differ(
        &with_dep("pub mod a { pub fn f() -> u8 { dep::g() } }\npub mod b {}"),
        &with_dep("pub mod a {}\npub mod b { pub fn f() -> u8 { dep::g() } }"),
    );
}

#[test]
fn moving_the_item_in_its_file_keeps_it() {
    same(
        &with_dep("pub fn f() -> u8 { dep::g() }\npub fn h() {}"),
        &with_dep("pub fn h() {}\npub fn f() -> u8 { dep::g() }"),
    );
}

#[test]
fn moving_a_location_macro_keeps_it() {
    let located = |body: &str| {
        with_dep(&format!(
            "#[rustc_builtin_macro]\nmacro_rules! line {{ () => {{}} }}\n\
             #[rustc_builtin_macro]\nmacro_rules! column {{ () => {{}} }}\n{body}"
        ))
    };
    // What `line!()` and `column!()` expand to is a slot the MIR record fills.
    same(
        &located("pub fn f() -> u32 { line!() + column!() }"),
        &located("\n\n\npub fn f() -> u32 {\n    line!() + column!()\n}"),
    );
    // Not where inference makes a type of its value.
    differ(
        &located("pub fn f() -> usize { [0u8; line!() as usize].len() }"),
        &located("\npub fn f() -> usize { [0u8; line!() as usize].len() }"),
    );
    differ(
        &located("pub fn f() -> u32 { const { line!() } }"),
        &located("\npub fn f() -> u32 { const { line!() } }"),
    );
}

#[test]
fn reformatting_keeps_it() {
    same(
        &with_dep("pub fn f() -> u8 { let x = dep::g(); x + 1 }"),
        &with_dep("pub fn f()\n    -> u8\n{\n    let   x =\n        dep::g();\n    x+1\n}"),
    );
}

#[test]
fn comments_keep_it() {
    same(
        &with_dep("pub fn f() -> u8 { dep::g() }"),
        &with_dep("/// f.\npub fn f() -> u8 {\n    // the dependency's\n    dep::g() /* ! */\n}"),
    );
}

#[test]
fn editing_another_body_keeps_it() {
    same(
        &with_dep("pub fn f() -> u8 { dep::g() }\npub fn h() -> u8 { 1 }"),
        &with_dep("pub fn f() -> u8 { dep::g() }\npub fn h() -> u8 { 2 + dep::g() }"),
    );
}

#[test]
fn an_unrelated_item_keeps_it() {
    same(
        &with_dep("pub fn f() -> u8 { dep::g() }"),
        &with_dep("pub fn f() -> u8 { dep::g() }\npub struct Unrelated;"),
    );
}

#[test]
fn the_same_body_in_two_databases_is_the_same() {
    let fixture = with_dep(
        "pub fn f() -> u8 { let c = |x: u8| x + dep::mac!(); { struct L; } c(dep::E::X as u8) }",
    );
    same(&fixture, &fixture);
}

#[test]
fn a_memoized_identity_allocates_nothing() {
    let db = db(&with_dep("pub fn f() -> u8 { dep::g() }"));
    let def = body(&db, "f");
    let first = identity(&db, def).unwrap();
    let again = assert_at_most(0, "a memoized identity", || identity(&db, def));
    assert_eq!(again, Ok(first));
}
