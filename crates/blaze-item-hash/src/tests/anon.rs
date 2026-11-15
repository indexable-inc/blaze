//! A record of a body names what it names relative to the body (`crate::keys`), so a record
//! filed under a body's key is read back in another database where the body moved, and the
//! body's presentation hash (`crate::presentation`) changes with its names and place while its
//! key, made from its position-free text, does not.
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
use crate::{identity::early_identity, keys, presentation::presentation};

/// The key and presentation hash of the body of the function `name`.
#[track_caller]
fn hashes(fixture: &str, name: &str) -> (ContentId, ContentId) {
    let db = db(fixture);
    let def = body(&db, name);
    (early_identity(&db, def).unwrap_or_else(|it| panic!("{it:?}")), presentation(&db, def))
}

#[test]
fn moving_the_item_keeps_its_key_and_changes_its_presentation() {
    let (key_before, presentation_before) =
        hashes(&with_dep("pub fn f() -> u8 { dep::g() }\npub fn h() {}"), "f");
    let (key_after, presentation_after) =
        hashes(&with_dep("pub fn h() {}\n\n\npub fn f() -> u8 { dep::g() }"), "f");
    assert_eq!(key_before, key_after, "the key is position-free");
    assert_ne!(presentation_before, presentation_after, "the presentation holds the place");
}

/// The control: a rename is an edit of the item's text, which its key hashes. Two bodies of
/// one text in two places share a key; two texts never do.
#[test]
fn renaming_a_local_changes_the_key() {
    let (before, _) = hashes(&with_dep("pub fn f() -> u8 { let x = dep::g(); x }"), "f");
    let (after, _) = hashes(&with_dep("pub fn f() -> u8 { let y = dep::g(); y }"), "f");
    assert_ne!(before, after);
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
fn a_record_of_a_moved_body_names_the_same_definitions() {
    let text = "pub fn f(a: u8) -> u8 { let c = |x: u8| x + a; c(dep::g()) }";
    let before = db(&with_dep(text));
    let after = db(&with_dep(&format!("pub struct Above;\n\n{text}")));
    let (old, new) = (body(&before, "f"), body(&after, "f"));
    assert_eq!(early_identity(&before, old), early_identity(&after, new));

    // The body's own item.
    let own = keys::def_key(&before, old, SolverDefId::FunctionId(body_fn(old)));
    assert_eq!(keys::def_of(&after, new, own), Some(SolverDefId::FunctionId(body_fn(new))));
    assert_ne!(own, key::def_key(&before, SolverDefId::FunctionId(body_fn(old))).0);

    // A closure of the body.
    let written = keys::def_key(&before, old, closure(&before, old));
    assert_eq!(keys::def_of(&after, new, written), Some(closure(&after, new)));

    // An item a path of the body names, by its plain item key: reading it back resolves no
    // path of the body (restore.md, "One form, a function: the end state").
    let callee = |db: &Db| SolverDefId::FunctionId(body_fn(body(db, "g")));
    let named = keys::def_key(&before, old, callee(&before));
    assert_eq!(named, key::def_key(&before, callee(&before)).0);
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
