//! The columns: a crate's hash columns against each item's hashes, and fact sets against the
//! facts they were made of.

use std::alloc::Global;

use blaze_ids::ContentId;
use blaze_mem::{Frame, Tag};
use hir_expand::fixture;

use super::{edit, items, ran, runs_of, single};
use crate::{
    Fact, FactSet, HashColumns, ItemKey, NameHash, SimplifiedType, crate_hashes, hashes, item_key,
    kinds, table::Keep,
};

fn id(n: u8) -> ContentId {
    ContentId([n; 32])
}

/// A copy of `columns` that outlives an edit of the database it came from, rebuilt from its
/// rows and kept in `frame`.
fn owned<'a>(columns: &HashColumns<'_>, frame: &'a Frame) -> HashColumns<'a> {
    let mut rows: Vec<_, Global> = Vec::new_in(Global);
    rows.extend((0..columns.len()).map(|row| (columns.keys()[row], columns.row(row))));
    let mut unhashable: Vec<_, Global> = Vec::new_in(Global);
    unhashable.extend_from_slice(columns.unhashable());
    HashColumns::new(Keep::Frame(frame), &mut rows, &mut unhashable)
}

/// A frame for a test's copy of a crate's columns.
fn test_frame() -> Frame {
    Frame::new(Tag::Other).expect("a test's frame")
}

/// A fact of each kind, two of fields.
pub(super) fn every_kind() -> Vec<Fact> {
    let (a, b) = (ItemKey(id(1)), ItemKey(id(2)));
    vec![
        Fact::Signature(a),
        Fact::Layout(b),
        Fact::Field { ty: a, field: 3 },
        Fact::Field { ty: a, field: 0 },
        Fact::Method { self_ty: SimplifiedType(id(3)), name: NameHash(id(4)) },
        Fact::Implements { trait_: b, self_ty: SimplifiedType(id(3)) },
        Fact::Resolves { scope: a, name: NameHash(id(4)) },
        Fact::Code(a),
        Fact::ConstValue(b),
        Fact::Name(a),
        Fact::Captures(b),
        Fact::AutoTraits(a),
        Fact::Coherence(b),
        Fact::Specializes(a),
        Fact::DropCheck(b),
        Fact::Vtable { trait_: a, self_ty: SimplifiedType(id(5)) },
        Fact::Derive(a),
        Fact::Target(id(6)),
        Fact::Crate(id(7)),
        Fact::HiddenTypes(b),
        Fact::RpititTys(a),
        Fact::InherentImpls(SimplifiedType(id(3))),
        Fact::TraitImpls(b),
        Fact::TraitsNamed(NameHash(id(4))),
        Fact::ScopeTraitsNamed { scope: a, name: NameHash(id(4)) },
        Fact::BlockImpls(b),
    ]
}

#[test]
fn a_fact_set_gives_back_its_facts_sorted_and_distinct() {
    let facts = every_kind();
    let mut twice = facts.clone();
    twice.extend(facts.iter().rev().copied());
    let set: FactSet = twice.into_iter().collect();
    assert_eq!(set.len(), facts.len());
    let back: Vec<Fact> = set.iter().collect();
    for fact in &facts {
        assert!(back.contains(fact), "{fact:?} is lost");
        assert!(set.contains(fact), "{fact:?} is not found");
    }
    let order: Vec<(u16, QueryOrder)> = back.iter().map(order).collect();
    assert!(order.windows(2).all(|it| it[0] < it[1]), "{back:?}");
}

type QueryOrder = (ContentId, Option<ContentId>, Option<u32>);

fn order(fact: &Fact) -> (u16, QueryOrder) {
    let parts = match *fact {
        Fact::Field { ty, field } => (ty.0, None, Some(field)),
        Fact::Method { self_ty, name } => (self_ty.0, Some(name.0), None),
        Fact::Implements { trait_, self_ty } | Fact::Vtable { trait_, self_ty } => {
            (trait_.0, Some(self_ty.0), None)
        }
        Fact::Resolves { scope, name } | Fact::ScopeTraitsNamed { scope, name } => {
            (scope.0, Some(name.0), None)
        }
        Fact::InherentImpls(it) => (it.0, None, None),
        Fact::TraitsNamed(it) => (it.0, None, None),
        Fact::Signature(it)
        | Fact::Layout(it)
        | Fact::Code(it)
        | Fact::ConstValue(it)
        | Fact::Name(it)
        | Fact::Captures(it)
        | Fact::AutoTraits(it)
        | Fact::Coherence(it)
        | Fact::Specializes(it)
        | Fact::DropCheck(it)
        | Fact::Derive(it)
        | Fact::HiddenTypes(it)
        | Fact::RpititTys(it)
        | Fact::TraitImpls(it)
        | Fact::BlockImpls(it) => (it.0, None, None),
        Fact::Target(it) | Fact::Crate(it) => (it, None, None),
    };
    (fact.kind().0, parts)
}

#[test]
fn a_range_scan_finds_the_facts_of_one_kind_about_one_subject() {
    let set: FactSet = every_kind().into_iter().collect();
    let fields: Vec<Fact> = set.about(kinds::FIELD, id(1)).collect();
    assert_eq!(
        fields,
        [
            Fact::Field { ty: ItemKey(id(1)), field: 0 },
            Fact::Field { ty: ItemKey(id(1)), field: 3 }
        ]
    );
    assert_eq!(set.about(kinds::FIELD, id(2)).count(), 0);
    assert_eq!(set.about(kinds::SIGNATURE, id(9)).count(), 0);
    assert!(!set.contains(&Fact::Field { ty: ItemKey(id(1)), field: 1 }));
    assert!(!set.contains(&Fact::Signature(ItemKey(id(2)))));
    assert!(FactSet::default().is_empty());
}

#[test]
fn crate_hash_columns_are_each_items_hashes() {
    let (mut db, file) = single(
        r#"
pub struct S { x: u32 }
pub enum E { A(S), B }
pub trait Tr { fn m(&self) -> u32 { 0 } }
impl Tr for S {}
pub fn f(s: &S) -> u32 { s.x }
pub const C: u32 = 1;
fn g() { fn inner() {} }
"#,
    );
    let krate = fixture::test_crate(&db);
    let frame = test_frame();
    let columns = owned(crate_hashes(&db, krate), &frame);
    assert_eq!(&columns, crate_hashes(&db, krate));
    assert!(columns.unhashable().is_empty(), "{:?}", columns.unhashable());
    for id in items(&db) {
        let key = item_key(&db, id);
        assert_eq!(columns.get(key), Some(hashes(&db, id).unwrap()), "{id:?}");
    }
    assert!(columns.len() > items(&db).len(), "the block's `inner` is in the columns");
    assert!(columns.keys().windows(2).all(|it| it[0] < it[1]));

    edit(
        &mut db,
        file,
        r#"
pub struct S { x: u64 }
pub enum E { A(S), B }
pub trait Tr { fn m(&self) -> u32 { 0 } }
impl Tr for S {}
pub fn f(s: &S) -> u64 { s.x }
pub const C: u32 = 1;
fn g() { fn inner() {} }
"#,
    );
    // The crate's id is of the snapshot it was taken in: taken again after the edit.
    let krate = fixture::test_crate(&db);
    let now = crate_hashes(&db, krate);
    let changed = now.changed_since(&columns);
    let named = |path: &str| item_key(&db, super::item(&db, path));
    for path in ["S", "E", "f"] {
        assert!(changed.contains(&named(path)), "`{path}` changed");
    }
    for path in ["C", "g"] {
        assert!(!changed.contains(&named(path)), "`{path}` did not change");
    }
    assert!(now.changed_since(now).is_empty());
}

// Restated for blaze-memo (port decision 9). It also claimed the edit reruns `crate_hashes`
// and not `crate_items_by_key`: false by design, since an edit of a file drops its crate's
// arena at the commit and every query of the crate runs again; reuse across an edit is the
// host's, on records (docs/v2/plan/design/host.md), and that claim is owed there.
#[test]
fn a_body_edit_changes_the_hash_of_its_item_alone() {
    let (mut db, file) = single(
        r#"
pub struct S { x: u32 }
pub fn f(s: &S) -> u32 { s.x }
fn g() { fn inner() {} }
"#,
    );
    let krate = fixture::test_crate(&db);
    let frame = test_frame();
    let before = owned(crate_hashes(&db, krate), &frame);
    edit(
        &mut db,
        file,
        r#"
pub struct S { x: u32 }
pub fn f(s: &S) -> u32 { s.x + 1 }
fn g() { fn inner() {} }
"#,
    );
    // The crate's id is of the snapshot it was taken in: taken again after the edit.
    let krate = fixture::test_crate(&db);
    let executed = runs_of(&db, || {
        let now = crate_hashes(&db, krate);
        assert_eq!(now.changed_since(&before), [item_key(&db, super::item(&db, "f"))]);
    });
    assert!(ran(&executed, crate::table::crate_hashes::KIND_NAME), "{executed:#?}");
}
