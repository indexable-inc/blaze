//! Sections 3.3, 3.5 and 3.7: a body's identity is anonymous (renames and moves keep it, and
//! its presentation hash changes with them), and a record of it names what it names relative
//! to the body, so a record of a renamed body is read back.
//!
//! Each test builds two databases from two fixtures and compares the body of the one function
//! named `f` in the first with the one named `name` in the second.

use base_db::Db;
use blaze_ids::ContentId;
use blaze_items::key;
use hir_def::{DefWithBodyId, expr_store::Body, hir::Expr};
use hir_ty::{
    InferBodyId,
    db::{InternedClosure, InternedClosureId},
    next_solver::SolverDefId,
};

use super::perturb::{body, db, with_dep};
use crate::{identity::identity, keys, presentation::presentation};

/// The identity and presentation hash of the body of the function `name`.
#[track_caller]
fn hashes(fixture: &str, name: &str) -> (ContentId, ContentId) {
    let db = db(fixture);
    let def = body(&db, name);
    (identity(&db, def).unwrap_or_else(|it| panic!("{it:?}")), presentation(&db, def))
}

/// `after`'s body of `name` is `before`'s of `f` renamed or moved: the same identity, another
/// presentation.
#[track_caller]
fn renamed(before: &str, after: &str, name: &str) {
    let (identity_before, presentation_before) = hashes(before, "f");
    let (identity_after, presentation_after) = hashes(after, name);
    assert_eq!(identity_before, identity_after, "identity\n{before}\n---\n{after}");
    assert_ne!(presentation_before, presentation_after, "presentation\n{before}\n---\n{after}");
}

#[track_caller]
fn differ(before: &str, after: &str) {
    assert_ne!(hashes(before, "f").0, hashes(after, "f").0, "{before}\n---\n{after}");
}

#[test]
fn renaming_a_parameter_keeps_the_identity() {
    renamed(
        &with_dep("pub fn f(a: u8) -> u8 { a + 1 }"),
        &with_dep("pub fn f(b: u8) -> u8 { b + 1 }"),
        "f",
    );
}

#[test]
fn renaming_a_local_keeps_the_identity() {
    renamed(
        &with_dep("pub fn f() -> u8 { let x = dep::g(); let y = x; y }"),
        &with_dep("pub fn f() -> u8 { let first = dep::g(); let second = first; second }"),
        "f",
    );
}

#[test]
fn renaming_a_capture_keeps_the_identity() {
    renamed(
        &with_dep("pub fn f() -> u8 { let x = 1; let c = || x; c() }"),
        &with_dep("pub fn f() -> u8 { let y = 1; let c = || y; c() }"),
        "f",
    );
}

#[test]
fn renaming_a_label_keeps_the_identity() {
    renamed(
        &with_dep("pub fn f() { 'a: loop { break 'a; } }"),
        &with_dep("pub fn f() { 'b: loop { break 'b; } }"),
        "f",
    );
}

#[test]
fn renaming_a_generic_or_a_lifetime_keeps_the_identity() {
    renamed(
        &with_dep("pub fn f<T: Copy>(t: T) -> T { let u: T = t; u }"),
        &with_dep("pub fn f<U: Copy>(t: U) -> U { let u: U = t; u }"),
        "f",
    );
    renamed(
        &with_dep("pub fn f<'a>(x: &'a u8) -> &'a u8 { let y: &'a u8 = x; y }"),
        &with_dep("pub fn f<'b>(x: &'b u8) -> &'b u8 { let y: &'b u8 = x; y }"),
        "f",
    );
}

#[test]
fn renaming_the_item_keeps_the_identity() {
    renamed(
        &with_dep("pub fn f() -> u8 { dep::g() }"),
        &with_dep("pub fn renamed() -> u8 { dep::g() }"),
        "renamed",
    );
    // A recursive call names the item as "self".
    renamed(
        &with_dep("pub fn f(n: u8) -> u8 { if n == 0 { 0 } else { f(n - 1) } }"),
        &with_dep("pub fn renamed(n: u8) -> u8 { if n == 0 { 0 } else { renamed(n - 1) } }"),
        "renamed",
    );
}

#[test]
fn an_import_alias_keeps_the_identity() {
    renamed(
        &with_dep("use dep::g;\npub fn f() -> u8 { g() }"),
        &with_dep("use dep::g as other_name;\npub fn f() -> u8 { other_name() }"),
        "f",
    );
}

#[test]
fn moving_the_item_in_its_file_keeps_the_identity() {
    renamed(
        &with_dep("pub fn f() -> u8 { dep::g() }\npub fn h() {}"),
        &with_dep("pub fn h() {}\n\n\npub fn f() -> u8 { dep::g() }"),
        "f",
    );
}

#[test]
fn renaming_a_callee_changes_the_identity() {
    // A record holds facts of each item a path names by its key (its constant's value, its
    // hidden type), so the callee's key is in the identity: two items with one signature are
    // not interchangeable.
    let fixture = |name: &str| {
        format!(
            "//- /main.rs crate:main deps:dep\npub fn f() -> u8 {{ dep::{name}() }}\n\
             //- /dep.rs crate:dep\npub fn {name}() -> u8 {{ 0 }}\n"
        )
    };
    differ(&fixture("g"), &fixture("renamed"));
}

#[test]
fn swapping_two_locals_uses_changes_the_identity() {
    differ(
        &with_dep("pub fn f(a: u8, b: u8) -> u8 { a - b }"),
        &with_dep("pub fn f(a: u8, b: u8) -> u8 { b - a }"),
    );
}

#[test]
fn another_field_or_method_changes_the_identity() {
    let field = |name: &str| {
        with_dep(&format!(
            "pub struct P {{ pub x: u8, pub y: u8 }}\npub fn f(p: P) -> u8 {{ p.{name} }}"
        ))
    };
    differ(&field("x"), &field("y"));
    let method = |name: &str| {
        with_dep(&format!(
            "pub trait M {{ fn m(&self) -> u8; fn n(&self) -> u8; }}\n\
             impl M for u8 {{ fn m(&self) -> u8 {{ 1 }} fn n(&self) -> u8 {{ 2 }} }}\n\
             pub fn f() -> u8 {{ 1u8.{name}() }}"
        ))
    };
    differ(&method("m"), &method("n"));
}

#[test]
fn moving_into_another_module_changes_the_identity() {
    differ(
        &with_dep("pub mod a { pub fn f() -> u8 { dep::g() } }\npub mod b {}"),
        &with_dep("pub mod a {}\npub mod b { pub fn f() -> u8 { dep::g() } }"),
    );
}

#[test]
fn editing_above_the_body_on_the_same_lines_keeps_the_presentation() {
    let (_, before) = hashes(&with_dep("pub fn h() -> u8 { 1 }\npub fn f() -> u8 { 2 }"), "f");
    let (_, after) = hashes(&with_dep("pub fn h() -> u8 { 100 }\npub fn f() -> u8 { 2 }"), "f");
    assert_eq!(before, after);
}

/// The first closure written in `def`'s body.
fn closure(db: &Db, def: DefWithBodyId) -> SolverDefId<'_> {
    let body = Body::of(db, def);
    let (expr, kind) = body
        .exprs()
        .find_map(|(id, expr)| match expr {
            Expr::Closure { closure_kind, .. } => Some((id, closure_kind)),
            _ => None,
        })
        .expect("a closure in the body");
    let loc = InternedClosure { owner: InferBodyId::DefWithBodyId(def), expr, kind };
    SolverDefId::InternedClosureId(InternedClosureId::new(db, loc))
}

#[test]
fn a_record_of_a_renamed_body_names_the_same_definitions() {
    let before = db(&with_dep("pub fn f(a: u8) -> u8 { let c = |x: u8| x + a; c(dep::g()) }"));
    let after = db(&with_dep(
        "\n\npub fn renamed(b: u8) -> u8 { let k = |y: u8| y + b; k(dep::g()) }",
    ));
    let (old, new) = (body(&before, "f"), body(&after, "renamed"));
    assert_eq!(identity(&before, old), identity(&after, new));

    // The body's own item.
    let own = keys::def_key(&before, old, SolverDefId::FunctionId(body_fn(old)));
    assert_eq!(keys::def_of(&after, new, own), Some(SolverDefId::FunctionId(body_fn(new))));
    assert_ne!(own, key::def_key(&before, SolverDefId::FunctionId(body_fn(old))).0);

    // A closure of the body, by its place in it.
    let written = keys::def_key(&before, old, closure(&before, old));
    assert_eq!(keys::def_of(&after, new, written), Some(closure(&after, new)));

    // An item a path of the body names, by the path.
    let callee = |db: &Db| SolverDefId::FunctionId(body_fn(body(db, "g")));
    let named = keys::def_key(&before, old, callee(&before));
    let read = keys::def_of(&after, new, named).expect("the callee read back");
    assert_eq!(key::def_key(&after, read).0, key::def_key(&before, callee(&before)).0);
    assert_eq!(read, callee(&after));
}

fn body_fn(def: DefWithBodyId) -> hir_def::FunctionId {
    match def {
        DefWithBodyId::FunctionId(it) => it,
        _ => unreachable!("the bodies here are functions'"),
    }
}
