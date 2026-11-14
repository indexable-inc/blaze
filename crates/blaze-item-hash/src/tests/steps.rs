//! What the rows of a body's steps' read lists stand for, as the item store asks
//! ([`crate::reads::fact_of`] through `hir_ty::body_store`), over real steps: every row is
//! answered by a rule or refused by a name listed with its reason, a read of an associated
//! item's hygiene key is its container's `Fields` or refused, and no row of another crate is
//! that crate's whole `Crate` fact. These were the read classifier's tests (the item-store
//! branch's `classify.rs`, deleted by the port); on blaze-memo a step's list is the memo's
//! (`blaze_memo::record`), each row given to [`crate::Bodies`] as the store gives it.

use std::{alloc::Global, sync::Arc};

use base_db::Db;
use blaze_core::lock::Lock;
use blaze_ids::ContentId;
use blaze_memo::{KindId, Read};
use hir_def::{DefWithBodyId, HasModule, kinds as def, nameres::demand::crate_body_owners};
use hir_expand::fixture;
use hir_ty::{
    InferBodyId, InferenceResult,
    body_store::{self, BodyFacts, Classified, Mode, Step},
    db::HirDatabase,
    next_solver::{SolverDefId, goal_store::FactRef},
};

use super::facts::Memory;
use blaze_items::key;

use crate::{Bodies, identity::give_build, reads, values};

/// A database of `fixture` as a build's: the fact boundaries marked ([`reads::mark_boundaries`]),
/// read lists kept, so the item store has a list to ask about, and the build given.
fn db_of(fixture: &str) -> Db {
    let mut db =
        fixture::db_with_boundaries(fixture, reads::mark_boundaries, blaze_memo::Reads::Kept)
            .into_db();
    let mut edit = db.edit();
    give_build(&mut edit, ContentId([7; 32]));
    _ = edit.commit();
    db
}

/// One row of one step's list and what [`Bodies`] answered for it.
#[derive(Clone, Copy, Debug)]
struct Seen {
    def: DefWithBodyId,
    step: Step,
    /// The row's kind's name: a query's full path, or an input's.
    kind: &'static str,
    /// Whether the row is a call of a fact boundary (a `Read::Query` row).
    query: bool,
    class: Classified,
}

/// [`Bodies`], with every row it is asked about kept. A refused row is answered as covered, so
/// the store asks about every row of a list (it stops at the first refusal); what is kept is
/// [`Bodies`]' own answer. The records filed under those answers are never read.
struct Probe {
    seen: Lock<Vec<Seen, Global>>,
}

impl BodyFacts for Probe {
    fn identity(&self, db: &Db, def: DefWithBodyId) -> Option<ContentId> {
        Bodies.identity(db, def)
    }

    fn fact_of(&self, db: &Db, def: DefWithBodyId, step: Step, read: Read) -> Classified {
        let class = Bodies.fact_of(db, def, step, read);
        let query = matches!(read, Read::Query { .. });
        self.seen.lock().push(Seen { def, step, kind: read.kind().name(), query, class });
        match class {
            Classified::Refused(_) => Classified::Covered,
            other => other,
        }
    }

    fn value(&self, db: &Db, def: DefWithBodyId, fact: FactRef) -> Option<ContentId> {
        Bodies.value(db, def, fact)
    }

    fn def_key<'db>(&self, db: &'db Db, body: DefWithBodyId, def: SolverDefId<'db>) -> ContentId {
        Bodies.def_key(db, body, def)
    }

    fn def_of<'db>(
        &self,
        db: &'db Db,
        body: DefWithBodyId,
        key: ContentId,
    ) -> Option<SolverDefId<'db>> {
        Bodies.def_of(db, body, key)
    }
}

/// Every row of the inference, MIR and borrow check of each body of `bodies`, as the item store
/// asked about them, in a store installed for this run alone.
fn steps_seen(db: &Db, bodies: &[DefWithBodyId]) -> Vec<Seen, Global> {
    let probe = Arc::new(Probe { seen: Lock::new("test.itemhash.steps", Vec::new_in(Global)) });
    let installed = body_store::install(
        db,
        Arc::new(Memory::default()),
        probe.clone(),
        ContentId([7; 32]),
        Mode::Serve,
    )
    .expect("an item store for the test database");
    for &def in bodies {
        let _ = InferenceResult::of(db, def);
    }
    for &def in bodies {
        let _ = db.mir_body(InferBodyId::DefWithBodyId(def));
    }
    for &def in bodies {
        let _ = hir_ty_borrowck::checked(db, def);
    }
    installed.flush().expect("the test store flushes");
    drop(installed);
    std::mem::replace(&mut *probe.seen.lock(), Vec::new_in(Global))
}

/// The steps' fixture: one body for each construct the steps' lists are about.
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

/// The rows a body's steps read that no rule answers on purpose, each by the name it is
/// refused by and why: each such row keeps its step out of the store. A name lands here only
/// with that reason; a refused row not here is a gap
/// (`every_tracked_query_a_step_reads_is_named_by_a_rule`). On blaze-memo a `Read::Query` row
/// is only ever of a kind [`reads::BOUNDARIES`] or lane items' list names (the memo lists no
/// other), so what is left to name is the input rows `reads::fact_of` refuses by kind.
fn unruled() -> [(&'static str, &'static str); 2] {
    [
        (
            KindId::of(base_db::VfsText::INPUT_KIND).name(),
            "a file's text: name resolution has no boundary of its own (hir-def keeps a \
             module's scope in its crate's def map, which reads the crate's files whole), so \
             its reads flatten into the step as the files' text (reads.rs, port report, \
             decision needed)",
        ),
        (
            KindId::of(base_db::FrontSettings::INPUT_KIND).name(),
            "the front end's settings: one singleton input with one fact for every field \
             (borrowck mode, `borrowck_debug`, `check_relative_bodies`), so a row of it is \
             no one setting's `Setting` fact (values::setting); the item-store branch kept \
             one fact per field (merge report, gap)",
        ),
    ]
}

/// The invariant: every row a body's steps read is named by a read rule: as a fact, as covered
/// by the identity, or as refused with its reason in [`unruled`]; a row no rule answers fails
/// here, not in a build's notes. And each [`unruled`] entry is still read, so the list holds
/// no row that is gone. The rows enumerated are those of every step of every body of
/// [`STEPS`], as the item store asks about them. The control: the steps read a function's
/// signature through its boundary (`FunctionSignature::served` or `lowered`), so the list is
/// the boundaries' and not empty; a query split or renamed under the rules (as
/// `XSignature::kept` was into `served` and `lowered`, f25aa5be3c, which refused every body)
/// is a kind no row of the table names, which the memo flattens and `the_boundaries_are_named_once_each`
/// and `every_boundary_row_of_a_step_has_a_rule` (reads.rs) hold to the table.
#[test]
fn every_tracked_query_a_step_reads_is_named_by_a_rule() {
    let db = db_of(STEPS);
    let krate = super::perturb::body(&db, "methods").krate(&db);
    let seen = steps_seen(&db, &crate_body_owners(&db, krate));
    assert!(seen.iter().any(|it| it.step == Step::Infer), "no inference row was asked about");
    let signatures =
        [def::FunctionSignature_served::KIND_NAME, def::FunctionSignature_lowered::KIND_NAME];
    assert!(
        seen.iter().any(|it| it.query && signatures.contains(&it.kind)),
        "the steps read no signature boundary, so this enumerates too little"
    );
    let unruled = unruled();
    let mut unnamed: Vec<(Step, &str, &str), Global> = Vec::new_in(Global);
    for it in seen.iter() {
        if let Classified::Refused(name) = it.class
            && !unruled.iter().any(|&(listed, _)| listed == name)
        {
            unnamed.push((it.step, it.kind, name));
        }
    }
    unnamed.sort_unstable();
    unnamed.dedup();
    assert!(
        unnamed.is_empty(),
        "rows the steps read that no rule answers, nor `unruled` with a reason (step, kind, \
         refused as): {unnamed:#?}"
    );
    let mut gone: Vec<&str, Global> = Vec::new_in(Global);
    for &(listed, _) in unruled.iter() {
        if !seen.iter().any(|it| it.class == Classified::Refused(listed)) {
            gone.push(listed);
        }
    }
    assert!(gone.is_empty(), "`unruled` lists rows the steps no longer read: {gone:?}");
}

/// The invariant: a body's read of the hygiene key of an associated item's name
/// (`assoc_item_key`) is the `Fields` fact of the item's impl where a `macro_rules!` wrote the
/// name (its answer is no context), and is refused by its own name where a `macro` 2.0 wrote
/// it (its answer is a context, which no fact spells). The control is the refusal: a rule that
/// answered every read with the `Fields` fact would keep the `macro` body too.
///
/// The path: a method's lookup reads its items' keys inside boundary queries
/// (`trait_value_item`, `impl_value_item`), whose reads are not in the body's list;
/// normalizing `<S as Tr>::Out` reads the impl item's key in the body's own step, in the
/// solver's `fetch_eligible_assoc_item` (`hir_def::nameres::implements`). The probe answers a
/// refused row as covered, so the store asks about every row; it keeps each one's own answer.
#[test]
fn an_assoc_items_hygiene_key_is_its_containers_fields_or_refused() {
    let db = db_of(
        r#"
//- minicore: sized
#![feature(decl_macro)]
pub trait Tr { type Out; fn m(&self) -> Self::Out; }
pub struct S;
pub struct T;
macro_rules! by_rules { ($t:ty) => { impl Tr for $t { type Out = u8; fn m(&self) -> u8 { 1 } } } }
by_rules!(S);
macro by_macro($t:ty) { impl Tr for $t { type Out = u8; fn m(&self) -> u8 { 2 } } }
by_macro!(T);
pub fn rules(s: S) -> u8 { s.m() }
pub fn macros(t: T) -> u8 { t.m() }
"#,
    );
    let (rules_def, macros_def) =
        (super::perturb::body(&db, "rules"), super::perturb::body(&db, "macros"));
    let seen = steps_seen(&db, &[rules_def, macros_def]);
    let reads = |body: DefWithBodyId| {
        let mut out: Vec<Classified, Global> = Vec::new_in(Global);
        for it in seen.iter() {
            if it.def == body && it.kind == def::assoc_item_key::KIND_NAME {
                out.push(it.class);
            }
        }
        out
    };
    let rules = reads(rules_def);
    assert!(!rules.is_empty(), "`rules` read no `assoc_item_key`: the fixture misses the path");
    for class in &rules {
        assert!(
            matches!(class, Classified::Fact(fact) if fact.kind == values::kinds::FIELDS),
            "`rules`: a `macro_rules!` name's key read as {class:?}, not its impl's `Fields`"
        );
    }
    let macros = reads(macros_def);
    assert!(
        macros.iter().any(|it| matches!(it, Classified::Refused(_))),
        "`macros`: a `macro` 2.0 name's key was not refused: {macros:?}"
    );
}

/// The invariant (granular caching): what a body's steps read of another crate is a fact of
/// what they read there (a signature, fields, the impls of one trait for one type, a crate's
/// settings), never that crate's `Crate` fact, whose value is every file of the crate and of
/// the crates below it (`blaze_items::facts::crate_inputs`): with one, a private body's edit
/// anywhere below misses every body that read it (blaze-cargo's
/// `a_dependencys_function_that_names_another_type_of_the_same_shape_reaches_the_dependent`,
/// its control). The bodies are that test's three callers. A failure names each row and the
/// step it was read in: the row whose rule falls back to the whole crate.
#[test]
fn a_bodys_steps_read_no_whole_crate_fact_of_another_crate() {
    let db = db_of(
        r#"
//- minicore: sized, phantom_data
//- /main.rs crate:main deps:dep
use dep::Tr;
pub fn param() { dep::f(dep::Meters(1)) }
pub fn marker() { dep::g(core::marker::PhantomData::<dep::A>) }
pub fn ret() -> u32 { dep::h().m() }
//- /dep.rs crate:dep
pub struct Meters(pub u32);
pub struct A;
pub fn f(_x: Meters) {}
pub fn g(_x: core::marker::PhantomData<A>) {}
pub trait Tr { fn m(&self) -> u32; }
impl Tr for Meters { fn m(&self) -> u32 { 1 } }
pub fn h() -> Meters { Meters(1) }
fn private() -> u32 { 1 }
"#,
    );
    let main = super::perturb::body(&db, "param").krate(&db);
    let own = key::crate_key(&db, main);
    let bodies = crate_body_owners(&db, main);
    let seen = steps_seen(&db, &bodies);
    let mut whole: Vec<(Step, &'static str), Global> = Vec::new_in(Global);
    for it in seen.iter().filter(|it| bodies.contains(&it.def)) {
        if let Classified::Fact(fact) | Classified::Scan(fact, _) = it.class
            && fact.kind == blaze_items::kinds::CRATE
            && fact.subject != own
        {
            whole.push((it.step, it.kind));
        }
    }
    whole.sort_unstable();
    whole.dedup();
    assert!(
        whole.is_empty(),
        "rows of `main`'s bodies kept under another crate's whole `Crate` fact (step, kind): {whole:?}"
    );
}
