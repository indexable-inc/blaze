//! What each read of a body's steps stands for (docs/v2/plan/design/item-hash.md section 9,
//! docs/v2/plan/design/item-hash.md): each fact changes across a pair of programs where
//! what it stands for changed and stays where it did not; every read the steps make over
//! fixtures with method calls, operators, closures, `async`, `?`, `for`, patterns, consts and
//! generics is covered, a fact, or in the refusal list below with why; and classifying a read
//! or hashing one of this crate's facts again allocates nothing.

use std::sync::Arc;

use base_db::{Crate, Db, all_crates};
use blaze_alloc_count::assert_at_most;
use blaze_format::Aligned;
use blaze_hash::HashMap;
use blaze_ids::{ContentId, Hasher};
use blaze_items::{Fact, ItemId, auto_traits, const_value, implements, key, method};
use hir_def::{
    AdtId, DefWithBodyId, HasModule, ImplId, ModuleDefId,
    deps::{ImageBuf, ImageStore},
    nameres::{crate_def_map, demand::crate_body_owners},
};
use hir_expand::fixture;
use hir_expand::name::Name;
use hir_ty::{
    InferBodyId, InferenceResult,
    body_store::{self, BodyFacts, Classified, Mode, Step},
    db::HirDatabase,
    next_solver::{
        SolverDefId,
        goal_store::{FactObject, FactRef},
    },
};
use blaze_core::lock::Lock;
use rustc_type_ir::fast_reject::SimplifiedType;
use std::alloc::Global;

use crate::{
    reads::{self, RULES, Rule},
    values::{self, kinds},
};

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

/// A lane items fact as its `FactRef`, as `blaze_items::table` splits it.
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

fn items_fact(fact: Fact) -> FactRef {
    let (subject, object) = match fact {
        Fact::Method { self_ty, name } => (self_ty.0, FactObject::Id(name.0)),
        Fact::Implements { trait_, self_ty } => (trait_.0, FactObject::Id(self_ty.0)),
        Fact::Signature(it) | Fact::ConstValue(it) | Fact::AutoTraits(it) => {
            (it.0, FactObject::None)
        }
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
        // alloc: test fixtures, once per pair.
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

/// Why `Fields` exists: lane items' interface hash names members by place, so it stays when a
/// member is renamed.
#[test]
fn a_rename_keeps_the_signature_and_changes_the_fields() {
    let (before, after) =
        (db_of("pub struct S { pub a: u32 }"), db_of("pub struct S { pub b: u32 }"));
    let signature = |db: &Db| items_fact(Fact::Signature(key::item_key(db, named(db, "S"))));
    let value = |db: &Db, fact: FactRef| values::value(db, local_crate(db), fact);
    assert_eq!(value(&before, signature(&before)), value(&after, signature(&after)));
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
    ]);
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
        // impl gives: nothing its signature names changes, but the type does.
        Pair {
            what: "the type an impl method gives a nested `impl Trait` changed by another impl",
            before: "pub trait It { type Item; } pub struct I; impl It for I { type Item = u8; }\n\
                     pub trait T { fn f(&self) -> impl It<Item = impl Sized>; }\n\
                     pub struct S; impl T for S { fn f(&self) -> I { I } }",
            after: "pub trait It { type Item; } pub struct I; impl It for I { type Item = u16; }\n\
                    pub trait T { fn f(&self) -> impl It<Item = impl Sized>; }\n\
                    pub struct S; impl T for S { fn f(&self) -> I { I } }",
            changes: true,
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
    let unused = FactRef {
        kind: blaze_query::QueryKind(0x180f),
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

#[test]
fn rules_are_sorted_and_name_one_rule_per_ingredient() {
    for pair in RULES.windows(2) {
        let ((a, at_a, _), (b, at_b, _)) = (pair[0], pair[1]);
        assert!((a, at_a) < (b, at_b), "RULES: {a} ({at_a}) before {b} ({at_b})");
    }
    for &(name, file, _) in RULES {
        assert!(file.ends_with(".rs") && !file.starts_with('/'), "RULES: {name}'s file {file}");
    }
}

/// Every read the table may call covered is one the identity says it stands for, with the
/// same key, and back (design section 6).
#[test]
fn the_covered_reads_are_the_identitys() {
    use crate::identity::Key;
    let mut ours: Vec<(&str, Key)> = RULES
        .iter()
        .filter_map(|&(name, _, rule)| match rule {
            Rule::Names => Some((name, Key::Any)),
            Rule::OwnBody | Rule::OwnStore => Some((name, Key::OwnBody)),
            Rule::CrateInput | Rule::OwnCrate(_) => Some((name, Key::OwnCrate)),
            _ => None,
        })
        .collect();
    let mut theirs: Vec<(&str, Key)> = crate::identity::covered_queries().to_vec();
    ours.sort_unstable_by_key(|it| it.0);
    ours.dedup();
    theirs.sort_unstable_by_key(|it| it.0);
    theirs.dedup();
    let only_ours: Vec<_> = ours.iter().filter(|it| !theirs.contains(it)).collect();
    let only_theirs: Vec<_> = theirs.iter().filter(|it| !ours.contains(it)).collect();
    assert!(
        only_ours.is_empty() && only_theirs.is_empty(),
        "covered by reads.rs only: {only_ours:?}; by identity::covered_queries only: {only_theirs:?}"
    );
}

/// The steps' fixture: one body for each construct the closure test is about.
const STEPS: &str = r#"
//- minicore: sized, copy, clone, derive, add, fn, iterator, option, result, try, future, async_fn, index, range, deref, from, send, sync
pub trait Show { fn show(&self) -> u8; const ID: u8; }
#[derive(Clone, Copy)]
pub struct Point { pub x: u32, pub y: u32 }
pub enum Shape { Dot(Point), Line { from: Point, to: Point }, Empty }
pub const LIMIT: usize = 3 + 4;
pub static ORIGIN: Point = Point { x: 0, y: 0 };
impl Show for Point { fn show(&self) -> u8 { self.x as u8 } const ID: u8 = 1; }
impl Point {
    pub fn new(x: u32, y: u32) -> Point { Point { x, y } }
    pub fn sum(&self) -> u32 { self.x + self.y }
}
impl core::ops::Add for Point {
    type Output = Point;
    fn add(self, o: Point) -> Point { Point { x: self.x + o.x, y: self.y + o.y } }
}
pub fn methods(p: &Point) -> u32 { p.sum() + p.show() as u32 + <Point as Show>::ID as u32 }
pub fn operators(a: Point, b: Point) -> u32 { let c = a + b; c.x * 2 - c.y / 1 }
pub fn closures(n: u32) -> u32 { let f = |x: u32| x + n; let g = move || n; f(1) + g() }
pub async fn later(n: u32) -> u32 { n }
pub async fn awaits() -> u32 { later(1).await + 1 }
pub fn question(r: Result<u32, ()>) -> Result<u32, ()> { let v = r?; Ok(v + 1) }
pub fn optional(o: Option<u32>) -> Option<u32> { Some(o? * 2) }
pub fn loops(n: u32) -> u32 { let mut t = 0; for i in 0..n { t = t + i; } t }
pub fn patterns(s: Shape) -> u32 {
    match s {
        Shape::Dot(Point { x, .. }) => x,
        Shape::Line { from, to: Point { y, .. } } => from.x + y,
        Shape::Empty => 0,
    }
}
pub fn consts() -> usize { let a = [0u8; LIMIT]; let _ = a; LIMIT * 2 + ORIGIN.x as usize }
pub fn generic<T: Show + Clone>(t: &T) -> u8 { t.clone().show() }
pub fn calls_generic(p: &Point) -> u8 { generic(p) + Point::new(1, 2).show() }
pub enum Small { A = 1, B = LIMIT as isize }
pub fn hidden(n: u32) -> impl Copy + Send { n }
pub fn sees_hidden() -> u32 { let h = hidden(2); let _ = h; Small::B as u32 + ORIGIN.y }
pub trait Make { fn make(&self) -> impl Copy; }
impl Make for Point { fn make(&self) -> u32 { self.x } }
pub fn makes(p: &Point) -> u32 { p.make() + 1 }
pub fn copies<M: Make>(m: &M) { let a = m.make(); let b = a; let _ = (a, b); }
"#;

/// Reads the closure test allows to be refused, each with why. A refused read keeps its step
/// out of the store; it is never a wrong answer. Each is to be made a fact or covered by the
/// request named, never by widening a rule here.
const REFUSALS: &[(&str, &str)] = &[
    // Tables of the body's own crate: another crate's are its `Crate` fact, but no fact lists
    // the own crate's impls or traits.
    ("InherentImpls::in_crate_for_self_ty", "the own crate's impls"),
    ("TraitImpls::in_crate_and_deps_for_trait_and_self_ty", "the own crate's impls"),
    ("TraitImpls::in_crate_for_trait_and_self_ty", "the own crate's impls"),
    ("TraitImpls::blanket_in_crate_and_deps", "the own crate's impls"),
    ("TraitImpls::for_crate_and_trait", "the own crate's impls"),
    ("in_crate_trait_value_items", "the own crate's traits"),
    ("module_items_named", "the items of the traits in scope of an own module"),
    ("InherentImpls::in_block_for_self_ty", "a block's impls: no fact yet"),
    ("closure_exprs", "read for another body's closures"),
    // Of a type or a goal in an environment: no fact names one.
    ("lookup_impl_method_stored", "a method's impl, selected in an environment"),
    ("layout_of_ty_query", "a type's layout in an environment"),
    ("layout_of_adt_query", "an ADT's layout for its arguments in an environment"),
    ("is_copy_query", "a type in an environment"),
    ("drop_glue_query", "a type in an environment"),
    ("prove_query", "a goal in an environment"),
    ("normalize_query", "a type in an environment"),
    ("auto_trait_depth", "an ADT's auto trait, for a crate"),
    ("copy_clone_impl", "an ADT's Copy or Clone impl, for a crate"),
    // A const's value, a leak the signature hash marks (design section 4): `ConstValue` names
    // a const evaluated as itself only.
    ("const_eval_query", "a const with arguments or an environment"),
    ("anon_const_eval", "an anonymous const: ConstValue names none"),
    // Keyed by a block of another body or a whole-crate walk: no fact names them.
    ("TraitImpls::for_block", "a block's impls: no fact yet"),
    ("InherentImpls::for_block", "a block's impls: no fact yet"),
    ("table", "a walk of every module of a crate (hir-ty lower/path.rs inherent_associated_type)"),
    ("modules", "a walk of every module of a published crate (DefMap::table)"),
    // Positions: the identity has no spans.
    ("Body::source_map", "a source map: positions"),
    ("Body::relative", "a source map: positions"),
    ("FunctionSignature::with_source_map", "a source map: positions"),
    ("FunctionSignature::relative", "a source map: positions"),
    // From other bodies' borrow checks (firewall E3): no fact yet.
    ("coroutine_witnesses_query", "an async body's witness types"),
    ("opaque_variances_query", "an opaque type's variances"),
];

/// Lane item-hash's side, with a body's identity its debug name (one database only) and every
/// classification kept.
struct Probe {
    seen: Lock<Vec<Seen>>,
}

#[derive(Clone, Copy, Debug)]
struct Seen {
    def: DefWithBodyId,
    step: Step,
    read: blaze_query::db::DatabaseKeyIndex,
    kind: blaze_query::db::ReadKind,
    name: &'static str,
    class: Classified,
}

impl BodyFacts for Probe {
    fn identity(&self, _: &Db, def: DefWithBodyId) -> Option<ContentId> {
        // alloc: test identity, once per lookup.
        Some(
            Hasher::new("blaze v2 item-hash classify test")
                .part(format!("{def:?}").as_bytes())
                .finish(),
        )
    }

    fn classify(
        &self,
        db: &Db,
        def: DefWithBodyId,
        step: Step,
        read: blaze_query::db::DatabaseKeyIndex,
        kind: blaze_query::db::ReadKind,
        name: &'static str,
    ) -> Classified {
        let class = reads::classify(db, def, step, read, kind, name);
        self.seen.lock().push(Seen { def, step, read, kind, name, class });
        class
    }

    fn value(&self, db: &Db, def: DefWithBodyId, fact: FactRef) -> Option<ContentId> {
        values::value_in(db, def, fact)
    }

    fn def_key<'db>(&self, db: &'db Db, body: DefWithBodyId, def: SolverDefId<'db>) -> ContentId {
        crate::keys::def_key(db, body, def)
    }

    fn def_of<'db>(
        &self,
        db: &'db Db,
        body: DefWithBodyId,
        key: ContentId,
    ) -> Option<SolverDefId<'db>> {
        crate::keys::def_of(db, body, key)
    }
}

/// A store in memory.
struct Memory {
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

    fn reads_now(&self, _: &base_db::Env, _: &[u8]) -> Result<ContentId, String> {
        Err(String::from("an item store reads no expansion"))
    }
}

/// Runs inference, MIR building and the borrow check of every body of [`STEPS`] with an item
/// store installed, and gives what each read was classified as.
fn steps_seen() -> (Db, Vec<Seen>) {
    steps_seen_in(db_of(STEPS))
}

/// [`steps_seen`] of every body of `db`'s local crate.
fn steps_seen_in(db: Db) -> (Db, Vec<Seen>) {
    let probe = Arc::new(Probe { seen: Lock::new("test.itemhash.seen", Vec::new()) });
    let installed = body_store::install(
        &db,
        Arc::new(Memory::default()),
        probe.clone(),
        ContentId([7; 32]),
        Mode::Serve,
    )
    .expect("an item store for the test database");
    let bodies = crate_body_owners(&db, local_crate(&db));
    for &def in &bodies {
        let _ = InferenceResult::of(&db, def);
    }
    for &def in &bodies {
        let _ = db.mir_body(InferBodyId::DefWithBodyId(def));
    }
    for &def in &bodies {
        let _ = hir_ty_borrowck::checked(&db, def);
    }
    installed.flush().expect("the test store flushes");
    drop(installed);
    let seen = std::mem::take(&mut *probe.seen.lock());
    (db, seen)
}

#[test]
fn every_read_of_the_steps_is_covered_a_fact_or_a_listed_refusal() {
    let (_db, seen) = steps_seen();
    assert!(seen.iter().any(|it| it.step == Step::Infer), "no inference read was classified");
    let mut unlisted: Vec<(Step, &str)> = seen
        .iter()
        .filter(|it| it.class == Classified::Refused)
        .filter(|it| !REFUSALS.iter().any(|&(name, _)| reads::cmp_listed(name, it.name).is_eq()))
        .map(|it| (it.step, it.name))
        .collect();
    unlisted.sort_unstable();
    unlisted.dedup();
    assert!(unlisted.is_empty(), "reads refused with no reason in REFUSALS: {unlisted:#?}");
}

/// A rule that names an ingredient no read of the steps is of is checked by the fact pairs
/// above, not here; this asserts what the closure test relies on: a read the table names is
/// never refused for its file, so the file suffixes match where the engine says each ingredient
/// is.
#[test]
fn a_read_the_table_names_by_name_is_found_by_its_file() {
    let (db, seen) = steps_seen();
    let named: Vec<&Seen> = seen
        .iter()
        .filter(|it| RULES.iter().any(|&(name, _, _)| reads::cmp_listed(name, it.name).is_eq()))
        .collect();
    assert!(!named.is_empty(), "no read of the steps is of an ingredient the table names");
    for it in named {
        assert!(
            reads::rule_of(&db, it.read, it.name).is_some(),
            "{}: in RULES, but its ingredient's file is none of those listed",
            it.name
        );
    }
}

/// What a body leaks into a signature (a hidden type, a static's value, a discriminant) and a
/// step reads is lane items' fact of it, and a read of a fact's value is that fact (design
/// section 4): never refused, never covered.
#[test]
fn leak_reads_are_lane_items_facts() {
    let (db, seen) = steps_seen();
    for it in &seen {
        let want = match it.name {
            "rpit_hidden_types" | "tait_hidden_types" => Some(blaze_items::kinds::HIDDEN_TYPES),
            "const_eval_static_query" | "const_eval_discriminant_variant" => {
                Some(blaze_items::kinds::CONST_VALUE)
            }
            _ => None,
        };
        if let Some(kind) = want {
            let Classified::Fact(fact) = it.class else {
                panic!("{}: {:?}, not a fact", it.name, it.class)
            };
            assert_eq!(fact.kind, kind, "{}", it.name);
            assert!(
                values::value(&db, local_crate(&db), fact).is_some(),
                "{}: {fact:?} has a value",
                it.name
            );
        }
        if it.name == "fact_value" {
            assert!(matches!(it.class, Classified::Fact(_)), "fact_value: {:?}", it.class);
        }
    }
}

/// A generic body's goals about a trait method's `impl Trait` (`<M as Make>::{impl Trait#0}:
/// Copy`) read its bounds (`rpitit_item_bounds`), which stand for the trait method's signature
/// (`blaze_items::goals`), so that read keeps no goal out of the goal store. (The same goals
/// still read the item's variances, `opaque_variances_query`, which no fact names yet.)
#[test]
fn rpitit_reads_keep_no_goal_out_of_the_goal_store() {
    let db = db_of(
        r#"
//- minicore: sized, copy, clone, add
pub trait Make { fn make(self) -> impl Copy; }
impl Make for u32 { fn make(self) -> u32 { self } }
pub fn wants<T: Copy>(t: T) -> T { t }
pub fn concrete() -> u32 { let a = wants(5u32.make()); a + a }
pub fn copies<M: Make>(m: M) { let a = m.make(); let b = wants(a); let _ = (a, b); }
"#,
    );
    let installed = hir_ty::next_solver::goal_store::install(
        &db,
        Arc::new(Memory::default()),
        Arc::new(blaze_items::goals::Goals),
        ContentId([7; 32]),
    )
    .expect("a goal store for the test database");
    for &def in &crate_body_owners(&db, local_crate(&db)) {
        let _ = InferenceResult::of(&db, def);
        let _ = db.mir_body(InferBodyId::DefWithBodyId(def));
        let _ = hir_ty_borrowck::checked(&db, def);
    }
    let flushed = installed.flush().expect("the test goal store flushes");
    assert!(flushed.written > 0, "no goal was stored: {flushed:?}");
    let kept_out: Vec<&(&str, u64)> = // alloc: test
        flushed.inexpressible.iter().filter(|(name, _)| name.contains("rpitit")).collect();
    assert!(kept_out.is_empty(), "goals kept out by reads that stand for no fact: {kept_out:?}");
}

/// An impl method that returns its own `impl Trait` for the trait method's (wasmtime 50's
/// `WasmTyList::valtypes` and `AccessorTask::run`): `impl_rpitit_tys` of it proves the trait's
/// bounds of the impl's opaque type (`Opaque(valtypes, 0): Iterator<Item = u32>`), which reads
/// that opaque's bounds, the impl method's `Signature` fact. With the goal store installed it
/// hashes that fact as it offers the goal; were the method's types for the trait's
/// `impl Trait`s in its signature, that hash would read `impl_rpitit_tys` of it again, a cycle
/// (`impl_rpitit_tys -> fact_value -> signature_of -> signature_form`). They are their own
/// fact (`blaze_items::Fact::RpititTys`), which a caller's read stands for.
#[test]
fn an_impl_methods_own_impl_trait_for_the_trait_methods_hashes_no_cycle() {
    let db = db_of(
        r#"
//- minicore: sized, copy, clone, iterator, option
pub struct Once(u32);
impl Iterator for Once { type Item = u32; fn next(&mut self) -> Option<u32> { None } }
pub trait List { fn valtypes() -> impl Iterator<Item = u32>; }
impl List for (u32,) { fn valtypes() -> impl Iterator<Item = u32> { Once(1) } }
pub trait Make { fn make(self) -> impl Copy; }
impl Make for u32 { fn make(self) -> impl Copy { self } }
pub fn first<L: List>() -> Option<u32> { L::valtypes().next() }
pub fn concrete() -> Option<u32> { let _ = 5u32.make(); <(u32,) as List>::valtypes().next() }
"#,
    );
    let installed = hir_ty::next_solver::goal_store::install(
        &db,
        Arc::new(Memory::default()),
        Arc::new(blaze_items::goals::Goals),
        ContentId([7; 32]),
    )
    .expect("a goal store for the test database");
    let krate = local_crate(&db);
    // The impl's types first, as the outermost query, as wasmtime's check reached them.
    for name in ["valtypes", "make"] {
        let method = impl_method(&db, name);
        let values = hir_ty::rpitit::impl_rpitit_tys(&db, method);
        assert!(
            values.len() == 1 && values.iter().all(Result::is_ok),
            "`{name}` gives its `impl Trait` a type"
        );
        let fact = FactRef {
            kind: blaze_items::kinds::RPITIT_TYS,
            subject: key::item_key(&db, ItemId::Function(method)).0,
            object: FactObject::None,
        };
        assert!(values::value(&db, krate, fact).is_some(), "`{name}`: {fact:?} has a value");
    }
    for &def in &crate_body_owners(&db, krate) {
        let _ = InferenceResult::of(&db, def);
        let _ = db.mir_body(InferBodyId::DefWithBodyId(def));
    }
    let flushed = installed.flush().expect("the test goal store flushes");
    // Goals were offered and kept, so their facts were hashed where the cycle was.
    assert!(flushed.written > 0, "no goal was stored: {flushed:?}");
}

/// A read keyed by several arguments (request 00827) is the fact of what its first names: a
/// crate's table its `Crate` fact, refused for the body's own; an impl's or trait's item of a
/// name its `Fields`; a const evaluated as itself its `ConstValue`.
#[test]
fn several_argument_reads_are_facts_of_their_first() {
    let (db, seen) = steps_seen();
    let mut facts = 0;
    for it in &seen {
        let want = match reads::rule_of(&db, it.read, it.name) {
            Some(Rule::DepCrate(_)) => blaze_items::kinds::CRATE,
            Some(Rule::FieldNamed(_)) => kinds::FIELDS,
            Some(Rule::ConstEval) => blaze_items::kinds::CONST_VALUE,
            _ => continue,
        };
        match it.class {
            Classified::Fact(fact) => {
                assert_eq!(fact.kind, want, "{}", it.name);
                let own = key::crate_key(&db, it.def.krate(&db));
                assert!(want != blaze_items::kinds::CRATE || fact.subject != own, "{}", it.name);
                facts += 1;
            }
            Classified::Refused => assert_ne!(want, kinds::FIELDS, "{}: refused", it.name),
            Classified::Covered => panic!("{}: covered", it.name),
        }
    }
    assert!(facts > 0, "no read of the steps by several arguments is a fact");
}

/// A later step's read of the body's own earlier step is that step's fact: MIR reads its
/// inference, the borrow check its MIR and its closures' and async blocks' MIR.
#[test]
fn own_earlier_steps_are_step_facts() {
    let (db, seen) = steps_seen();
    let (mut infer, mut mir) = (false, false);
    for it in &seen {
        let Some(Rule::OwnStep(earlier)) = reads::rule_of(&db, it.read, it.name) else { continue };
        match it.class {
            Classified::Fact(fact) => {
                assert!(earlier < it.step, "{}: a fact for {:?}", it.name, it.step);
                assert_eq!(Some(fact), values::step_fact(earlier), "{}", it.name);
                infer |= fact.kind == kinds::STEP_INFER;
                mir |= fact.kind == kinds::STEP_MIR;
            }
            Classified::Refused => {}
            Classified::Covered => panic!("{}: covered", it.name),
        }
    }
    assert!(infer && mir, "no MIR read of inference ({infer}) or borrow check read of MIR ({mir})");
}

#[test]
fn names_rules_are_covered_for_any_key() {
    let (db, seen) = steps_seen();
    for it in &seen {
        if reads::rule_of(&db, it.read, it.name) == Some(Rule::Names) {
            assert_eq!(it.class, Classified::Covered, "{}", it.name);
        }
    }
}

#[test]
fn classifying_again_allocates_nothing() {
    let (db, seen) = steps_seen();
    for it in &seen {
        let _ = reads::classify(&db, it.def, it.step, it.read, it.kind, it.name);
    }
    assert_at_most(0, "classifying every read of the steps again", || {
        for it in &seen {
            let _ = reads::classify(&db, it.def, it.step, it.read, it.kind, it.name);
        }
    });
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

/// Two statics and two functions alike but for their names: each pair has one identity.
const TWINS: &str = r#"
//- minicore: sized, copy, clone, add
pub static FIRST: u32 = 1 + 2;
pub static SECOND: u32 = 1 + 2;
pub fn one(n: u32) -> u32 { n + 1 }
pub fn two(n: u32) -> u32 { n + 1 }
"#;

fn body_named(db: &Db, name: &str) -> DefWithBodyId {
    match named(db, name) {
        ItemId::Function(it) => DefWithBodyId::FunctionId(it),
        ItemId::Static(it) => DefWithBodyId::StaticId(it),
        ItemId::Const(it) => DefWithBodyId::ConstId(it),
        _ => panic!("`{name}` has no body"),
    }
}

/// The shapes of each step of the bodies of `a` and `b`, which have one identity, compared:
/// `Ok` with how many facts of them name the body's own item, `Err` naming a step the two list
/// different facts for. `listed` is how a shape lists a fact of a body: as it was classified,
/// or as a planted bug lists it.
fn alike_bodies_list_one_shape(
    db: &Db,
    seen: &[Seen],
    (a, b): (&str, &str),
    listed: impl Fn(DefWithBodyId, FactRef) -> FactRef,
) -> Result<usize, String> {
    let (first, second) = (body_named(db, a), body_named(db, b));
    assert_eq!(
        crate::identity::identity(db, first),
        crate::identity::identity(db, second),
        "`{a}` and `{b}` are alike but for their names: one identity"
    );
    let mut own = 0;
    for step in Step::ALL {
        let shape = |def: DefWithBodyId| {
            let mut facts: Vec<FactRef> = seen
                .iter()
                .filter(|it| it.def == def && it.step == step)
                .filter_map(|it| match it.class {
                    Classified::Fact(fact) => Some(listed(def, fact)),
                    Classified::Covered | Classified::Refused => None,
                })
                .collect();
            facts.sort_unstable();
            facts.dedup();
            facts
        };
        let (of_first, of_second) = (shape(first), shape(second));
        if of_first != of_second {
            return Err(format!(
                "`{a}` and `{b}` have one identity and two shapes of their {step:?}: {of_first:?} and {of_second:?}"
            ));
        }
        own += of_first.iter().filter(|it| it.subject == values::OWN).count();
    }
    Ok(own)
}

/// The invariant: two bodies of one identity that read the same of themselves list one shape
/// for each step, so the store keeps one shape and one answer for both, and a rename keeps a
/// body's records. A body's identity writes its own item as "self"; a fact about that item
/// (its inference as its MIR reads it, its MIR and match check as its borrow check does) names
/// it as [`values::OWN`], and its value is the body's own ([`values::value_in`]).
///
/// Until 2026-11-10 such a fact named the item by its key: `FIRST` and `SECOND` filed two
/// shapes of their MIR under one base key in one flush, which the daemon's store refused
/// ("keeping the shape of the MIR of StaticId(..): two images were made under ..") after every
/// build of ripgrep.
#[test]
fn bodies_alike_but_for_their_names_list_one_shape() {
    let (db, seen) = steps_seen_in(super::perturb::db(TWINS));
    for pair in [("FIRST", "SECOND"), ("one", "two")] {
        let own = alike_bodies_list_one_shape(&db, &seen, pair, |_, fact| fact)
            .unwrap_or_else(|why| panic!("{why}"));
        assert!(own > 0, "{pair:?}: no step read a fact of its own body, so nothing was tested");
        // With no body to say whose, a fact of "its own" has no value: never `absent`, which
        // every body's would share.
        let def = body_named(&db, pair.0);
        let infer = values::step_fact(Step::Infer).expect("inference has a fact");
        assert_eq!(values::value(&db, def.krate(&db), infer), None);
    }
}

/// The control: a shape that names the body's own item by its key, as before the fix, gives
/// two bodies of one identity two shapes, and the oracle names the step.
#[test]
fn the_oracle_catches_a_shape_that_names_its_own_body_by_key() {
    let (db, seen) = steps_seen_in(super::perturb::db(TWINS));
    let by_key = |def: DefWithBodyId, fact: FactRef| match fact.subject == values::OWN {
        true => FactRef { subject: values::body_key(&db, def), ..fact },
        false => fact,
    };
    for pair in [("FIRST", "SECOND"), ("one", "two")] {
        let why = alike_bodies_list_one_shape(&db, &seen, pair, by_key)
            .expect_err("each body's key is its own");
        assert!(why.contains("one identity and two shapes of their"), "{why}");
    }
}
