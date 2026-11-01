//! What each read of a body's steps stands for (docs/v2/plan/design/item-hash.md section 9,
//! docs/v2/plan/lanes/item-hash/reads.md): each fact changes across a pair of programs where
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
use parking_lot::Mutex;
use rustc_type_ir::fast_reject::SimplifiedType;

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
fn items_fact(fact: Fact) -> FactRef {
    let (subject, object) = match fact {
        Fact::Method { self_ty, name } => (self_ty.0, FactObject::Id(name.0)),
        Fact::Implements { trait_, self_ty } => (trait_.0, FactObject::Id(self_ty.0)),
        Fact::Signature(it) | Fact::ConstValue(it) | Fact::AutoTraits(it) => (it.0, FactObject::None),
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
        let (before, after) =
            (db_of(&format!("{PRELUDE}{}", pair.before)), db_of(&format!("{PRELUDE}{}", pair.after)));
        let fact = (pair.of)(&before);
        let was = values::value(&before, fact);
        let is = values::value(&after, (pair.of)(&after));
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
    assert_eq!(values::value(&before, signature(&before)), values::value(&after, signature(&after)));
    let fields = fields_of("S");
    assert_ne!(values::value(&before, fields(&before)), values::value(&after, fields(&after)));
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
    let unused = FactRef { kind: blaze_query::QueryKind(0x180f), subject: ContentId([1; 32]), object: FactObject::None };
    assert_eq!(values::value(&db, unused), None);
    let with_object = FactRef { object: FactObject::Field(0), ..values::fields(&db, named(&db, "S")) };
    assert_eq!(values::value(&db, with_object), None);
    let gone = FactRef { subject: ContentId([2; 32]), ..values::fields(&db, named(&db, "S")) };
    assert_eq!(values::value(&db, gone), Some(values::absent(kinds::FIELDS)));
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
    ("Body::with_source_map", "a source map: positions"),
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
    seen: Mutex<Vec<Seen>>,
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
        Some(Hasher::new("blaze v2 item-hash classify test").part(format!("{def:?}").as_bytes()).finish())
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

    fn value(&self, db: &Db, fact: FactRef) -> Option<ContentId> {
        values::value(db, fact)
    }

    fn def_key<'db>(&self, db: &'db Db, body: DefWithBodyId, def: SolverDefId<'db>) -> ContentId {
        crate::keys::def_key(db, body, def)
    }

    fn def_of<'db>(&self, db: &'db Db, body: DefWithBodyId, key: ContentId) -> Option<SolverDefId<'db>> {
        crate::keys::def_of(db, body, key)
    }
}

/// A store in memory.
#[derive(Default)]
struct Memory {
    images: Mutex<HashMap<ContentId, Arc<Aligned>>>,
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
    let db = db_of(STEPS);
    let probe = Arc::new(Probe { seen: Mutex::default() });
    let installed =
        body_store::install(&db, Arc::new(Memory::default()), probe.clone(), ContentId([7; 32]), Mode::Serve)
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
    let named: Vec<&Seen> =
        seen.iter().filter(|it| RULES.iter().any(|&(name, _, _)| reads::cmp_listed(name, it.name).is_eq())).collect();
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
            "const_eval_static_query" | "const_eval_discriminant_variant" => Some(blaze_items::kinds::CONST_VALUE),
            _ => None,
        };
        if let Some(kind) = want {
            let Classified::Fact(fact) = it.class else { panic!("{}: {:?}, not a fact", it.name, it.class) };
            assert_eq!(fact.kind, kind, "{}", it.name);
            assert!(values::value(&db, fact).is_some(), "{}: {fact:?} has a value", it.name);
        }
        if it.name == "fact_value" {
            assert!(matches!(it.class, Classified::Fact(_)), "fact_value: {:?}", it.class);
        }
    }
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
                assert_eq!(Some(fact), values::step_fact(&db, it.def, earlier), "{}", it.name);
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
    let db = db_of(&format!("{PRELUDE}{}", "pub struct S { pub a: u32 } pub trait T { fn m(&self); }"));
    let facts = [
        values::fields(&db, named(&db, "S")),
        values::fields(&db, named(&db, "T")),
        values::crate_root(&db, local_crate(&db)),
        attrs_of_item_or_adt(&db, "S"),
    ];
    for &fact in &facts {
        assert!(values::value(&db, fact).is_some(), "{fact:?} has a value");
    }
    assert_at_most(0, "hashing this crate's facts again", || {
        for &fact in &facts {
            let _ = values::value(&db, fact);
        }
    });
}

fn attrs_of_item_or_adt(db: &Db, name: &str) -> FactRef {
    let ItemId::Adt(adt) = named(db, name) else { panic!("`{name}` is no ADT") };
    values::attrs(db, adt.into()).expect("an ADT's attrs have a key")
}
