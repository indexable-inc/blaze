//! The goal store against a store in memory (docs/v2/plan/lanes/frontend/fast-trait-store.md,
//! "Tests"): a goal's stable form decodes back to the goal, its records read back as written,
//! an answer is found while its facts hold and refused when one or the solver changed, a goal
//! with a read that is no fact is never stored, and a lookup with no store allocates nothing.

use std::{collections::BTreeSet, marker::PhantomData, path::Path, sync::Arc};

use base_db::{Crate, Db, Env, all_crates};
use blaze_alloc_count::assert_at_most;
use blaze_format::Aligned;
use blaze_hash::HashMap;
use blaze_ids::ContentId;
use blaze_query::QueryKind;
use blaze_query::db::Setter;
use hir_def::{
    FunctionId, GenericDefId, ModuleDefId,
    deps::{ImageBuf, ImageStore},
    nameres::crate_def_map,
    signatures::FunctionSignature,
};
use hir_expand::fixture;
use parking_lot::Mutex;
use rustc_type_ir::inherent::{IntoKind as _, Ty as _};

use super::{
    Answer, FactObject, FactRef, Flushed, GoalFacts, SOLVER_FILES, install, lookup, lookup_goal,
    offer, offer_goal,
    record::{self, Shape, ShapeRef},
    solver_version,
    stable::{self, Goal},
};
use crate::{
    InferenceResult,
    db::HirDatabase,
    next_solver::{
        AdtDef, Binder, ClauseKind, Clauses, DbInterner, ParamEnv, Pattern, PatternKind, Predicate,
        PredicateKind, SolverDefId, Ty, TyKind, TypingMode,
        global_goals::GoalKey,
        persist::{self, Code, Node, Sink, Source},
    },
};

const FIXTURE: &str = r#"
//- minicore: sized, copy, clone, fn, phantom_data, future, async_fn, iterator, option
#![feature(extern_types)]
use core::marker::PhantomData;

pub trait Show { fn show(&self) -> u8; }
pub trait Assoc { type Out; }
pub struct Wrap<T>(T);
pub enum Either<A, B> { Left(A), Right(B) }
pub union Bits { int: u32, float: f32 }
extern "C" { pub type Opaque; }

impl Show for u8 { fn show(&self) -> u8 { *self } }
impl<T: Show> Show for Wrap<T> { fn show(&self) -> u8 { self.0.show() } }
impl Assoc for u8 { type Out = u16; }

fn diverge() -> ! { loop {} }
fn make() -> impl Show { 1u8 }

pub fn generic<T, U>(t: T, u: U) -> Wrap<T>
where
    T: Show + Assoc,
    U: Assoc<Out = T::Out> + Clone,
{
    let _ = u.clone();
    let out: Option<<T as Assoc>::Out> = None;
    let _ = out;
    Wrap(t)
}

pub fn every(
    x: &mut u32,
    p: *const u8,
    s: &str,
    d: &dyn Show,
    f: fn(&u8) -> u8,
    a: [u16; 3],
    sl: &[i64],
    e: Either<char, bool>,
    b: Bits,
    o: &Opaque,
    w: Wrap<f64>,
) {
    let t = (1u8, 2i32, 3.0f32, 4u128, 5isize);
    let c = |y: u32| y + *x;
    let _ = c(1);
    let g = generic::<u8, u8>;
    let ph = PhantomData::<u8>;
    let m = make();
    let fut = async { 1u8 };
    let ac = async |z: u8| z;
    let _ = (t, g, ph, m, fut, ac, p, s, d, f, a, sl, e, b, o, w);
    if false { diverge() }
}
"#;

const BUILD: ContentId = ContentId([7; 32]);
const OTHER_BUILD: ContentId = ContentId([8; 32]);
const CRATE: ContentId = ContentId([5; 32]);

/// The one fact a test's offered goal reads: [`Knob::value`].
const KNOB: FactRef = FactRef { kind: QueryKind(0x1000), subject: ContentId([3; 32]), object: FactObject::None };

/// A stand-in for what a goal's evaluation reads: `value` is its fact; `holds`, the answer, is
/// read outside the capture, as a read the facts would miss.
#[blaze_query::input]
struct Knob {
    #[returns(copy)]
    value: u32,
    #[returns(copy)]
    holds: bool,
    #[returns(copy)]
    krate: Crate,
}

fn db_of(fixture: &str) -> Db {
    let db = fixture::db(fixture).into_db();
    db
}

fn local_crate(db: &Db) -> Crate {
    *all_crates(db).iter().find(|krate| krate.data(db).origin.is_local()).expect("a local crate")
}

fn functions(db: &Db, krate: Crate) -> HashMap<String, FunctionId> {
    let mut found = HashMap::default();
    for (_, module) in crate_def_map(db, krate).modules(db) {
        for def in module.scope(db).declarations() {
            if let ModuleDefId::FunctionId(id) = def {
                found.insert(FunctionSignature::of(db, id).name.as_str().to_owned(), id);
            }
        }
    }
    found
}

/// A store in memory, which refuses a second put of a key with other bytes as the store does.
#[derive(Default)]
struct Memory {
    images: Mutex<HashMap<ContentId, Arc<Aligned>>>,
}

impl Memory {
    fn len(&self) -> usize {
        self.images.lock().len()
    }
}

impl ImageStore for Memory {
    fn get(&self, key: &ContentId) -> Result<Option<ImageBuf>, String> {
        Ok(self.images.lock().get(key).map(|it| it.clone() as ImageBuf))
    }

    fn put(&self, key: ContentId, value: Vec<u8>) -> Result<(), String> {
        let mut images = self.images.lock();
        match images.get(&key) {
            Some(kept) if kept[..] != value[..] => Err(format!("{key:?} kept with other bytes")),
            Some(_) => Ok(()),
            None => {
                images.insert(key, Arc::new(Aligned::copy_from(&value)));
                Ok(())
            }
        }
    }

    fn reads_now(&self, _: &Env, _: &[u8]) -> Result<ContentId, String> {
        Err(String::from("a goal store reads no expansion"))
    }
}

/// Lane items' part, for tests: a definition's key is the hash of `persist`'s words of it, kept
/// so [`Back`] can read a stable form back; every read but an interned one is [`KNOB`] (or none,
/// when not `expressible`).
struct TestFacts {
    defs: Mutex<HashMap<ContentId, Vec<u32>>>,
    knob: Knob,
    expressible: bool,
    names: Mutex<Vec<&'static str>>,
}

impl TestFacts {
    fn new(knob: Knob, expressible: bool) -> TestFacts {
        TestFacts { defs: Mutex::default(), knob, expressible, names: Mutex::default() }
    }
}

impl GoalFacts for TestFacts {
    fn def_key<'db>(&self, _: &'db Db, def: SolverDefId<'db>) -> ContentId {
        let mut words = RawWords(Vec::new());
        persist::put_def(&mut words, def);
        let bytes: Vec<u8> = words.0.iter().flat_map(|word| word.to_le_bytes()).collect();
        let key = ContentId::of("blaze v2 goal test def", &bytes);
        self.defs.lock().insert(key, words.0);
        key
    }

    fn crate_key(&self, _: &Db, _: Option<Crate>) -> ContentId {
        CRATE
    }

    fn fact_of(&self, _: &Db, _: blaze_query::db::DatabaseKeyIndex, name: &'static str) -> Option<FactRef> {
        self.names.lock().push(name);
        self.expressible.then_some(KNOB)
    }

    fn value(&self, db: &Db, fact: FactRef) -> Option<ContentId> {
        (fact == KNOB).then(|| ContentId::of("blaze v2 goal test knob", &self.knob.value(db).to_le_bytes()))
    }
}

/// `persist`'s words of a definition.
struct RawWords(Vec<u32>);

impl<'db> Sink<'db> for RawWords {
    fn word(&mut self, word: u32) {
        self.0.push(word);
    }

    fn node<N: Node<'db>>(&mut self, _: N) {
        panic!("a definition's words have no children");
    }

    fn adt(&mut self, _: AdtDef) {
        panic!("a definition's words have no ADT");
    }
}

struct RawSource<'a> {
    words: &'a [u32],
}

impl<'db> Source<'db> for RawSource<'_> {
    type Error = &'static str;

    fn word(&mut self) -> Result<u32, &'static str> {
        let (&word, rest) = self.words.split_first().ok_or("a definition cut short")?;
        self.words = rest;
        Ok(word)
    }

    fn node<N: Node<'db>>(&mut self) -> Result<N, &'static str> {
        Err("a child in a definition")
    }

    fn adt(&mut self) -> Result<AdtDef, &'static str> {
        Err("an ADT in a definition")
    }

    fn count(&mut self) -> Result<usize, &'static str> {
        Err("a count in a definition")
    }

    fn invalid(&self, what: &'static str) -> &'static str {
        what
    }
}

/// A stable form, read back: children inline, definitions by the keys [`TestFacts`] made.
struct Back<'a, 'db> {
    words: &'a [u32],
    defs: &'a HashMap<ContentId, Vec<u32>>,
    db: PhantomData<&'db ()>,
}

impl<'db> Source<'db> for Back<'_, 'db> {
    type Error = &'static str;

    fn word(&mut self) -> Result<u32, &'static str> {
        let (&word, rest) = self.words.split_first().ok_or("cut short")?;
        self.words = rest;
        Ok(word)
    }

    fn node<N: Node<'db>>(&mut self) -> Result<N, &'static str> {
        N::get_node(self)
    }

    fn adt(&mut self) -> Result<AdtDef, &'static str> {
        let SolverDefId::AdtId(id) = self.def()? else { return Err("an ADT of another definition") };
        let bits = u8::try_from(self.word()?).map_err(|_| "ADT flags past a byte")?;
        AdtDef::from_parts(id, bits).ok_or("unknown ADT flags")
    }

    fn count(&mut self) -> Result<usize, &'static str> {
        let count = self.word()? as usize;
        if count > self.words.len() {
            return Err("a list longer than its words");
        }
        Ok(count)
    }

    fn invalid(&self, what: &'static str) -> &'static str {
        what
    }

    fn def(&mut self) -> Result<SolverDefId<'db>, &'static str> {
        let variant = self.word()?;
        let mut key = [0; 32];
        for at in 0..8 {
            key[4 * at..4 * at + 4].copy_from_slice(&self.word()?.to_le_bytes());
        }
        let words = self.defs.get(&ContentId(key)).ok_or("a definition no key names")?;
        if words.first() != Some(&variant) {
            return Err("a definition of another variant than its key's");
        }
        persist::get_def(&mut RawSource { words })
    }
}

/// The head words, predicate and clauses of the stable form `words`.
fn decode<'db>(
    words: &[u32],
    defs: &HashMap<ContentId, Vec<u32>>,
) -> Result<([u32; 3], Predicate<'db>, Clauses<'db>), &'static str> {
    let mut back = Back { words, defs, db: PhantomData };
    let head = [back.word()?, back.word()?, back.word()?];
    let predicate = Predicate::get(&mut back)?;
    let clauses = Clauses::get(&mut back)?;
    if !back.words.is_empty() {
        return Err("words past the goal");
    }
    Ok((head, predicate, clauses))
}

fn kind_name(ty: Ty<'_>) -> &'static str {
    match ty.kind() {
        TyKind::Bool => "Bool",
        TyKind::Char => "Char",
        TyKind::Int(_) => "Int",
        TyKind::Uint(_) => "Uint",
        TyKind::Float(_) => "Float",
        TyKind::Adt(..) => "Adt",
        TyKind::Foreign(_) => "Foreign",
        TyKind::Str => "Str",
        TyKind::Array(..) => "Array",
        TyKind::Pat(..) => "Pat",
        TyKind::Slice(_) => "Slice",
        TyKind::RawPtr(..) => "RawPtr",
        TyKind::Ref(..) => "Ref",
        TyKind::FnDef(..) => "FnDef",
        TyKind::FnPtr(..) => "FnPtr",
        TyKind::UnsafeBinder(_) => "UnsafeBinder",
        TyKind::Dynamic(..) => "Dynamic",
        TyKind::Closure(..) => "Closure",
        TyKind::CoroutineClosure(..) => "CoroutineClosure",
        TyKind::Coroutine(..) => "Coroutine",
        TyKind::CoroutineWitness(..) => "CoroutineWitness",
        TyKind::Never => "Never",
        TyKind::Tuple(_) => "Tuple",
        TyKind::Alias(..) => "Alias",
        TyKind::Param(_) => "Param",
        TyKind::Bound(..) => "Bound",
        TyKind::Placeholder(_) => "Placeholder",
        TyKind::Infer(_) => "Infer",
        TyKind::Error(_) => "Error",
    }
}

/// Every kind of type a stored goal can hold: all but bound, placeholder, inference and error
/// types, which make a goal not global (`global_goals::NOT_GLOBAL`), and coroutine witnesses,
/// which only a coroutine's own goals name.
const KINDS: [&str; 24] = [
    "Bool", "Char", "Int", "Uint", "Float", "Adt", "Foreign", "Str", "Array", "Pat", "Slice",
    "RawPtr", "Ref", "FnDef", "FnPtr", "UnsafeBinder", "Dynamic", "Closure", "CoroutineClosure",
    "Coroutine", "Never", "Tuple", "Alias", "Param",
];

/// Every type of the fixture's two bodies, two that no body has, and one long enough that its
/// stable form spans many of the hash's buffers.
fn fixture_types<'db>(db: &'db Db, interner: DbInterner<'db>, functions: &HashMap<String, FunctionId>) -> Vec<Ty<'db>> {
    let mut tys = Vec::new();
    for name in ["every", "generic"] {
        let result = InferenceResult::of(db, functions[name]);
        tys.extend(result.expression_types().map(|(_, ty)| ty));
        tys.extend(result.pattern_types().map(|(_, ty)| ty));
    }
    let u8_ = interner.default_types().types.u8;
    tys.push(Ty::new(interner, TyKind::Pat(u8_, Pattern::new(interner, PatternKind::NotNull))));
    tys.push(Ty::new(interner, TyKind::UnsafeBinder(Binder::dummy(u8_).into())));
    tys.push(Ty::new_tup(interner, &[u8_; 700]));
    tys
}

fn well_formed<'db>(interner: DbInterner<'db>, ty: Ty<'db>) -> Predicate<'db> {
    Predicate::new(interner, Binder::dummy(PredicateKind::Clause(ClauseKind::WellFormed(ty.into()))))
}

/// The goal the store tests offer and look up.
fn the_goal<'db>(interner: DbInterner<'db>) -> Goal<'db> {
    let types = &interner.default_types().types;
    let ty = Ty::new_tup(interner, &[types.u8, types.u16]);
    Goal { predicate: well_formed(interner, ty), clauses: ParamEnv::empty().clauses(), mode: 5 }
}

#[test]
fn a_goal_reads_back_from_its_stable_form_and_its_record() {
    let db = db_of(FIXTURE);
    let krate = local_crate(&db);
    let interner = DbInterner::new_with(&db, krate);
    let facts = TestFacts::new(Knob::new(&db, 1, true, krate), true);
    let functions = functions(&db, krate);
    let tys = fixture_types(&db, interner, &functions);

    let mut kinds = BTreeSet::new();
    for &ty in &tys {
        for arg in ty.walk() {
            if let Some(ty) = arg.ty() {
                kinds.insert(kind_name(ty));
            }
        }
    }
    let missing: Vec<_> = KINDS.iter().filter(|kind| !kinds.contains(*kind)).collect();
    assert!(missing.is_empty(), "the fixture has no type of kind {missing:?}");

    let env = db.trait_environment(GenericDefId::FunctionId(functions["generic"])).clauses();
    assert!(env.iter().count() >= 4, "the where clauses of `generic` are in its param env");
    let mut goals = Vec::new();
    for (n, &ty) in tys.iter().enumerate() {
        let clauses = if n % 2 == 0 { env } else { Clauses::empty() };
        goals.push(Goal { predicate: well_formed(interner, ty), clauses, mode: (n % 7) as u8 });
    }
    for clause in env {
        goals.push(Goal { predicate: clause.as_predicate(), clauses: env, mode: 1 });
    }

    let read = [
        KNOB,
        FactRef { kind: QueryKind(0x1001), subject: ContentId([9; 32]), object: FactObject::Field(7) },
        FactRef { kind: QueryKind(0x1002), subject: ContentId([10; 32]), object: FactObject::Id(ContentId([1; 32])) },
    ];
    let version = solver_version(&BUILD);
    let memory = Memory::default();
    let limit = rustc_type_ir::Interner::recursion_limit(interner) as u32;
    let mut keys = HashMap::default();
    let mut longest = 0;
    for (n, &goal) in goals.iter().enumerate() {
        let mut words = Vec::new();
        assert!(stable::goal_words(interner, &facts, goal, &mut words), "goal {n} is short enough");
        longest = longest.max(words.len());
        let defs = facts.defs.lock().clone();
        let (head, predicate, clauses) = decode(&words, &defs).unwrap_or_else(|why| panic!("goal {n}: {why}"));
        assert_eq!(head, [u32::from(goal.mode), u32::from(interner.regions()), limit], "goal {n}");
        assert_eq!((predicate, clauses), (goal.predicate, goal.clauses), "goal {n}");

        let key = stable::goal_key(interner, &facts, &version, &CRATE, goal).expect("short enough");
        assert_eq!(key, stable::key_of(&version, &CRATE, &words), "goal {n}: streamed and buffered keys");
        if let Some(earlier) = keys.insert(key, goal) {
            assert_eq!(earlier, goal, "two goals under one key");
        }

        let mut shape = Vec::new();
        record::write_shape(&mut shape, &key, &version, &words, &read);
        assert_eq!(shape.len(), record::shape_len(words.len(), read.len()));
        let at = record::shape_key(&key, 0);
        memory.put(at, shape).unwrap();
        let kept = memory.get(&at).unwrap().expect("just put");
        let opened = ShapeRef::open(kept.bytes(), &key, &version).unwrap_or_else(|why| panic!("goal {n}: {why}"));
        assert_eq!(opened.to_shape(), Shape { words: words.clone(), facts: read.to_vec() }, "goal {n}");
        let (_, predicate, clauses) = decode(&opened.to_shape().words, &defs).unwrap();
        assert_eq!((predicate, clauses), (goal.predicate, goal.clauses), "goal {n}, from its record");
        assert!(ShapeRef::open(kept.bytes(), &key, &solver_version(&OTHER_BUILD)).is_err());
        assert!(ShapeRef::open(kept.bytes(), &ContentId([0; 32]), &version).is_err());
        assert!(ShapeRef::open(&kept.bytes()[..kept.bytes().len() - 1], &key, &version).is_err());
    }
    assert!(longest > 256 * 4, "a stable form spans the hash's buffer several times");

    for answer in [Answer::Holds, Answer::Fails] {
        let key = ContentId([11; 32]);
        let bytes = record::answer_record(&key, answer);
        assert_eq!(record::open_answer(&bytes, &key), Ok(answer));
        assert!(record::open_answer(&bytes, &ContentId([12; 32])).is_err());
    }
}

/// Offers [`the_goal`] with the reads of reading the knob's value, answered by `holds`.
#[blaze_query::tracked(returns(copy))]
fn offered(db: &Db, knob: Knob) -> u32 {
    let interner = DbInterner::new_with(db, knob.krate(db));
    let answer = if knob.holds(db) { Answer::Holds } else { Answer::Fails };
    let (value, reads) = blaze_query::db::capture_reads(db, || knob.value(db));
    offer_goal(interner, the_goal(interner), answer, reads.expect("an input's read is shareable"));
    value
}

fn looked_up(db: &Db, krate: Crate) -> Option<Answer> {
    let interner = DbInterner::new_with(db, krate);
    lookup_goal(interner, the_goal(interner)).map(|found| found.answer)
}

#[test]
fn an_answer_is_found_while_its_facts_hash_the_same_and_its_solver_matches() {
    let mut db = db_of(FIXTURE);
    let krate = local_crate(&db);
    let knob = Knob::new(&db, 1, true, krate);
    let memory = Arc::new(Memory::default());
    let facts = Arc::new(TestFacts::new(knob, true));
    let installed = install(&db, memory.clone(), facts.clone(), BUILD).unwrap();
    assert_eq!(looked_up(&db, krate), None, "nothing stored yet");

    assert_eq!(offered(&db, knob), 1);
    assert_eq!(memory.len(), 0, "an offer writes nothing until the flush");
    let flushed = installed.flush().unwrap();
    assert_eq!(flushed, Flushed { written: 1, ..Flushed::default() });
    assert_eq!(memory.len(), 2, "a shape and an answer");
    assert_eq!(looked_up(&db, krate), Some(Answer::Holds));
    assert_eq!(installed.flush().unwrap(), Flushed::default(), "a flush writes each offer once");

    knob.set_value(&mut db).to(2);
    assert_eq!(looked_up(&db, krate), None, "a fact it read hashes otherwise");
    assert_eq!(offered(&db, knob), 2);
    assert_eq!(installed.flush().unwrap(), Flushed { written: 1, ..Flushed::default() });
    assert_eq!(memory.len(), 3, "the same shape, a second answer");
    assert_eq!(looked_up(&db, krate), Some(Answer::Holds));
    knob.set_value(&mut db).to(1);
    assert_eq!(looked_up(&db, krate), Some(Answer::Holds), "the first answer, by content");

    drop(installed);
    assert_eq!(looked_up(&db, krate), None, "no store installed");
    let other = install(&db, memory.clone(), facts, OTHER_BUILD).unwrap();
    assert_eq!(looked_up(&db, krate), None, "another solver version");
    assert_eq!(other.flush().unwrap(), Flushed::default());
}

#[test]
fn two_answers_under_the_same_facts_stop_the_flush() {
    let mut db = db_of(FIXTURE);
    let krate = local_crate(&db);
    let knob = Knob::new(&db, 1, true, krate);
    let memory = Arc::new(Memory::default());
    let installed = install(&db, memory.clone(), Arc::new(TestFacts::new(knob, true)), BUILD).unwrap();
    offered(&db, knob);
    installed.flush().unwrap();
    knob.set_holds(&mut db).to(false);
    offered(&db, knob);
    let why = installed.flush().expect_err("the answer depends on a read the facts miss");
    assert!(why.contains("two answers under the same fact values"), "{why}");
}

#[test]
fn a_goal_with_a_read_that_is_no_fact_is_never_stored() {
    let db = db_of(FIXTURE);
    let krate = local_crate(&db);
    let knob = Knob::new(&db, 1, true, krate);
    let memory = Arc::new(Memory::default());
    let facts = Arc::new(TestFacts::new(knob, false));
    let installed = install(&db, memory.clone(), facts.clone(), BUILD).unwrap();
    offered(&db, knob);
    let flushed = installed.flush().unwrap();
    let names = facts.names.lock().clone();
    assert_eq!(names.len(), 1, "one read asked about: {names:?}");
    assert_eq!(flushed, Flushed { inexpressible: vec![(names[0], 1)], ..Flushed::default() });
    assert_eq!(memory.len(), 0);
    assert_eq!(looked_up(&db, krate), None);
}

#[test]
fn a_database_finds_only_its_own_store() {
    let db = db_of(FIXTURE);
    let other = db_of(FIXTURE);
    let krate = local_crate(&db);
    let knob = Knob::new(&db, 1, true, krate);
    let memory = Arc::new(Memory::default());
    let installed = install(&db, memory.clone(), Arc::new(TestFacts::new(knob, true)), BUILD).unwrap();
    assert!(install(&db, memory.clone(), Arc::new(TestFacts::new(knob, true)), BUILD).is_err());
    offered(&db, knob);
    installed.flush().unwrap();
    assert_eq!(looked_up(&db, krate), Some(Answer::Holds));
    assert_eq!(looked_up(&other, local_crate(&other)), None);
}

/// Offers [`the_goal`] under [`assert_at_most`]: with no store, nothing is allocated.
#[blaze_query::tracked(returns(copy))]
fn offered_with_no_store(db: &Db, knob: Knob) -> u32 {
    let interner = DbInterner::new_with(db, knob.krate(db));
    let goal = the_goal(interner);
    let key = GoalKey::of(goal.predicate, ParamEnv::empty(), TypingMode::PostAnalysis).expect("a global goal");
    let (value, reads) = blaze_query::db::capture_reads(db, || knob.value(db));
    let reads = reads.expect("an input's read is shareable");
    offer(interner, key, Answer::Holds, reads);
    assert_at_most(0, "a goal-store offer with no store installed", || offer(interner, key, Answer::Holds, reads));
    value
}

#[test]
fn a_lookup_or_offer_with_no_store_allocates_nothing() {
    let db = db_of(FIXTURE);
    let krate = local_crate(&db);
    let interner = DbInterner::new_with(&db, krate);
    let goal = the_goal(interner);
    let key = GoalKey::of(goal.predicate, ParamEnv::empty(), TypingMode::PostAnalysis).expect("a global goal");
    assert_eq!(super::stable::Goal::of(key), goal);
    // Another test's store may be installed: the first lookup registers this thread with
    // `blaze_reclaim`, which allocates once.
    assert!(lookup(interner, key).is_none());
    assert!(assert_at_most(0, "a goal-store lookup with no store installed", || lookup(interner, key)).is_none());
    offered_with_no_store(&db, Knob::new(&db, 1, true, krate));
}

#[test]
fn the_solver_version_covers_every_patch() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut on_disk = BTreeSet::from([String::from("harness/toolchain")]);
    for dir in ["vendor/rustc_next_trait_solver", "vendor/rustc_type_ir"] {
        on_disk.insert(format!("{dir}/PATCHES.md"));
        for entry in std::fs::read_dir(root.join(dir).join("patches")).unwrap() {
            let name = entry.unwrap().file_name().into_string().unwrap();
            on_disk.insert(format!("{dir}/patches/{name}"));
        }
    }
    let listed: BTreeSet<String> = SOLVER_FILES.iter().map(|(name, _)| String::from(*name)).collect();
    assert_eq!(listed, on_disk, "`SOLVER_FILES` lists the toolchain and every solver patch");
    for (name, bytes) in SOLVER_FILES {
        assert_eq!(std::fs::read(root.join(name)).unwrap(), bytes, "{name} as embedded");
    }
    assert_ne!(solver_version(&BUILD), solver_version(&OTHER_BUILD));
}
