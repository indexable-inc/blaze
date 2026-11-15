//! What the rows of a body's steps' read lists stand for, as the item store asks
//! ([`crate::reads::fact_of`] through `hir_ty::body_store`), over real steps: every row is
//! answered by a rule or refused by a name listed with its reason, a read of an associated
//! item's hygiene key is its container's `Fields` or refused, and no row of another crate is
//! that crate's whole `Crate` fact. These were the read classifier's tests (the item-store
//! branch's `classify.rs`, deleted by the port); on blaze-memo a step's list is the memo's
//! (`blaze_memo::record`), each row given to [`crate::Bodies`] as the store gives it.

use std::{
    alloc::{Allocator, Global},
    sync::Arc,
};

use base_db::{Crate, Db};
use blaze_core::lock::Lock;
use blaze_ids::ContentId;
use blaze_memo::{Builder, KindId, Read};
use hir_def::{
    DefWithBodyId, FunctionId, HasModule, kinds as def, nameres::demand::crate_body_owners,
    signatures::FunctionSignature,
};
use hir_expand::fixture;
use hir_ty::{
    InferBodyId, InferenceResult,
    body_store::{self, BodyFacts, Classified, Mode, Step},
    db::HirDatabase,
    next_solver::{
        SolverDefId,
        goal_store::{FactObject, FactRef},
    },
};

use super::facts::Memory;
use blaze_items::{ItemId, key};

use crate::{Bodies, identity::give_build, reads, values};

/// A database of `fixture` as a build's: the fact boundaries marked ([`reads::mark_boundaries`]),
/// read lists kept, so the item store has a list to ask about, and the build given.
fn db_of(fixture: &str) -> Db {
    db_marked(fixture, reads::mark_boundaries)
}

/// [`db_of`], with the boundaries `mark` marks.
fn db_marked(fixture: &str, mark: fn(&mut Builder)) -> Db {
    let mut db = fixture::db_with_boundaries(fixture, mark, blaze_memo::Reads::Kept).into_db();
    let mut edit = db.edit();
    give_build(&mut edit, ContentId([7; 32]));
    _ = edit.commit();
    db
}

/// Every boundary [`reads::mark_boundaries`] marks but name resolution's
/// (`hir_def::nameres::boundary::KINDS`): the plant of the tests of name resolution's rows.
fn mark_without_names(builder: &mut Builder) {
    for row in reads::BOUNDARIES {
        builder.mark_boundary(row.kind);
    }
    for &name in blaze_items::goals::QUERIES {
        builder.mark_boundary(name);
    }
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
///
/// None is left: a file's text, which a step read through its crate's def map, is behind name
/// resolution's boundaries (`hir_def::nameres::boundary`), and each front-end setting is read
/// as its own field's row (`base_db::SettingField`), answered by its `Setting` fact.
fn unruled() -> [(&'static str, &'static str); 0] {
    []
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

/// A crate whose body `f` resolves names of its own crate: an import, a module's items, a
/// function of the root, a field. `g` is the other body of the file the edits touch.
const NAMES: &str = r#"
//- minicore: sized, add
mod inner {
    pub struct P { pub x: u32 }
    pub fn make() -> P { P { x: 1 } }
}
use inner::{P, make};
pub fn f() -> u32 { let p: P = make(); helper(p.x) }
pub fn helper(x: u32) -> u32 { x + 1 }
pub fn g() -> u32 { 1 }
"#;

/// [`NAMES`] with `g`'s body edited: the same file, another body's text.
const NAMES_G_EDITED: &str = r#"
//- minicore: sized, add
mod inner {
    pub struct P { pub x: u32 }
    pub fn make() -> P { P { x: 1 } }
}
use inner::{P, make};
pub fn f() -> u32 { let p: P = make(); helper(p.x) }
pub fn helper(x: u32) -> u32 { x + 1 }
pub fn g() -> u32 { 2 + 3 }
"#;

/// The kinds of a file's rows: its text, the source root it is in, and that root.
fn file_kinds() -> [&'static str; 3] {
    [
        KindId::of(base_db::VfsText::INPUT_KIND).name(),
        KindId::of(base_db::VfsFileRoot::INPUT_KIND).name(),
        KindId::of(base_db::VfsRoot::INPUT_KIND).name(),
    ]
}

/// What the store keys each step of `def`'s body by: every fact a row of its lists stands for
/// with its value now, sorted, and the names its refused rows are refused by.
fn entries<A: Allocator + Clone>(
    db: &Db,
    def: DefWithBodyId,
    alloc: A,
) -> (Vec<(Step, FactRef, Option<ContentId>), A>, Vec<&'static str, A>) {
    entries_in(db, def, &steps_seen(db, &[def]), alloc)
}

/// [`entries`] of the rows `seen` already holds: a step runs once a database, so a test that
/// also looks at the rows themselves takes them once ([`steps_seen`]) and reads both off them.
fn entries_in<A: Allocator + Clone>(
    db: &Db,
    def: DefWithBodyId,
    seen: &[Seen],
    alloc: A,
) -> (Vec<(Step, FactRef, Option<ContentId>), A>, Vec<&'static str, A>) {
    let mut facts = Vec::new_in(alloc.clone());
    let mut refused = Vec::new_in(alloc);
    for it in seen.iter().filter(|it| it.def == def) {
        match it.class {
            Classified::Covered => {}
            Classified::Fact(fact) | Classified::Scan(fact, _) => {
                facts.push((it.step, fact, Bodies.value(db, def, fact)));
            }
            Classified::Refused(name) => refused.push(name),
        }
    }
    facts.sort_unstable();
    facts.dedup();
    refused.sort_unstable();
    refused.dedup();
    (facts, refused)
}

/// The facts of `before` that `after` lists with another value, or does not list: what a
/// record keyed by `before` would miss on (or be served stale under, if the store did not
/// value them).
fn moved<A: Allocator + Clone>(
    before: &[(Step, FactRef, Option<ContentId>)],
    after: &[(Step, FactRef, Option<ContentId>)],
    alloc: A,
) -> Vec<(Step, FactRef), A> {
    let mut out = Vec::new_in(alloc);
    for &(step, fact, value) in before {
        if !after.iter().any(|&(s, f, v)| s == step && f == fact && v == value) {
            out.push((step, fact));
        }
    }
    out
}

/// The invariant (the boundary): no row of a body's steps is a file's (`VfsText`,
/// `VfsFileRoot`, `VfsRoot`): what the steps read of their crate's names is behind name
/// resolution's boundaries (`hir_def::nameres::boundary`), one row per name, scope or crate.
/// The control (the plant: the boundaries removed): the same steps in a database where name
/// resolution's kinds are not marked list the crate's files, read through its def map.
///
/// Over two bodies: [`NAMES`]' `f`, which resolves names of its crate's modules, and
/// [`BLOCK_ITEMS`]' `f`, whose block holds a `use` of another crate, a nested `struct` and a
/// nested `fn`, so its block's collection (the item tree, the dependency table, the prelude it
/// injects, the crate's blaze edition) runs in its steps' lists. Both in a crate's root file,
/// the file the crate's edition is said in.
#[test]
fn a_bodys_steps_read_no_file_row() {
    for fixture in [NAMES, BLOCK_ITEMS] {
        steps_read_no_file_row(fixture);
    }
}

/// A body `f` in its crate's root file with a block of a `use` of another crate and nested
/// items.
const BLOCK_ITEMS: &str = r#"
//- minicore: sized, add
//- /main.rs crate:main deps:dep
pub fn f() -> u32 {
    use dep::value;
    struct S { x: u32 }
    fn inner(s: S) -> u32 { s.x + value() }
    inner(S { x: 1 })
}
//- /dep.rs crate:dep
pub fn value() -> u32 { 1 }
"#;

/// [`a_bodys_steps_read_no_file_row`] of the body `f` of `fixture`, with its plant.
fn steps_read_no_file_row(fixture: &str) {
    let files = file_kinds();
    let rows = |db: &Db| {
        let def = super::perturb::body(db, "f");
        let mut kinds: Vec<(Step, &'static str), Global> = Vec::new_in(Global);
        for it in steps_seen(db, &[def]).iter() {
            if it.def == def && files.contains(&it.kind) {
                kinds.push((it.step, it.kind));
            }
        }
        kinds.sort_unstable();
        kinds.dedup();
        kinds
    };
    let read = rows(&db_of(fixture));
    assert!(read.is_empty(), "`f`'s steps read a file's rows (step, kind): {read:?}\n{fixture}");
    let planted = rows(&db_marked(fixture, mark_without_names));
    assert!(
        !planted.is_empty(),
        "with name resolution's boundaries unmarked `f`'s steps read no file: the test proves \
         nothing about them\n{fixture}"
    );
}

/// The invariant (the floor): an edit of another body's text in the same file leaves every
/// entry of this body's steps valid: the same facts, each with the same value, and nothing
/// refused, so a record kept by the first build is served to the second. The control (the
/// plant: a fact of the file's text): the edit moves the file's text, so a step keyed by any
/// fact of it would miss; `moved` catches that fact.
#[test]
fn an_edit_of_another_body_in_the_file_leaves_this_bodys_entries_valid() {
    let before_db = db_of(NAMES);
    let after_db = db_of(NAMES_G_EDITED);
    let (before, refused) = entries(&before_db, super::perturb::body(&before_db, "f"), Global);
    assert!(refused.is_empty(), "`f`'s steps are refused, so none is kept: {refused:?}");
    assert!(
        before.iter().any(|(_, fact, _)| fact.kind == blaze_items::kinds::RESOLVES),
        "`f`'s steps list no name's `Resolves` fact: its names were read some other way"
    );
    let (after, refused) = entries(&after_db, super::perturb::body(&after_db, "f"), Global);
    assert!(refused.is_empty(), "after the edit `f`'s steps are refused: {refused:?}");
    let missed = moved(&before, &after, Global);
    assert!(missed.is_empty(), "`g`'s edit moved `f`'s facts (step, fact): {missed:?}");

    // The plant: the file's text as a fact of `f`'s inference.
    let file = FactRef {
        kind: values::kinds::FIELDS,
        subject: ContentId::of("a planted fact of the file's text", b"main.rs"),
        object: FactObject::None,
    };
    let text = |fixture: &str| Some(ContentId::of("a planted file's text", fixture.as_bytes()));
    let mut planted_before = before.clone();
    planted_before.push((Step::Infer, file, text(NAMES)));
    let mut planted_after = after.clone();
    planted_after.push((Step::Infer, file, text(NAMES_G_EDITED)));
    assert_eq!(
        moved(&planted_before, &planted_after, Global),
        [(Step::Infer, file)],
        "a fact of the file's text did not move with `g`'s edit: the oracle catches nothing"
    );
}

/// A body that calls a function a glob import brings, and the same crate with an item of the
/// same name declared in the body's module, which shadows the glob.
const SHADOWED: [&str; 2] = [
    r#"
//- minicore: sized
//- /main.rs crate:main deps:dep
use dep::*;
pub fn f() -> u32 { value() }
//- /dep.rs crate:dep
pub fn value() -> u32 { 1 }
"#,
    r#"
//- minicore: sized
//- /main.rs crate:main deps:dep
use dep::*;
pub fn f() -> u32 { value() }
pub fn value() -> u32 { 2 }
//- /dep.rs crate:dep
pub fn value() -> u32 { 1 }
"#,
];

/// The function named `name` in a crate other than `not_in`.
fn function_outside(db: &Db, name: &str, not_in: Crate) -> FunctionId {
    for &krate in base_db::all_crates(db).iter().filter(|&&it| it != not_in) {
        for owner in crate_body_owners(db, krate) {
            if let DefWithBodyId::FunctionId(it) = owner
                && FunctionSignature::of(db, it).name.as_str() == name
            {
                return it;
            }
        }
    }
    panic!("no function `{name}` outside {not_in:?}");
}

/// The `Resolves` facts of `before` whose module is `module`, and the others.
fn split_by_module<A: Allocator + Clone>(
    before: &[(Step, FactRef, Option<ContentId>)],
    module: ContentId,
    alloc: A,
) -> [Vec<(Step, FactRef, Option<ContentId>), A>; 2] {
    let mut here = Vec::new_in(alloc.clone());
    let mut elsewhere = Vec::new_in(alloc);
    for &row in before {
        match row.1.subject == module {
            true => here.push(row),
            false => elsewhere.push(row),
        }
    }
    [here, elsewhere]
}

/// The invariant (the scope is in the fact): a new item that shadows a name a body resolved
/// moves the body's fact of that name in the module the item was added to (lane items'
/// `Resolves` of the module and the name), so its kept steps miss. The lookup of `value` also
/// reads the std prelude's slot of `value` (the name fills one namespace of three, so
/// `resolve_name_in_module` goes on to the prelude): that row is of another crate's scope,
/// which no item added to `main` can change, and keeps its value on purpose, the floor (rule
/// 67) being that only the edited module's rows move. The control (the plant: a fact of the
/// resolved item alone, without the scope): the function the name resolved to before,
/// `dep::value`, keeps its `Signature` value across the edit, so a step keyed by it would be
/// served stale.
#[test]
fn a_new_item_shadowing_a_resolved_name_moves_the_bodys_fact() {
    let name = FactObject::Id(key::name_hash("value").0);
    let of_name = |facts: &[(Step, FactRef, Option<ContentId>)]| {
        let mut out: Vec<(Step, FactRef, Option<ContentId>), Global> = Vec::new_in(Global);
        out.extend(
            facts
                .iter()
                .copied()
                .filter(|(_, fact, _)| fact.kind == blaze_items::kinds::RESOLVES && fact.object == name),
        );
        out
    };
    let [before_db, after_db] = SHADOWED.map(db_of);
    let before_f = super::perturb::body(&before_db, "f");
    let after_f = super::perturb::body(&after_db, "f");
    let (before, _) = entries(&before_db, before_f, Global);
    let (after, _) = entries(&after_db, after_f, Global);
    let (before, after) = (of_name(&before), of_name(&after));
    assert!(!before.is_empty(), "`f`'s steps list no `Resolves` fact of `value`: {before:?}");
    assert_eq!(
        before.iter().map(|&(step, fact, _)| (step, fact)).collect::<Vec<_, Global>>(),
        after.iter().map(|&(step, fact, _)| (step, fact)).collect::<Vec<_, Global>>(),
        "the edit changed which `Resolves` facts of `value` `f` reads, not only their values"
    );
    // The module `value` was added to, `f`'s own (the crate root of `main`); its key is the same
    // in both databases, which differ only in that module's items.
    let module = key::module_key(&before_db, before_f.module(&before_db)).0;
    assert_eq!(module, key::module_key(&after_db, after_f.module(&after_db)).0);
    let [here, elsewhere] = split_by_module(&before, module, Global);
    assert!(!here.is_empty(), "`f`'s steps list no `Resolves` of `value` in its own module: {before:?}");
    assert_eq!(
        moved(&here, &after, Global).len(),
        here.len(),
        "a `Resolves` fact of `value` in the module the shadowing item was added to kept its \
         value: {here:?} then {after:?}"
    );
    // Every other row is of the std prelude's scope, in another crate.
    let prelude = |db: &Db, f: DefWithBodyId| {
        let (prelude, _) = hir_def::nameres::crate_def_map(db, f.krate(db))
            .prelude()
            .expect("`main` has a std prelude (minicore's)");
        assert_ne!(prelude.krate(db), f.krate(db), "the prelude is another crate's");
        key::module_key(db, prelude).0
    };
    let prelude_key = prelude(&before_db, before_f);
    assert_eq!(prelude_key, prelude(&after_db, after_f));
    assert!(
        elsewhere.iter().all(|(_, fact, _)| fact.subject == prelude_key),
        "`f`'s steps list a `Resolves` of `value` in a module neither its own nor the std \
         prelude: {elsewhere:?}"
    );
    assert!(
        moved(&elsewhere, &after, Global).is_empty(),
        "an item added to `main` moved the std prelude's `Resolves` of `value`: {elsewhere:?} \
         then {after:?}"
    );

    // The plant: `dep::value`'s own `Signature`, the fact of the resolved item alone.
    let signature = |db: &Db, f: DefWithBodyId| {
        let it = function_outside(db, "value", f.krate(db));
        let fact = FactRef {
            kind: blaze_items::kinds::SIGNATURE,
            subject: key::item_key(db, ItemId::Function(it)).0,
            object: FactObject::None,
        };
        (fact, Bodies.value(db, f, fact))
    };
    let (planted_before, planted_after) =
        (signature(&before_db, before_f), signature(&after_db, after_f));
    assert!(planted_before.1.is_some(), "`dep::value` has no `Signature` value");
    assert_eq!(
        planted_before, planted_after,
        "`dep::value`'s signature moved with an item added to `main`: the plant would catch \
         the shadowing, and this test would not show the scope is needed"
    );
}

/// The plant of the block tests: [`reads::mark_boundaries`] and a block's def map marked a
/// boundary too, as it was, so its resolution's reads hide behind one row no rule answers.
fn mark_with_block_boundary(builder: &mut Builder) {
    reads::mark_boundaries(builder);
    builder.mark_boundary(hir_def::nameres::block_def_map::KIND_NAME);
}

/// The kind of a block's def map's row, which the plant alone lists.
fn block_kind() -> &'static str {
    hir_def::nameres::block_def_map::KIND_NAME
}

/// A body whose block imports a function of another crate, and the same with that function's
/// signature changed; `f`'s own text is the same in both.
const BLOCK_USE: [&str; 2] = [
    r#"
//- minicore: sized
//- /main.rs crate:main deps:dep
pub fn f() { use dep::value; let _ = value(); }
//- /dep.rs crate:dep
pub fn value() -> u32 { 1 }
"#,
    r#"
//- minicore: sized
//- /main.rs crate:main deps:dep
pub fn f() { use dep::value; let _ = value(); }
//- /dep.rs crate:dep
pub fn value() -> u64 { 1 }
"#,
];

/// `Resolves(dep's root, value)`: the lookup `f`'s block's `use dep::value;` makes.
fn resolves_dep_value(db: &Db, f: DefWithBodyId) -> FactRef {
    let dep_root = function_outside(db, "value", f.krate(db)).module(db);
    FactRef {
        kind: blaze_items::kinds::RESOLVES,
        subject: key::module_key(db, dep_root).0,
        object: FactObject::Id(key::name_hash("value").0),
    }
}

/// The invariant (a block's resolution is the body's reads): a body whose block holds
/// `use dep::value;` lists the lookup that import made, `Resolves(dep's root, value)`, as an
/// ordinary row of its steps, no row of the block's def map, and nothing refused: the block's
/// text is the body's own, covered by its key. The control (the plant: the block's def map a
/// boundary again): the steps list a row of the block's def map and no `Resolves` of `value`
/// in `dep`, the import's lookup hidden behind it.
#[test]
fn a_blocks_use_is_read_as_the_names_it_looked_up() {
    let db = db_of(BLOCK_USE[0]);
    let f = super::perturb::body(&db, "f");
    let seen = steps_seen(&db, &[f]);
    let (facts, refused) = entries_in(&db, f, &seen, Global);
    assert!(refused.is_empty(), "`f`'s steps are refused: {refused:?}");
    let want = resolves_dep_value(&db, f);
    assert!(
        facts.iter().any(|(_, fact, _)| *fact == want),
        "`f`'s steps list no `Resolves` of `value` in `dep`, the lookup its block's `use` made: \
         {facts:?}"
    );
    assert!(
        !seen.iter().any(|it| it.def == f && it.kind == block_kind()),
        "`f`'s steps list a row of its block's def map"
    );

    // The plant: the block's def map a boundary.
    let planted = db_marked(BLOCK_USE[0], mark_with_block_boundary);
    let pf = super::perturb::body(&planted, "f");
    let seen = steps_seen(&planted, &[pf]);
    assert!(
        seen.iter().any(|it| it.def == pf && it.kind == block_kind()),
        "with the block's def map a boundary `f`'s steps list no row of it: the plant is not \
         planted"
    );
    let (facts, _) = entries_in(&planted, pf, &seen, Global);
    let want = resolves_dep_value(&planted, pf);
    assert!(
        !facts.iter().any(|(_, fact, _)| *fact == want),
        "with the block's def map a boundary the import's lookup is still listed: this test \
         does not show the boundary is open"
    );
}

/// The invariant (soundness): a change of the signature of the function a block's `use`
/// imports moves a fact of the body's steps, so a record kept before misses. The control (the
/// plant: a key of the body's text alone): `f`'s own key is the same in both databases, so
/// only the rows of what its block looked up can tell the two apart.
#[test]
fn an_edit_of_what_a_blocks_use_imports_moves_the_bodys_facts() {
    let [before_db, after_db] = BLOCK_USE.map(db_of);
    let before_f = super::perturb::body(&before_db, "f");
    let after_f = super::perturb::body(&after_db, "f");
    let (before, refused) = entries(&before_db, before_f, Global);
    assert!(refused.is_empty(), "`f`'s steps are refused: {refused:?}");
    let (after, refused) = entries(&after_db, after_f, Global);
    assert!(refused.is_empty(), "after the edit `f`'s steps are refused: {refused:?}");
    let signature = FactRef {
        kind: blaze_items::kinds::SIGNATURE,
        subject: key::item_key(
            &before_db,
            ItemId::Function(function_outside(&before_db, "value", before_f.krate(&before_db))),
        )
        .0,
        object: FactObject::None,
    };
    assert!(
        before.iter().any(|(_, fact, _)| *fact == signature),
        "`f`'s steps do not list `dep::value`'s `Signature`: {before:?}"
    );
    let missed = moved(&before, &after, Global);
    assert!(
        missed.iter().any(|(_, fact)| *fact == signature),
        "`dep::value`'s signature changed and `f`'s fact of it kept its value: {missed:?}"
    );

    // The plant: `f`'s own key, a key of its text alone, does not move.
    let key_before = crate::identity::early_identity(&before_db, before_f);
    let key_after = crate::identity::early_identity(&after_db, after_f);
    assert!(key_before.is_ok(), "`f` has no key: {key_before:?}");
    assert_eq!(
        key_before, key_after,
        "`f`'s own key moved with an edit of another crate: it, not the rows, would tell the two \
         apart, and this test would show nothing of them"
    );
}

/// A body `f` with a block holding an item, a body `g` beside it, and the same with another
/// item added to `f`'s block.
const BLOCK_ITEM_ADDED: [&str; 2] = [
    r#"
//- minicore: sized
pub fn value() -> u32 { 1 }
pub fn f() -> u32 { struct Local; let _ = Local; value() }
pub fn g() -> u32 { value() }
"#,
    r#"
//- minicore: sized
pub fn value() -> u32 { 1 }
pub fn f() -> u32 { struct Local; struct Extra; let _ = Local; value() }
pub fn g() -> u32 { value() }
"#,
];

/// The invariant (the floor): an item added to one body's block moves no fact of another body:
/// `g`'s entries are the same facts with the same values, nothing refused, and its key is the
/// same. The control (the plant: the edit is real and seen): `f`'s own key moves, the block's
/// text being `f`'s, so the body that holds the block is the one whose record misses.
#[test]
fn an_item_added_to_a_block_moves_no_fact_of_another_body() {
    let [before_db, after_db] = BLOCK_ITEM_ADDED.map(db_of);
    let body = |db: &Db, name: &str| super::perturb::body(db, name);
    let (before, refused) = entries(&before_db, body(&before_db, "g"), Global);
    assert!(refused.is_empty(), "`g`'s steps are refused: {refused:?}");
    assert!(!before.is_empty(), "`g`'s steps list no fact: the test compares nothing");
    let (after, refused) = entries(&after_db, body(&after_db, "g"), Global);
    assert!(refused.is_empty(), "after the edit `g`'s steps are refused: {refused:?}");
    let missed = moved(&before, &after, Global);
    assert!(missed.is_empty(), "an item added to `f`'s block moved `g`'s facts: {missed:?}");
    let key = |db: &Db, name: &str| crate::identity::early_identity(db, body(db, name));
    assert!(key(&before_db, "g").is_ok(), "`g` has no key");
    assert_eq!(key(&before_db, "g"), key(&after_db, "g"), "`g`'s key moved with `f`'s block");

    // The plant: the block is `f`'s text, so `f`'s key sees the edit.
    assert!(key(&before_db, "f").is_ok(), "`f` has no key");
    assert_ne!(
        key(&before_db, "f"),
        key(&after_db, "f"),
        "`f`'s key did not move with an item added to its own block: the edit is not one"
    );
}

/// A body whose block declares a struct and a function that takes it, and calls the function.
const NESTED: &str = r#"
//- minicore: sized
pub fn f() -> u32 {
    struct S { x: u32 }
    fn inner(s: S) -> u32 { s.x }
    inner(S { x: 1 })
}
"#;

/// The invariant: a body with a nested `fn` and a nested `struct` in its block checks with
/// nothing refused, its block's def map read as the body's own. The control (the plant: the
/// block's def map a boundary): its row is refused by its kind's name, as no rule answers it.
#[test]
fn a_block_with_a_nested_fn_and_struct_checks_without_refusal() {
    let db = db_of(NESTED);
    let (_, refused) = entries(&db, super::perturb::body(&db, "f"), Global);
    assert!(refused.is_empty(), "`f`'s steps are refused: {refused:?}");

    let planted = db_marked(NESTED, mark_with_block_boundary);
    let (_, refused) = entries(&planted, super::perturb::body(&planted, "f"), Global);
    assert!(
        refused.contains(&block_kind()),
        "with the block's def map a boundary its row is not refused: {refused:?}"
    );
}
