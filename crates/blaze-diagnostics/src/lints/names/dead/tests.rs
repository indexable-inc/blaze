//! Allocation budgets of the dead-code pass's hot paths: once the crate is numbered and a
//! definition's level and attributes have been asked for, every answer the walk asks again is
//! a read, and allocates nothing.

use blaze_alloc_count::assert_at_most;

use super::{
    defs::{DefSet, Defs},
    level::Levels,
    reach::Reach,
    seed::Crate,
};
use crate::{cx::Cx, lints::names::effective::Effective};

const FIXTURE: &str = r#"
//- /main.rs crate:main
trait T {
    fn f(&self);
    type A;
}
enum E { A(i32), B }
struct S(i32);
impl T for S {
    fn f(&self) {}
    type A = E;
}
impl S {
    const C: i32 = 1;
}
#[allow(dead_code)]
fn unused() {}
fn main() {
    S(1).f();
}
"#;

/// `check` given the numbered crate of the fixture's first workspace crate.
fn with_crate(check: impl FnOnce(&Crate<'_, '_, '_>)) {
    let checked = crate::tests::checked(FIXTURE);
    let db = &checked.db;
    hir::attach_db(db, || {
        let krate = hir::check::workspace_crates(db)[0];
        let frame = crate::tests::frame();
        let cx = Cx::new(db, krate, checked.files(), &frame);
        let defs = Defs::of(&cx);
        let levels = Levels::new();
        let effective = Effective::of(&cx);
        let reach = Reach::of(&cx, &defs, &effective);
        let k = Crate::new(&cx, &defs, &levels, &reach);
        check(&k);
    });
}

/// A definition's level and `allow_or_lang`, asked a second time, are a `Cell` read.
#[test]
fn cached_level_and_attributes() {
    with_crate(|k| {
        for def in k.defs.all() {
            k.level(def);
            k.allow_or_lang(def);
        }
        assert_at_most(0, "a cached level or attribute answer", || {
            for def in k.defs.all() {
                k.level(def);
                k.allow_or_lang(def);
            }
        });
    });
}

/// Looking a definition up by its id, its children, its impl and its trait's item reads the
/// numbering.
#[test]
fn numbering_lookups() {
    with_crate(|k| {
        let defs = k.defs;
        assert_ne!(defs.len(), 0, "the fixture has definitions");
        assert_at_most(0, "a lookup in the crate's numbering", || {
            for def in defs.all() {
                let _ = defs.get(defs[def].id);
                let _ = defs.children(def);
                let _ = defs.impl_of(def);
                let _ = defs.trait_item_of(def);
                let _ = k.reach.is_reachable(def);
            }
        });
    });
}

/// A bitset over the definitions tests and inserts in place.
#[test]
fn bitset_test_and_insert() {
    let mut bits = DefSet::new(1000);
    assert_at_most(0, "a bitset's test and insert", || {
        for at in 0..1000 {
            assert!(!bits.contains(at));
            assert!(bits.insert(at));
            assert!(!bits.insert(at));
        }
    });
}
