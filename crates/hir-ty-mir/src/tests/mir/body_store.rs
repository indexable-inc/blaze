//! The item store's body steps where MIR is built: hir-ty's test binary links no MIR builder
//! (it registers only the coroutine-witness `Mir` table, whose `no_build` is `unreachable!`),
//! so what of `hir_ty::body_store` asks built MIR, or infers a body whose inference evaluates
//! a constant, is tested here. Moved from `hir_ty::body_store::mir::tests` and
//! `hir_ty::body_store::tests`, each still claiming what it claimed there.

use std::{alloc::Global, sync::Arc};

use base_db::{Crate, Db, all_crates};
use blaze_ids::ContentId;
use hir_def::{
    AdtId, DefWithBodyId,
    deps::{ImageStore, PortableIds, body_key},
    nameres::demand::crate_body_owners,
    signatures::FunctionSignature,
};
use hir_expand::{fixture, name::Name};
use hir_ty::{
    body_store::{
        BodyFacts, Classified, Closure, Lowered, Mode, Step, Traced, Untraced, entries, install,
        install_trace, lowering,
    },
    next_solver::{SolverDefId, goal_store::FactRef},
};

use crate::tests::consteval::items::Memory;

const BUILD: ContentId = ContentId([7; 32]);

/// The database of `fixture`, its queries keeping read lists (what a step's trace is made
/// from).
fn db_of(#[rust_analyzer::rust_fixture] fixture: &str) -> Db {
    fixture::db_with_reads(fixture).into_db()
}

fn crate_named(db: &Db, name: &str) -> Crate {
    *all_crates(db)
        .iter()
        .find(|it| it.extra_data(db).display_name.as_ref().is_some_and(|it| it.to_string() == name))
        .unwrap_or_else(|| panic!("no crate `{name}`"))
}

/// The function `name` of the crate `main`.
fn function(db: &Db, name: &str) -> DefWithBodyId {
    crate_body_owners(db, crate_named(db, "main"))
        .into_iter()
        .find(|&def| match def {
            DefWithBodyId::FunctionId(it) => FunctionSignature::of(db, it).name == Name::new_root(name),
            _ => false,
        })
        .unwrap_or_else(|| panic!("no function `{name}`"))
}

// ---------------------------------------------------------------------------------------------
// The places of a body's MIR owners.

/// Closures where the body's inference types them and where it does not: nested, in an inline
/// `const` block (inferred inside the body), in an array length (a body of its own), an
/// `async` closure, and an `async fn`, whose body is an `async` block.
const PLACES: &str = r#"
//- minicore: sized, copy, fn, future, async_fn
//- /main.rs crate:main deps:core new_source_root:local
pub fn places() -> u32 {
    let outer = |x: u32| { let inner = move || x; inner() };
    let k = const { let f = || 3u32; 4u32 };
    let a = [0u8; { let g = || 5usize; 6 }];
    let c = async |y: u32| y;
    let _ = (a, c);
    outer(k)
}
pub async fn later(n: u32) -> u32 { let h = |m: u32| m; h(n) }
"#;

/// The invariant `entries::places` states: the places the body's lowering gives, with no
/// inference, are those of the MIR owners its inference gives, in order. `entries::run_mir`
/// checks it for every place it is asked (an error naming the body where the two lists differ,
/// or where no owner is at the place), so each lowered place is run there. The control: a
/// lowering that takes an array length's closure for the body's own (the walk with no
/// anonymous constant left out) gives a place no owner is at, and its list differs from the
/// owners' (which the runs showed equal to the lowering's).
#[test]
fn the_lowering_gives_the_places_the_inference_does() {
    let db = db_of(PLACES);
    for name in ["places", "later"] {
        let def = function(&db, name);
        let lowered: Vec<_, Global> = {
            let mut it = Vec::new_in(Global);
            it.extend(lowering(&db, def).places());
            it
        };
        for &at in &lowered {
            if let Err(error) = entries::run_mir(&db, def, at) {
                panic!("`{name}`: the lowering's places and the inference's: {error}");
            }
        }
        assert!(lowered.len() > 1, "`{name}` has closures: {lowered:?}");
        // The control: every closure taken as the body's own.
        let mut every_own: Vec<Closure, Global> = Vec::new_in(Global);
        every_own.extend(lowering(&db, def).closures.iter().map(|it| Closure { own: true, ..*it }));
        let mut every: Vec<_, Global> = Vec::new_in(Global);
        every.extend(Lowered { closures: &every_own, ..lowering(&db, def) }.places());
        if name == "places" {
            assert_ne!(every, lowered, "the array length's closure taken for the body's went unseen");
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Traces of a body task.

/// `main`'s bodies are the ones traced; [`PAD`] comes first in the second database, so
/// `main`'s ids there are not the first one's.
const FIXTURE: &str = r#"
//- minicore: sized, copy, clone, add, fn
//- /other.rs crate:other deps:core new_source_root:library
pub struct O(pub u32);
pub fn o1() -> u32 { 1 }
pub fn o2(o: O) -> u32 { o.0 + o1() }
//- /main.rs crate:main deps:core,other new_source_root:local
pub trait Show { fn show(&self) -> u8; }
pub struct Wrap(pub u8);
impl Show for Wrap { fn show(&self) -> u8 { self.0 } }
pub fn uses_show(w: Wrap) -> u8 { w.show() }
pub fn plain() {}
pub fn seven() -> u32 { 7 }
pub fn closures(n: u32) -> u32 { let f = |x: u32| x + n; f(other::o1()) }
"#;

/// A crate of nothing the fixture's crates name, its functions interned before theirs.
const PAD: &str = "//- /pad.rs crate:pad deps:core new_source_root:library
pub fn p1() {}
pub fn p2() {}
pub fn p3() {}
pub struct P;
impl P { pub fn p4(&self) {} }
";

const BODIES: [&str; 4] = ["uses_show", "plain", "seven", "closures"];

/// The second database: `fixture` with a crate `pad` before its own, so `main`'s ids differ
/// from the first's. Name resolution interns every crate's items as the database is made, in
/// the crate graph's order, so only items made before `main`'s can move them.
fn second(fixture: &str) -> Db {
    let at = fixture.find("//- /").expect("a fixture with a file");
    db_of(&format!("{}{PAD}{}", &fixture[..at], &fixture[at..]))
}

/// The key of a body's own item in its traces: no hash, so no item's portable form has it.
const OWN: ContentId = ContentId(*b"body store test: the body itself");

/// The facts of these tests, covering what a body task runs: a body's identity is its item's
/// key; every read of its lowering, inference and built MIR is covered by it, and a read of
/// any other step is refused. A definition an answer names is keyed by its portable form in
/// `main`'s table, which another database gives again; the body's own item is [`OWN`].
struct Facts {
    main: Crate,
}

/// `body`'s own item as an answer names it.
fn own<'db>(body: DefWithBodyId) -> SolverDefId<'db> {
    match body {
        DefWithBodyId::FunctionId(it) => SolverDefId::FunctionId(it),
        DefWithBodyId::ConstId(it) => SolverDefId::ConstId(it),
        DefWithBodyId::StaticId(it) => SolverDefId::StaticId(it),
        DefWithBodyId::VariantId(it) => SolverDefId::EnumVariantId(it),
    }
}

impl BodyFacts for Facts {
    fn identity(&self, db: &Db, def: DefWithBodyId) -> Option<ContentId> {
        body_key(db, def)
    }

    fn fact_of(&self, _: &Db, _: DefWithBodyId, step: Step, _: blaze_memo::Read) -> Classified {
        match step {
            Step::Lower | Step::Infer | Step::Mir => Classified::Covered,
            Step::Borrowck | Step::Const => {
                Classified::Refused("a read of a step a body task's inference and MIR do not run")
            }
        }
    }

    fn value(&self, _: &Db, _: DefWithBodyId, _: FactRef) -> Option<ContentId> {
        None
    }

    fn def_key<'db>(&self, db: &'db Db, body: DefWithBodyId, def: SolverDefId<'db>) -> ContentId {
        if def == own(body) {
            return OWN;
        }
        let mut ids = PortableIds::new();
        let tag: u8 = match def {
            SolverDefId::FunctionId(it) => (ids.index(it), 0).1,
            SolverDefId::AdtId(AdtId::StructId(it)) => (ids.index(it), 1).1,
            SolverDefId::AdtId(AdtId::UnionId(it)) => (ids.index(it), 2).1,
            SolverDefId::AdtId(AdtId::EnumId(it)) => (ids.index(it), 3).1,
            SolverDefId::ConstId(it) => (ids.index(it), 4).1,
            SolverDefId::StaticId(it) => (ids.index(it), 5).1,
            SolverDefId::TraitId(it) => (ids.index(it), 6).1,
            SolverDefId::TypeAliasId(it) => (ids.index(it), 7).1,
            SolverDefId::ImplId(it) => (ids.index(it), 8).1,
            SolverDefId::EnumVariantId(it) => (ids.index(it), 9).1,
            _ => panic!("a key asked of a definition that is no item: {def:?}"),
        };
        let bytes = ids.encode(db, self.main).expect("a test item has a portable form");
        blaze_ids::Hasher::new("blaze v2 item store test def").update(&[tag]).part(&bytes).finish()
    }

    fn def_of<'db>(&self, _: &'db Db, _: DefWithBodyId, key: ContentId) -> Option<SolverDefId<'db>> {
        unreachable!("a trace store looks nothing up: {key:?}")
    }
}

/// What a body task takes of `name`'s inference and the built MIR of each of its owners in
/// `db`, whose store is `installed`, through the entry points it calls: each step with its
/// schema and its trace.
fn body_task(
    db: &Db,
    installed: &hir_ty::body_store::Installed,
    name: &str,
) -> Vec<(Step, blaze_format::SchemaHash, Arc<Traced>), Global> {
    let def = function(db, name);
    let mut out = Vec::new_in(Global);
    let schema = entries::run_infer(db, def);
    let traced = installed.trace(def, Step::Infer, schema).expect("the inference is traced");
    out.push((Step::Infer, schema, traced));
    for place in entries::places(db, def) {
        let schema = entries::run_mir(db, def, place).unwrap();
        let traced = installed.trace(def, Step::Mir, schema).expect("the MIR is traced");
        out.push((Step::Mir, schema, traced));
    }
    out
}

/// In `Mode::Trace` two runs of the same body in two fresh databases, whose ids differ
/// ([`second`]), give byte-equal traces: the same facts and the same answers, every step kept.
/// And a trace store files nothing: its flush is an error, and a store not in `Mode::Trace`
/// gives no trace.
#[test]
fn a_trace_is_the_same_in_two_fresh_databases() {
    let (first_db, other_db) = (db_of(FIXTURE), second(FIXTURE));
    {
        // Declared after the databases: uninstalled before they drop.
        let facts = |db: &Db| Arc::new(Facts { main: crate_named(db, "main") });
        let first = install_trace(&first_db, facts(&first_db), BUILD).unwrap();
        let other = install_trace(&other_db, facts(&other_db), BUILD).unwrap();
        for name in BODIES {
            let (a, b) = (body_task(&first_db, &first, name), body_task(&other_db, &other, name));
            assert_eq!(a.len(), b.len(), "`{name}`'s owners");
            for ((step, schema, one), (_, other_schema, two)) in a.iter().zip(&b) {
                assert_eq!(schema, other_schema, "`{name}`'s {step:?} schema");
                assert!(matches!(&**one, Traced::Kept { .. }), "`{name}`'s {step:?} is kept: {one:?}");
                assert_eq!(**one, **two, "`{name}`'s {step:?} traces, facts and answers");
            }
        }
        assert!(first.flush().is_err(), "a trace store files nothing");
    }

    let db = db_of(FIXTURE);
    // dyn: `install` takes the driver's store
    let images = Arc::new(Memory::default()) as Arc<dyn ImageStore + Send + Sync>;
    let facts = Arc::new(Facts { main: crate_named(&db, "main") });
    // Declared after the database: uninstalled before it drops.
    let serve = install(&db, images, facts, BUILD, Mode::Serve).unwrap();
    let seven = function(&db, "seven");
    let schema = entries::run_infer(&db, seven);
    assert_eq!(serve.trace(seven, Step::Infer, schema), Err(Untraced::NotTracing));
}
