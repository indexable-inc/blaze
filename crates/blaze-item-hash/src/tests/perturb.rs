//! A body's key, made before lowering (docs/v2/plan/design/end-state/engine.md 4.2 and 4.5):
//! it changes with each input of it alone (its crate's settings, the front end's build, its
//! own text, its owner, its module) and with nothing else. What a body resolves (a path's
//! target, a glob shadowed, a trait in scope, a named item's signature) is not in the key: it
//! is a fact of the step's shape, and `classify` tests that each such read is one.
//!
//! Each test builds two databases from two fixtures and compares the key of the body of the
//! one function named `f`.

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

use crate::identity::{early_identity, set_build};

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
fn key_in(fixture: &str) -> ContentId {
    let db = db(fixture);
    let def = body(&db, "f");
    early_identity(&db, def).unwrap_or_else(|it| panic!("{it:?}"))
}

#[track_caller]
fn differ(before: &str, after: &str) {
    assert_ne!(key_in(before), key_in(after), "{before}\n---\n{after}");
}

#[track_caller]
fn same(before: &str, after: &str) {
    assert_eq!(key_in(before), key_in(after), "{before}\n---\n{after}");
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
fn a_change_to_each_crate_setting_changes_it() {
    let body = "pub fn f() -> u8 { dep::g() }";
    let base = with_dep(body);
    differ(&base, &format!("//- /main.rs crate:main deps:dep edition:2018\n{body}\n{DEP}"));
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
    let before = early_identity(&db, def).unwrap();
    set_build(&mut db, ContentId([8; 32]));
    assert_ne!(early_identity(&db, def).unwrap(), before);
}

#[test]
fn the_items_own_signature_changes_it() {
    differ(&with_dep("pub fn f() -> u8 { 0 }"), &with_dep("pub fn f() -> u16 { 0 }"));
    differ(&with_dep("pub fn f(_: u8) {}"), &with_dep("pub fn f(_: u16) {}"));
}

/// What the body resolves is a fact of its steps, not a part of its key: a named item's
/// hidden type or value, a dependency's item's signature, which item a path or a glob finds.
/// `classify` holds each such read to a fact, so the step's answer is checked against it.
#[test]
fn what_the_body_resolves_does_not_change_it() {
    same(
        &with_dep("pub fn g() -> impl Copy { 1u8 }\npub fn f() { let _ = g(); }"),
        &with_dep("pub fn g() -> impl Copy { 1u16 }\npub fn f() { let _ = g(); }"),
    );
    same(
        &with_dep("pub const C: u8 = 1;\npub fn f() -> u8 { C }"),
        &with_dep("pub const C: u8 = 2;\npub fn f() -> u8 { C }"),
    );
    let dep = |s: &str| {
        "//- /main.rs crate:main deps:dep\npub fn f() { let _ = dep::S(0); }\n//- /dep.rs crate:dep\nSTRUCT\n"
            .replace("STRUCT", s)
    };
    same(&dep("pub struct S(pub u8);"), &dep("pub struct S(pub u16);"));
    let glob = |shadow: &str| {
        let main = "//- /main.rs crate:main deps:dep,other\nuse dep::*;\nSHADOW\npub fn f() -> u8 { g() }\n\
                    //- /other.rs crate:other\npub fn g() -> u8 { 1 }\n";
        format!("{}{DEP}", main.replace("SHADOW", shadow))
    };
    same(&glob(""), &glob("use other::g;"));
}

#[test]
fn an_async_fn_and_a_const_fn_have_keys() {
    key_in(&with_dep("pub async fn f() -> u8 { dep::g() }"));
    key_in(&with_dep("pub const fn f() -> u8 { 1 }"));
    key_in(&with_dep("pub fn f() -> impl Copy { dep::g() }"));
}

#[test]
fn another_impl_as_owner_changes_it() {
    differ(
        &with_dep("pub struct W; pub struct V;\nimpl W { pub fn f() {} }"),
        &with_dep("pub struct W; pub struct V;\nimpl V { pub fn f() {} }"),
    );
}

#[test]
fn one_token_changes_it() {
    differ(&with_dep("pub fn f() -> u8 { 1 }"), &with_dep("pub fn f() -> u8 { 2 }"));
    differ(&with_dep("pub fn f() -> u8 { 1 + 1 }"), &with_dep("pub fn f() -> u8 { 1 - 1 }"));
    differ(
        &with_dep("pub fn f(a: u8, b: u8) -> u8 { a - b }"),
        &with_dep("pub fn f(a: u8, b: u8) -> u8 { b - a }"),
    );
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
        &with_dep("pub fn h() {}\n\n\npub fn f() -> u8 { dep::g() }"),
    );
}

/// A position is a read of its own (engine.md 4.2, the `Fn` record: "positions as separate
/// reads"), never a part of the key: moving a body that calls `line!()` keeps it.
#[test]
fn moving_a_location_macro_keeps_it() {
    let located = |body: &str| {
        with_dep(&format!(
            "#[rustc_builtin_macro]\nmacro_rules! line {{ () => {{}} }}\n\
             #[rustc_builtin_macro]\nmacro_rules! column {{ () => {{}} }}\n{body}"
        ))
    };
    same(
        &located("pub fn f() -> u32 { line!() + column!() }"),
        &located("\n\n\npub fn f() -> u32 { line!() + column!() }"),
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
        &with_dep("// f.\npub fn f() -> u8 {\n    // the dependency's\n    dep::g() /* ! */\n}"),
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
fn a_memoized_key_allocates_nothing() {
    let db = db(&with_dep("pub fn f() -> u8 { dep::g() }"));
    let def = body(&db, "f");
    let first = early_identity(&db, def).unwrap();
    let again = assert_at_most(0, "a memoized key", || early_identity(&db, def));
    assert_eq!(again, Ok(first));
}
