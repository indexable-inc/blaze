//! A constant an entry of the item store (`hir_ty::body_store::konst`; AGENTS.md
//! `granular-caching`), by counts: of N constants of one crate, an edit of one finds the other
//! N - 1 in the store, in a database whose ids differ, with none of them evaluated and no MIR
//! built for them. The control, that the key this replaced (the crate's check key in every
//! constant's key) moves for all N, is hir-ty's
//! `body_store::konst::tests::keyed_by_its_crate_every_constant_misses`, which evaluates
//! nothing. These evaluate, so they are here, where MIR builds.

use std::sync::Arc;

use base_db::{ContentId, Env, all_crates};
use blaze_core::lock::Lock;
use blaze_hash::HashMap;
use blaze_ids::Hasher;
use hir_def::{
    ConstId, DefWithBodyId, ModuleDefId,
    deps::{Held, ImageBuf, ImageStore, body_key},
    expr_store::Body,
    nameres::crate_def_map,
};
use hir_expand::fixture;
use std::alloc::Global;

use crate::{
    body_store::{BodyFacts, Classified, Installed, Mode, Step, StepCounts, install},
    db::{GeneralConstId, HirDatabase},
    next_solver::{GenericArgs, SolverDefId, goal_store::FactRef},
    test_db::TestDB,
};

/// Five constants of one crate, none reading another.
const NAMES: [&str; 5] = ["A", "B", "C", "D", "E"];

const BUILD: ContentId = ContentId([7; 32]);

/// A crate of nothing `main` names, its items interned before `main`'s: the second database's
/// ids of `main` are not the first's.
const PAD: &str = "//- /pad.rs crate:pad deps:core new_source_root:library
pub fn p1() {}
pub const P2: u8 = 0;
pub struct P;
";

fn fixture_of(b: u32, pad: bool) -> String {
    format!(
        "//- minicore: sized, copy, add\n{}//- /main.rs crate:main deps:core new_source_root:local\n\
         pub const A: u32 = 1;\npub const B: u32 = {b};\npub const C: u32 = 3 + 4;\n\
         pub const D: u8 = 9;\npub const E: bool = true;\n",
        if pad { PAD } else { "" }
    )
}

/// The bytes a store keeps, as it gave them.
struct Bytes(Vec<u8>);

impl Held for Bytes {
    fn bytes(&self) -> &[u8] {
        &self.0
    }
}

/// A store in memory, which refuses a second put of a key with other bytes as the store does.
struct Memory {
    values: Lock<HashMap<ContentId, Arc<Bytes>, Global>>,
}

impl Default for Memory {
    fn default() -> Memory {
        Memory { values: Lock::new("test.mir.consteval.items.values", Default::default()) }
    }
}

impl ImageStore for Memory {
    fn get(&self, key: &ContentId) -> Result<Option<ImageBuf>, String> {
        Ok(self.values.lock().get(key).map(|it| Arc::clone(it) as ImageBuf))
    }

    fn put(&self, key: ContentId, value: Vec<u8>) -> Result<(), String> {
        let mut values = self.values.lock();
        match values.get(&key) {
            Some(kept) if kept.0 != value => Err(format!("{key:?} is kept with other bytes")),
            Some(_) => Ok(()),
            None => {
                values.insert(key, Arc::new(Bytes(value)));
                Ok(())
            }
        }
    }

    fn reads_now(&self, _: &Env, _: &[u8]) -> Result<ContentId, String> {
        Err("these fixtures expand no proc macro".to_owned())
    }
}

/// The facts of these fixtures: a body's identity is its item's key and its body as printed
/// (no ids, no spans); every read of a constant's evaluation is covered by it, as none of the
/// fixtures' constants reads another item. Only the constant step is stored: a read of any
/// other step is refused, so no inference or MIR record is written and none of their ids is
/// asked for.
struct Facts;

impl BodyFacts for Facts {
    fn identity(&self, db: &TestDB, def: DefWithBodyId) -> Option<ContentId> {
        let key = body_key(db, def)?;
        let printed = Body::of(db, def).pretty_print(db, def, span::Edition::CURRENT);
        Some(Hasher::new("blaze v2 item store constant test identity 1").id(&key).part(printed.as_bytes()).finish())
    }

    fn classify(
        &self,
        _: &TestDB,
        _: DefWithBodyId,
        step: Step,
        _: blaze_query::db::DatabaseKeyIndex,
        _: blaze_query::db::ReadKind,
        _: &'static str,
    ) -> Classified {
        match step {
            Step::Const => Classified::Covered,
            _ => Classified::Refused,
        }
    }

    fn value(&self, _: &TestDB, _: DefWithBodyId, _: FactRef) -> Option<ContentId> {
        None
    }

    fn def_key<'db>(&self, _: &'db TestDB, _: DefWithBodyId, def: SolverDefId<'db>) -> ContentId {
        unreachable!("only the constant step is stored, and its bytes name no definition: {def:?}")
    }

    fn def_of<'db>(&self, _: &'db TestDB, _: DefWithBodyId, key: ContentId) -> Option<SolverDefId<'db>> {
        unreachable!("only the constant step is stored, and its bytes name no definition: {key:?}")
    }
}

/// A database of `fixture` with an item store over `memory`. The store is uninstalled before
/// the database drops: it is declared after it where this is called.
fn session(fixture: &str, memory: &Arc<Memory>, mode: Mode) -> (TestDB, Installed) {
    let db: TestDB = fixture::db(fixture).into_db();
    // dyn: hir-ty's `install` takes the driver's store and facts, a boundary
    let images = Arc::clone(memory) as Arc<dyn ImageStore + Send + Sync>;
    let installed = install(&db, images, Arc::new(Facts), BUILD, mode).expect("a first store of a database");
    (db, installed)
}

fn constant(db: &TestDB, name: &str) -> ConstId {
    let main = *all_crates(db)
        .iter()
        .find(|it| it.extra_data(db).display_name.as_ref().is_some_and(|it| it.to_string() == "main"))
        .expect("the fixture's crate `main`");
    for (_, module) in crate_def_map(db, main).modules(db) {
        for def in module.scope(db).declarations() {
            if let ModuleDefId::ConstId(it) = def
                && GeneralConstId::ConstId(it).name(db) == name
            {
                return it;
            }
        }
    }
    panic!("no constant `{name}`")
}

/// Each constant's memory, evaluated or read from the store in `db`.
fn values(db: &TestDB) -> Vec<Vec<u8>> {
    NAMES
        .iter()
        .map(|name| {
            let value = db
                .const_eval(constant(db, name), GenericArgs::empty(), None)
                .unwrap_or_else(|error| panic!("`{name}` evaluates: {error:?}"));
            value.memory.to_vec()
        })
        .collect()
}

/// How often `step`'s work was asked for: found in the store or run.
fn asked(steps: &[StepCounts; Step::COUNT], step: Step) -> u64 {
    steps[step as usize].hits + steps[step as usize].ran
}

/// The invariant: a constant is found by its own item and what its evaluation read, so an
/// edit of one of N leaves the other N - 1 entries found, in a database whose ids differ, with
/// none of them evaluated and nothing of their bodies asked for (no inference, no MIR); the
/// edited one is evaluated and has its new value.
#[test]
fn an_edit_of_one_constant_finds_the_others_and_runs_one() {
    let memory = Arc::new(Memory::default());
    let n = NAMES.len() as u64;
    let (first, mir_first) = {
        let (db, installed) = session(&fixture_of(2, false), &memory, Mode::Serve);
        let first = values(&db);
        assert_eq!(first[1], 2u32.to_le_bytes());
        let flushed = installed.flush().unwrap();
        let all = StepCounts { ran: n, ..StepCounts::default() };
        assert_eq!(flushed.steps[Step::Const as usize], all, "an empty store, each evaluated: {flushed:?}");
        (first, asked(&flushed.steps, Step::Mir))
    };

    let (db, installed) = session(&fixture_of(20, true), &memory, Mode::Serve);
    let now = values(&db);
    assert_eq!(now[1], 20u32.to_le_bytes(), "the edited constant has its new value");
    for at in [0, 2, 3, 4] {
        assert_eq!(now[at], first[at], "`{}` is the stored value", NAMES[at]);
    }
    let flushed = installed.flush().unwrap();
    let counts = StepCounts { hits: n - 1, ran: 1, refused: 0 };
    assert_eq!(flushed.steps[Step::Const as usize], counts, "N - 1 hit and one ran: {flushed:?}");
    // The saved work did not run: of the bodies of N constants, the edited one's alone was
    // asked for. (Where the first build asked for none through the item store, this holds
    // nothing: the message says how many it asked for.)
    let (infer, mir) = (asked(&flushed.steps, Step::Infer), asked(&flushed.steps, Step::Mir));
    assert!(
        infer <= 1 && mir <= mir_first / n,
        "a hit asks for nothing of its body: {infer} inferences and {mir} MIR bodies asked for \
         after the edit, {mir_first} MIR bodies in the first build: {flushed:?}"
    );
    assert_eq!(installed.errors(), Ok(()));
}

/// Under `Mode::Verify` a hit evaluates as well, and a stored value equal to the evaluation
/// is no error. (That a differing one is an error is `konst::stored`'s one comparison; these
/// fixtures make no entry under another body's key to plant one: not covered.)
#[test]
fn verify_evaluates_each_hit_and_equal_values_are_no_error() {
    let memory = Arc::new(Memory::default());
    let n = NAMES.len() as u64;
    let (first, bodies_first) = {
        let (db, installed) = session(&fixture_of(2, false), &memory, Mode::Serve);
        let first = values(&db);
        let flushed = installed.flush().unwrap();
        (first, (asked(&flushed.steps, Step::Infer), asked(&flushed.steps, Step::Mir)))
    };
    let (db, installed) = session(&fixture_of(2, true), &memory, Mode::Verify);
    assert_eq!(values(&db), first);
    let flushed = installed.flush().unwrap();
    assert_eq!(flushed.steps[Step::Const as usize].hits, n, "each is found: {flushed:?}");
    // Verify ran each: what the first build asked for of the bodies is asked for again,
    // where a served hit asks for none of it.
    let bodies = (asked(&flushed.steps, Step::Infer), asked(&flushed.steps, Step::Mir));
    assert_eq!(bodies, bodies_first, "each hit was evaluated as well: {flushed:?}");
    assert_eq!(installed.errors(), Ok(()), "equal values are no error");
}
