//! What this crate's facts are worth (docs/v2/plan/design/item-hash.md section 9): each fact
//! changes across a pair of programs where what it stands for changed and stays where it did
//! not, and hashing one of this crate's facts again allocates nothing.
//!
//! The tests of the read classifier (the rule table; sealed rows) went with it: on blaze-memo a
//! step's read list is what `blaze_memo::record` lists (docs/v2/plan/design/front-end-on-memo.md
//! rule 5). What the rows of real steps stand for is `super::steps`'.

use std::sync::Arc;

use base_db::{Crate, Db, all_crates};
use blaze_alloc_count::assert_at_most;
use blaze_format::Aligned;
use blaze_hash::HashMap;
use blaze_ids::ContentId;
use blaze_items::{Fact, ItemId, auto_traits, const_value, implements, key, method, rpitit_tys};
use hir_def::{
    AdtId, DefWithBodyId, HasModule, ImplId, ModuleDefId,
    deps::{ImageBuf, ImageStore},
    nameres::crate_def_map,
};
use hir_expand::fixture;
use hir_expand::name::Name;
use hir_ty::{
    body_store::{self, Mode, Step},
    next_solver::{
        SolverDefId,
        goal_store::{FactObject, FactRef},
    },
};
use blaze_core::lock::Lock;
use rustc_type_ir::fast_reject::SimplifiedType;
use std::alloc::Global;

use crate::values::{self, kinds};

fn db_of(fixture: &str) -> Db {
    let db = fixture::db(fixture).into_db();
    db
}

fn local_crate(db: &Db) -> Crate {
    *all_crates(db).iter().find(|krate| krate.data(db).origin.is_local()).expect("a local crate")
}

/// The local crate's item whose path ends in `name`.
fn named(db: &Db, name: &str) -> ItemId {
    let krate = local_crate(db);
    for (_, module) in crate_def_map(db, krate).modules(db) {
        for def in module.scope(db).declarations() {
            let item = match def {
                ModuleDefId::AdtId(it) => ItemId::Adt(it),
                ModuleDefId::TraitId(it) => ItemId::Trait(it),
                ModuleDefId::ConstId(it) => ItemId::Const(it),
                ModuleDefId::FunctionId(it) => ItemId::Function(it),
                ModuleDefId::StaticId(it) => ItemId::Static(it),
                ModuleDefId::TypeAliasId(it) => ItemId::Alias(it),
                _ => continue,
            };
            if key::path(db, item).rsplit("::").next() == Some(name) {
                return item;
            }
        }
    }
    panic!("no item `{name}` in the local crate")
}

/// The local crate's `n`th impl, in module order (each fixture here has one module).
fn nth_impl(db: &Db, n: usize) -> ImplId {
    let krate = local_crate(db);
    crate_def_map(db, krate)
        .modules(db)
        .flat_map(|(_, module)| module.scope(db).impls())
        .nth(n)
        .unwrap_or_else(|| panic!("no impl {n} in the local crate"))
}

/// The local crate's impl method `name`.
fn impl_method(db: &Db, name: &str) -> hir_def::FunctionId {
    crate_def_map(db, local_crate(db))
        .modules(db)
        .flat_map(|(_, module)| module.scope(db).impls())
        .find_map(|imp| {
            imp.impl_items(db).items.iter().find_map(|(item_name, item)| match *item {
                hir_def::AssocItemId::FunctionId(it) if item_name.as_str() == name => Some(it),
                _ => None,
            })
        })
        .unwrap_or_else(|| panic!("no impl method `{name}` in the local crate"))
}

/// A lane items fact as its `FactRef`, as `blaze_items::table` splits it.
fn items_fact(fact: Fact) -> FactRef {
    let (subject, object) = match fact {
        Fact::Method { self_ty, name } => (self_ty.0, FactObject::Id(name.0)),
        Fact::Implements { trait_, self_ty } => (trait_.0, FactObject::Id(self_ty.0)),
        Fact::Signature(it)
        | Fact::ConstValue(it)
        | Fact::AutoTraits(it)
        | Fact::RpititTys(it) => (it.0, FactObject::None),
        other => panic!("no test builds a {other:?}"),
    };
    FactRef { kind: fact.kind(), subject, object }
}

fn adt_type<'db>(adt: AdtId) -> SimplifiedType<SolverDefId<'db>> {
    SimplifiedType::Adt(SolverDefId::AdtId(adt))
}

/// A pair of programs and whether the fact `of` picks in each must differ between them.
struct Pair {
    what: &'static str,
    before: &'static str,
    after: &'static str,
    changes: bool,
    of: fn(&Db) -> FactRef,
}

fn fields_of(name: &'static str) -> impl Fn(&Db) -> FactRef {
    move |db| values::fields(db, named(db, name))
}

const PRELUDE: &str = "//- minicore: sized, copy, clone, add, send, sync\n";

fn check(pairs: &[Pair]) {
    for pair in pairs {
        let (before, after) = (
            db_of(&format!("{PRELUDE}{}", pair.before)),
            db_of(&format!("{PRELUDE}{}", pair.after)),
        );
        let fact = (pair.of)(&before);
        let was = values::value(&before, local_crate(&before), fact);
        let is = values::value(&after, local_crate(&after), (pair.of)(&after));
        assert!(was.is_some(), "{}: the fact has no value before", pair.what);
        assert!(is.is_some(), "{}: the fact has no value after", pair.what);
        assert_eq!(was != is, pair.changes, "{}: {fact:?}", pair.what);
    }
}

#[test]
fn fields_changes_with_a_member_name_and_only_then() {
    check(&[
        Pair {
            what: "a renamed field",
            before: "pub struct S { pub a: u32 }",
            after: "pub struct S { pub b: u32 }",
            changes: true,
            of: |db| values::fields(db, named(db, "S")),
        },
        Pair {
            what: "fields swapped",
            before: "pub struct S { pub a: u32, pub b: u32 }",
            after: "pub struct S { pub b: u32, pub a: u32 }",
            changes: true,
            of: |db| values::fields(db, named(db, "S")),
        },
        Pair {
            what: "a renamed variant",
            before: "pub enum E { A, B(u8) }",
            after: "pub enum E { A, C(u8) }",
            changes: true,
            of: |db| values::fields(db, named(db, "E")),
        },
        Pair {
            what: "a renamed field of a variant",
            before: "pub enum E { A { x: u8 } }",
            after: "pub enum E { A { y: u8 } }",
            changes: true,
            of: |db| values::fields(db, named(db, "E")),
        },
        Pair {
            what: "a trait method renamed with its impl",
            before: "pub trait T { fn m(&self); } impl T for u8 { fn m(&self) {} }",
            after: "pub trait T { fn n(&self); } impl T for u8 { fn n(&self) {} }",
            changes: true,
            of: |db| values::fields(db, named(db, "T")),
        },
        Pair {
            what: "an inherent method added",
            before: "pub struct S; impl S { pub fn a(&self) {} }",
            after: "pub struct S; impl S { pub fn a(&self) {} pub fn b(&self) {} }",
            changes: true,
            of: |db| values::fields(db, ItemId::Impl(nth_impl(db, 0))),
        },
        Pair {
            what: "a method's body edited",
            before: "pub struct S; impl S { pub fn a(&self) -> u32 { 1 } }",
            after: "pub struct S; impl S { pub fn a(&self) -> u32 { 2 } }",
            changes: false,
            of: |db| values::fields(db, ItemId::Impl(nth_impl(db, 0))),
        },
        Pair {
            what: "another item added",
            before: "pub struct S { pub a: u32 }",
            after: "pub struct S { pub a: u32 }\npub fn other() {}",
            changes: false,
            of: |db| values::fields(db, named(db, "S")),
        },
    ]);
}

/// A renamed trait item moves both the trait's signature and its `Fields`. The trait's form
/// names its items by place (`blaze_items::form`'s `trait_`) and the signature hash fills each
/// by its key as well (D51), so a renamed item, which is another key, moves it; `Fields` holds
/// the names a lookup by name compares, and moves on its own account (a name a macro's context
/// keys, which no item key spells, is `Fields`' alone). A type's fields and variants that hash
/// names itself (`for const` over them reads `Field::NAME` and `Variant::NAME`: `form`'s
/// `fields`), so a renamed field changes the type's signature and its `Fields`, which is over
/// that signature.
#[test]
fn a_renamed_trait_item_moves_the_signature_and_the_fields() {
    let signature =
        |db: &Db, name: &str| items_fact(Fact::Signature(key::item_key(db, named(db, name))));
    let value = |db: &Db, fact: FactRef| {
        let value = values::value(db, local_crate(db), fact);
        assert!(value.is_some(), "{fact:?} has no value");
        value
    };

    let (before, after) = (
        db_of(&format!("{PRELUDE}pub trait T {{ fn m(&self); }} impl T for u8 {{ fn m(&self) {{}} }}")),
        db_of(&format!("{PRELUDE}pub trait T {{ fn n(&self); }} impl T for u8 {{ fn n(&self) {{}} }}")),
    );
    assert_ne!(
        value(&before, signature(&before, "T")),
        value(&after, signature(&after, "T")),
        "a trait's signature names its items by place and key"
    );
    let fields = fields_of("T");
    assert_ne!(value(&before, fields(&before)), value(&after, fields(&after)));

    let (before, after) =
        (db_of("pub struct S { pub a: u32 }"), db_of("pub struct S { pub b: u32 }"));
    assert_ne!(
        value(&before, signature(&before, "S")),
        value(&after, signature(&after, "S")),
        "a type's signature names its fields"
    );
    let fields = fields_of("S");
    assert_ne!(value(&before, fields(&before)), value(&after, fields(&after)));
}

#[test]
fn attrs_changes_with_any_flag_the_signature_leaves_out() {
    check(&[
        Pair {
            what: "#[diagnostic::do_not_recommend] added",
            before: "pub trait T {} impl T for u8 {}",
            after: "pub trait T {} #[diagnostic::do_not_recommend] impl T for u8 {}",
            changes: true,
            of: |db| values::attrs(db, nth_impl(db, 0).into()).expect("an impl's attrs have a key"),
        },
        Pair {
            what: "#[must_use] added",
            before: "pub fn f() -> u8 { 1 }",
            after: "#[must_use] pub fn f() -> u8 { 1 }",
            changes: true,
            of: |db| attrs_of_item(db, "f"),
        },
        Pair {
            what: "the body edited",
            before: "pub fn f() -> u8 { 1 }",
            after: "pub fn f() -> u8 { 2 }",
            changes: false,
            of: |db| attrs_of_item(db, "f"),
        },
        // A layout reads an ADT's `#[repr]` (`AttrFlags::repr_assume_has`) as this fact: one
        // that changes the integer keeps the flags as they were, and must change the fact.
        Pair {
            what: "an enum's #[repr] integer changed",
            before: "#[repr(u8)] pub enum E { A, B }",
            after: "#[repr(u16)] pub enum E { A, B }",
            changes: true,
            of: |db| attrs_of_item_or_adt(db, "E"),
        },
        Pair {
            what: "a field added under the same #[repr]",
            before: "#[repr(C)] pub struct S { pub a: u8 }",
            after: "#[repr(C)] pub struct S { pub a: u8, pub b: u8 }",
            changes: false,
            of: |db| attrs_of_item_or_adt(db, "S"),
        },
    ]);
}

/// The invariant: the `FieldOrder` fact, which every ADT layout's read of the plan stands
/// for, is the plan's ID or that there is none: equal for an equal plan, another for another
/// plan, and back to the first with the plan gone.
#[test]
fn the_field_order_fact_changes_with_the_plan_and_only_then() {
    use hir_ty::layout::field_order::{self, FieldOrderPlan};
    let mut db = db_of(&format!("{PRELUDE}pub struct S {{ pub a: u32 }}"));
    let fact = values::field_order();
    let plan = |n: u8| {
        Some(Arc::new(FieldOrderPlan::new(
            ContentId([n; 32]),
            blaze_items::field_order::adt_key,
            HashMap::default(),
        )))
    };
    // The plan installed in the session arena from the next commit on, as the driver gives it;
    // the crate taken again after the commit, as an id is of the snapshot it was taken in.
    let install = |db: &mut Db, plan: Option<Arc<FieldOrderPlan>>| {
        let mut edit = db.edit();
        edit.set_given(
            None,
            field_order::plan_fact(plan.as_deref()),
            // clone: the plan's `Arc`, once per making of the session arena
            Box::new_in(
                move |builder: &blaze_memo::Builder, _: &blaze_memo::Inputs<'_>| {
                    field_order::install(builder, plan.clone());
                },
                Global,
            ),
        );
        _ = edit.commit();
    };
    let value = |db: &Db| values::value(db, local_crate(db), fact);
    let none = value(&db);
    assert!(none.is_some(), "no plan is a value");
    install(&mut db, plan(1));
    let one = value(&db);
    install(&mut db, plan(1));
    assert_eq!(value(&db), one, "the same plan again");
    install(&mut db, plan(2));
    let two = value(&db);
    assert!(none != one && one != two && none != two, "{none:?} {one:?} {two:?}");
    install(&mut db, None);
    assert_eq!(value(&db), none, "the plan gone");
}

fn attrs_of_item(db: &Db, name: &str) -> FactRef {
    let ItemId::Function(f) = named(db, name) else { panic!("`{name}` is no function") };
    values::attrs(db, f.into()).expect("a function's attrs have a key")
}

#[test]
fn crate_root_changes_with_the_root_and_not_with_a_body() {
    check(&[
        Pair {
            what: "a #![feature] added",
            before: "pub fn f() -> u8 { 1 }",
            after: "#![feature(never_type)]\npub fn f() -> u8 { 1 }",
            changes: true,
            of: |db| values::crate_root(db, local_crate(db)),
        },
        Pair {
            what: "a body edited",
            before: "pub fn f() -> u8 { 1 }",
            after: "pub fn f() -> u8 { 2 }",
            changes: false,
            of: |db| values::crate_root(db, local_crate(db)),
        },
        Pair {
            what: "an item added",
            before: "pub fn f() -> u8 { 1 }",
            after: "pub fn f() -> u8 { 1 }\npub struct New;",
            changes: false,
            of: |db| values::crate_root(db, local_crate(db)),
        },
    ]);
}

/// Lane items' kinds, read through [`values::value`].
/// An impl method whose type for the trait method's nested `impl Sized` is `<I as It>::Item`,
/// which the impl of `It` for `I` gives: `u8` here,
const NESTED_RPITIT_U8: &str = "pub trait It { type Item; } pub struct I; impl It for I { type Item = u8; }\n\
     pub trait T { fn f(&self) -> impl It<Item = impl Sized>; }\n\
     pub struct S; impl T for S { fn f(&self) -> I { I } }";
/// and `u16` here, with nothing else changed.
const NESTED_RPITIT_U16: &str = "pub trait It { type Item; } pub struct I; impl It for I { type Item = u16; }\n\
     pub trait T { fn f(&self) -> impl It<Item = impl Sized>; }\n\
     pub struct S; impl T for S { fn f(&self) -> I { I } }";

#[test]
fn items_facts_change_through_value() {
    check(&[
        Pair {
            what: "an impl added",
            before: "pub trait T {} pub struct S;",
            after: "pub trait T {} pub struct S; impl T for S {}",
            changes: true,
            of: |db| {
                let (ItemId::Trait(t), ItemId::Adt(s)) = (named(db, "T"), named(db, "S")) else {
                    panic!("`T` and `S` are a trait and a struct")
                };
                items_fact(implements(db, t, &adt_type(s)))
            },
        },
        Pair {
            what: "an impl of another trait added",
            before: "pub trait T {} pub trait U {} pub struct S;",
            after: "pub trait T {} pub trait U {} pub struct S; impl U for S {}",
            changes: false,
            of: |db| {
                let (ItemId::Trait(t), ItemId::Adt(s)) = (named(db, "T"), named(db, "S")) else {
                    panic!("`T` and `S` are a trait and a struct")
                };
                items_fact(implements(db, t, &adt_type(s)))
            },
        },
        Pair {
            what: "a method added",
            before: "pub struct S; impl S { pub fn a(&self) {} }",
            after: "pub struct S; impl S { pub fn a(&self) {} pub fn m(&self) {} }",
            changes: true,
            of: |db| {
                let ItemId::Adt(s) = named(db, "S") else { panic!("`S` is a struct") };
                items_fact(method(db, &adt_type(s), &Name::new_root("m")))
            },
        },
        Pair {
            what: "a const's value changed",
            before: "pub const C: u32 = 1 + 1;",
            after: "pub const C: u32 = 3;",
            changes: true,
            of: |db| {
                let ItemId::Const(c) = named(db, "C") else { panic!("`C` is a const") };
                items_fact(const_value(db, c))
            },
        },
        Pair {
            what: "a const's expression changed, its value not",
            before: "pub const C: u32 = 1 + 1;",
            after: "pub const C: u32 = 2;",
            changes: false,
            of: |db| {
                let ItemId::Const(c) = named(db, "C") else { panic!("`C` is a const") };
                items_fact(const_value(db, c))
            },
        },
        Pair {
            what: "an opaque type losing Send",
            before: "pub struct P(*const u8);\npub fn f() -> impl Sized { 1u8 }",
            after: "pub struct P(*const u8);\npub fn f() -> impl Sized { P(0 as *const u8) }",
            changes: true,
            of: |db| items_fact(auto_traits(db, named(db, "f"), 0)),
        },
        // The impl method's type for the nested `impl Sized` is `<I as It>::Item`, which another
        // impl gives: nothing its signature names changes, but the type does. The types are
        // their own fact (`Fact::RpititTys`), so that fact changes,
        Pair {
            what: "the type an impl method gives a nested `impl Trait` changed by another impl",
            before: NESTED_RPITIT_U8,
            after: NESTED_RPITIT_U16,
            changes: true,
            of: |db| items_fact(rpitit_tys(db, impl_method(db, "f"))),
        },
        // and the method's signature, which holds none of them (it is read while they are
        // found), does not.
        Pair {
            what: "the same change, asked of the impl method's signature",
            before: NESTED_RPITIT_U8,
            after: NESTED_RPITIT_U16,
            changes: false,
            of: |db| {
                items_fact(Fact::Signature(key::item_key(
                    db,
                    ItemId::Function(impl_method(db, "f")),
                )))
            },
        },
        Pair {
            what: "the impl method's hidden type changed, the trait's `impl Trait` as written",
            before: "pub trait T { fn f(&self) -> impl Sized; }\n\
                     pub struct S; impl T for S { fn f(&self) -> impl Sized { 1u8 } }",
            after: "pub trait T { fn f(&self) -> impl Sized; }\n\
                    pub struct S; impl T for S { fn f(&self) -> impl Sized { 1u16 } }",
            changes: false,
            of: |db| {
                items_fact(Fact::Signature(key::item_key(
                    db,
                    ItemId::Function(impl_method(db, "f")),
                )))
            },
        },
        Pair {
            what: "an opaque type's hidden type changed, still Send",
            before: "pub fn f() -> impl Sized { 1u8 }",
            after: "pub fn f() -> impl Sized { 1u16 }",
            changes: false,
            of: |db| items_fact(auto_traits(db, named(db, "f"), 0)),
        },
    ]);
}

#[test]
fn a_fact_of_this_crates_kind_that_names_nothing_has_no_value() {
    let db = db_of("pub struct S;");
    // A kind of this crate's range that no constant names, found rather than written down: a
    // literal one is taken the day a kind is added (as `0x180f` was, by `MODULES`), and the
    // test would then ask a real kind about a missing subject, which is `absent`, not `None`.
    for kind in kinds::ALL {
        assert!(kinds::RANGE.contains(&kind.0), "{kind:?} is outside this crate's range");
    }
    let free = kinds::RANGE
        .clone()
        .find(|code| !kinds::ALL.iter().any(|kind| kind.0 == *code))
        .expect("this crate's range has a kind no constant names");
    let unused = FactRef {
        kind: blaze_query::QueryKind(free),
        subject: ContentId([1; 32]),
        object: FactObject::None,
    };
    assert_eq!(values::value(&db, local_crate(&db), unused), None);
    let with_object =
        FactRef { object: FactObject::Field(0), ..values::fields(&db, named(&db, "S")) };
    assert_eq!(values::value(&db, local_crate(&db), with_object), None);
    let gone = FactRef { subject: ContentId([2; 32]), ..values::fields(&db, named(&db, "S")) };
    assert_eq!(values::value(&db, local_crate(&db), gone), Some(values::absent(kinds::FIELDS)));
}

/// A store in memory.
pub(super) struct Memory {
    images: Lock<HashMap<ContentId, Arc<Aligned>, Global>>,
}

impl Default for Memory {
    fn default() -> Memory {
        Memory { images: Lock::new("test.itemhash.images", Default::default()) }
    }
}

impl ImageStore for Memory {
    fn get(&self, key: &ContentId) -> Result<Option<ImageBuf>, String> {
        Ok(self.images.lock().get(key).map(|it| it.clone() as ImageBuf))
    }

    fn put(&self, key: ContentId, value: Vec<u8>) -> Result<(), String> {
        self.images.lock().entry(key).or_insert_with(|| Arc::new(Aligned::copy_from(&value)));
        Ok(())
    }

    // dyn: `ImageStore::pages`'s signature, once per crate read from pages
    fn pages(&self, key: &ContentId) -> Result<Option<Arc<dyn hir_def::deps::PageSource>>, String> {
        // A test's store holds each value whole: no part of one is read alone.
        Ok(hir_def::deps::whole_pages(self.get(key)?, std::alloc::Global))
    }

    // dyn: `ImageStore::for_each`'s signature, one call per batch of crates
    fn for_each(&self, count: usize, task: &(dyn Fn(usize) + Sync)) {
        // A test's store runs a batch in order on its own thread: the same result.
        (0..count).for_each(task);
    }

    fn reads_now(&self, _: &base_db::Env, _: &[u8]) -> Result<ContentId, String> {
        Err(String::from("an item store reads no expansion"))
    }
}

#[test]
fn hashing_this_crates_facts_again_allocates_nothing() {
    let db =
        db_of(&format!("{PRELUDE}{}", "pub struct S { pub a: u32 } pub trait T { fn m(&self); }"));
    let facts = [
        values::fields(&db, named(&db, "S")),
        values::fields(&db, named(&db, "T")),
        values::crate_root(&db, local_crate(&db)),
        attrs_of_item_or_adt(&db, "S"),
    ];
    let krate = local_crate(&db);
    for &fact in &facts {
        assert!(values::value(&db, krate, fact).is_some(), "{fact:?} has a value");
    }
    assert_at_most(0, "hashing this crate's facts again", || {
        for &fact in &facts {
            let _ = values::value(&db, krate, fact);
        }
    });
}

fn attrs_of_item_or_adt(db: &Db, name: &str) -> FactRef {
    let ItemId::Adt(adt) = named(db, name) else { panic!("`{name}` is no ADT") };
    values::attrs(db, adt.into()).expect("an ADT's attrs have a key")
}

fn body_named(db: &Db, name: &str) -> DefWithBodyId {
    match named(db, name) {
        ItemId::Function(it) => DefWithBodyId::FunctionId(it),
        ItemId::Static(it) => DefWithBodyId::StaticId(it),
        ItemId::Const(it) => DefWithBodyId::ConstId(it),
        _ => panic!("`{name}` has no body"),
    }
}

fn root_of(db: &Db) -> hir_def::ModuleId {
    hir_def::nameres::crate_def_map(db, local_crate(db)).root
}

fn name_in_root(name: &'static str) -> impl Fn(&Db) -> FactRef {
    move |db| values::resolves(db, root_of(db), &hir_expand::name::Name::new_root(name))
}

/// A name a body looked up and did not find is the `Resolves` fact with the value "nothing":
/// an item that later supplies the name changes it, another item does not. The control is
/// the third pair: were an absent name no fact (or one without a value), `check` would fail
/// on "the fact has no value before".
#[test]
fn a_name_fact_changes_when_the_name_is_supplied_or_shadowed_and_only_then() {
    check(&[
        Pair {
            what: "an absent name supplied",
            before: "pub fn f() -> u8 { 1 }",
            after: "pub fn f() -> u8 { 1 }\npub fn g() {}",
            changes: true,
            of: |db| name_in_root("g")(db),
        },
        Pair {
            what: "an absent name, another item added",
            before: "pub fn f() -> u8 { 1 }",
            after: "pub fn f() -> u8 { 1 }\npub fn h() {}",
            changes: false,
            of: |db| name_in_root("g")(db),
        },
        Pair {
            what: "a glob's name shadowed by an item",
            before: "mod m { pub fn g() {} }\npub use m::*;\npub fn f() { g() }",
            after: "mod m { pub fn g() {} }\npub use m::*;\npub fn g() {}\npub fn f() { g() }",
            changes: true,
            of: |db| name_in_root("g")(db),
        },
        Pair {
            what: "a found name, its body edited",
            before: "pub fn g() -> u8 { 1 }",
            after: "pub fn g() -> u8 { 2 }",
            changes: false,
            of: |db| name_in_root("g")(db),
        },
    ]);
}

#[test]
fn scope_and_crate_name_facts_change_with_what_they_hold_and_only_then() {
    check(&[
        Pair {
            what: "a trait brought into scope",
            before: "mod m { pub trait T {} }\npub fn f() {}",
            after: "mod m { pub trait T {} }\nuse m::T;\npub fn f() {}",
            changes: true,
            of: |db| values::traits_in_scope(db, root_of(db)),
        },
        Pair {
            what: "a function added, the traits in scope",
            before: "pub trait T {}\npub fn f() {}",
            after: "pub trait T {}\npub fn f() {}\npub fn g() {}",
            changes: false,
            of: |db| values::traits_in_scope(db, root_of(db)),
        },
        Pair {
            what: "a function added, the whole scope",
            before: "pub fn f() {}",
            after: "pub fn f() {}\npub fn g() {}",
            changes: true,
            of: |db| values::whole_scope(db, root_of(db)),
        },
        Pair {
            what: "a body edited, the whole scope",
            before: "pub fn f() -> u8 { 1 }",
            after: "pub fn f() -> u8 { 2 }",
            changes: false,
            of: |db| values::whole_scope(db, root_of(db)),
        },
        Pair {
            what: "an item added, the crate's names",
            before: "pub fn f() {}",
            after: "pub fn f() {}\npub struct New;",
            changes: false,
            of: |db| values::crate_names(db, local_crate(db)),
        },
        Pair {
            what: "a primitive type shadowed, the crate's names",
            before: "pub fn f() {}",
            after: "pub fn f() {}\n#[allow(non_camel_case_types)]\npub struct u8;",
            changes: true,
            of: |db| values::crate_names(db, local_crate(db)),
        },
    ]);
}

fn adt_named(db: &Db, name: &str) -> AdtId {
    match named(db, name) {
        ItemId::Adt(it) => it,
        _ => panic!("`{name}` is no ADT"),
    }
}

/// The invariant of `InherentAssocTypes`: the fact of an ADT changes when an associated type
/// of its inherent impls is added, renamed or moved to another impl of it, and not when
/// another item, or another ADT's inherent alias, is. The control is the pair that renames
/// the alias: a value made of the impls alone (a planted fact that ignored the rows' names)
/// would leave it equal, and `check` fails on it.
#[test]
fn inherent_assoc_types_fact_changes_with_the_list_and_only_then() {
    check(&[
        Pair {
            what: "an inherent alias added",
            before: "pub struct S;\nimpl S { pub fn f() {} }",
            after: "pub struct S;\nimpl S { pub fn f() {} pub type A = u8; }",
            changes: true,
            of: |db| values::inherent_assoc_types(db, adt_named(db, "S")),
        },
        Pair {
            what: "an inherent alias renamed",
            before: "pub struct S;\nimpl S { pub type A = u8; }",
            after: "pub struct S;\nimpl S { pub type B = u8; }",
            changes: true,
            of: |db| values::inherent_assoc_types(db, adt_named(db, "S")),
        },
        Pair {
            what: "an inherent alias moved to another impl of the ADT",
            before: "pub struct S;\nimpl S { pub type A = u8; }\nimpl S { pub fn f() {} }",
            after: "pub struct S;\nimpl S { }\nimpl S { pub fn f() {} pub type A = u8; }",
            changes: true,
            of: |db| values::inherent_assoc_types(db, adt_named(db, "S")),
        },
        Pair {
            what: "a function added",
            before: "pub struct S;\nimpl S { pub type A = u8; }",
            after: "pub struct S;\nimpl S { pub type A = u8; }\npub fn g() {}",
            changes: false,
            of: |db| values::inherent_assoc_types(db, adt_named(db, "S")),
        },
        Pair {
            what: "another ADT's inherent alias added",
            before: "pub struct S;\npub struct T;\nimpl S { pub type A = u8; }",
            after: "pub struct S;\npub struct T;\nimpl S { pub type A = u8; }\nimpl T { pub type A = u8; }",
            changes: false,
            of: |db| values::inherent_assoc_types(db, adt_named(db, "S")),
        },
    ]);
}

/// The value of `fact`, the local crate seeing it, in a database of `source` with an item
/// store of [`crate::Bodies`] installed (a body's step value is its answer in the store's
/// encoding, which names items through the store's facts).
fn step_fact_value(source: &str, fact: fn(&Db) -> FactRef) -> Option<ContentId> {
    let db = db_of(&format!("{PRELUDE}{source}"));
    let installed = body_store::install(
        &db,
        Arc::new(Memory::default()),
        Arc::new(crate::Bodies),
        ContentId([7; 32]),
        Mode::Serve,
    )
    .expect("an item store for the test database");
    let value = values::value(&db, local_crate(&db), fact(&db));
    drop(installed);
    value
}

fn k_infer(db: &Db) -> FactRef {
    values::step_fact_of(db, body_named(db, "k"), Step::Infer).expect("an inference step fact")
}

fn k_mir(db: &Db) -> FactRef {
    values::step_fact_of(db, body_named(db, "k"), Step::Mir).expect("a MIR step fact")
}

/// The invariant of another body's step fact (a `const fn` evaluated, a coroutine's
/// witnesses): its value is the hash of that body's answer, so it changes exactly when the
/// answer does. A literal changed is the same inference and another MIR; a return type changed
/// is another inference; another body's edit changes neither. The control is the literal pair
/// under the inference: a value made of the body's text (a planted fact keyed by its source,
/// not its answer) would change there, and the assertion fails on it.
#[test]
fn another_bodys_step_fact_changes_with_its_answer_and_only_then() {
    let cases: [(&str, &str, &str, fn(&Db) -> FactRef, bool); 4] = [
        ("a literal, the MIR", "pub const fn k() -> u32 { 1 }", "pub const fn k() -> u32 { 2 }", k_mir, true),
        ("a literal, the inference", "pub const fn k() -> u32 { 1 }", "pub const fn k() -> u32 { 2 }", k_infer, false),
        ("a return type, the inference", "pub fn k() -> u32 { 1 }", "pub fn k() -> u64 { 1 }", k_infer, true),
        (
            "another body edited, the MIR",
            "pub fn k() -> u32 { 1 }\npub fn j() -> u32 { 1 }",
            "pub fn k() -> u32 { 1 }\npub fn j() -> u32 { 3 }",
            k_mir,
            false,
        ),
    ];
    for (what, before, after, fact, changes) in cases {
        let (was, is) = (step_fact_value(before, fact), step_fact_value(after, fact));
        assert!(was.is_some() && is.is_some(), "{what}: a step fact with no value: {was:?} {is:?}");
        assert_eq!(was != is, changes, "{what}");
    }
}

/// The invariant of `CrateSettings`: a dependency's settings fact (what a row of its `Crate`
/// input stands for in a body of another crate, `reads::fact_of`) stays across an edit of a
/// private body of it and moves with its edition. The control is the edition: a value made of
/// nothing of the crate would stay there too.
#[test]
fn a_crates_settings_fact_moves_with_its_settings_and_not_its_bodies() {
    let source = |dep_header: &str, body: &str| {
        format!(
            "//- /main.rs crate:main deps:dep\npub fn f() -> u32 {{ dep::g() }}\n\
             //- /dep.rs {dep_header}\npub fn g() -> u32 {{ 1 }}\nfn private() -> u32 {{ {body} }}\n"
        )
    };
    let settings = |text: &str| {
        let db = super::perturb::db(text);
        let main = super::perturb::body(&db, "f").krate(&db);
        let dep = super::perturb::body(&db, "private").krate(&db);
        values::value(&db, main, values::crate_settings(&db, dep))
            .expect("a dependency's settings fact has a value")
    };
    let base = settings(&source("crate:dep", "1"));
    assert_eq!(
        base,
        settings(&source("crate:dep", "2")),
        "a private body's edit moved its crate's settings fact"
    );
    assert_ne!(
        base,
        settings(&source("crate:dep edition:2015", "1")),
        "the edition of a crate left its settings fact"
    );
}
