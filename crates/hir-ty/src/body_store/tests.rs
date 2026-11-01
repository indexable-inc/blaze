//! The item store against a store in memory (docs/v2/plan/design/item-hash.md, section 9): a
//! body inferred and flushed in one database is read in a fresh one, whose ids differ, with no
//! inference run; a changed body misses; an added impl misses where the body's shape lists the
//! impls and hits where it does not; a refused read is tallied by name; a borrow check verdict
//! is stored only when it passes; a corrupt record and a verify mismatch are errors naming the
//! body; and a hit allocates only what decoding the result does.

use std::sync::Arc;

use base_db::{Crate, Db, all_crates};
use blaze_alloc_count::{assert_at_most, measure};
use blaze_format::Aligned;
use blaze_hash::HashMap;
use blaze_ids::{ContentId, Hasher};
use blaze_query::QueryKind;
use blaze_query::db::Setter;
use hir_def::{
    AdtId, ConstId, DefWithBodyId, EnumId, EnumVariantId, FunctionId, ImplId, StaticId, StructId,
    TraitId, TypeAliasId, UnionId,
    deps::{ImageBuf, ImageStore, PortableIds, body_key},
    expr_store::Body,
    nameres::{crate_def_map, demand::crate_body_owners},
    signatures::FunctionSignature,
};
use hir_expand::fixture;
use hir_expand::name::Name;
use parking_lot::Mutex;

use super::{
    BodyFacts, Classified, Flushed, Lookup, Mode, Step, StepCounts, capture, infer, install,
    lookup_inference, lookup_passed, offer_passed,
    record::{self, AnswerKey, ShapeRef},
    step_value,
};
use crate::{
    InferenceResult,
    next_solver::{
        SolverDefId,
        goal_store::{FactObject, FactRef, solver_version},
    },
};

/// `main`'s bodies are the ones stored; [`PAD`] comes first in the second database, so `main`'s
/// ids there are not the first one's.
pub(super) const FIXTURE: &str = r#"
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

const ADDED_IMPL: &str = "impl Show for u16 { fn show(&self) -> u8 { 0 } }\n";

pub(super) const BUILD: ContentId = ContentId([7; 32]);

/// The fact every read of an impl table stands for: [`TestFacts::value`] hashes how many impls
/// `main` has.
const IMPLS: FactRef = FactRef { kind: QueryKind(0x1800), subject: ContentId([4; 32]), object: FactObject::None };

pub(super) fn db_of(fixture: &str) -> Db {
    let db = fixture::db(fixture).into_db();
    db
}

fn crate_named(db: &Db, name: &str) -> Crate {
    *all_crates(db)
        .iter()
        .find(|it| it.extra_data(db).display_name.as_ref().is_some_and(|it| it.to_string() == name))
        .unwrap_or_else(|| panic!("no crate `{name}`"))
}

pub(super) fn function(db: &Db, name: &str) -> DefWithBodyId {
    let main = crate_named(db, "main");
    crate_body_owners(db, main)
        .into_iter()
        .find(|def| match def {
            DefWithBodyId::FunctionId(it) => FunctionSignature::of(db, *it).name == Name::new_root(name),
            _ => false,
        })
        .unwrap_or_else(|| panic!("no function `{name}`"))
}

/// The second database: `fixture` with a crate `pad` before its own, so `main`'s ids differ
/// from the first's. Name resolution interns every crate's items as the database is made, in
/// the crate graph's order, so only items made before `main`'s can move them, not what is
/// asked of the database afterwards.
pub(super) fn second(fixture: &str) -> Db {
    let at = fixture.find("//- /").expect("a fixture with a file");
    db_of(&format!("{}{PAD}{}", &fixture[..at], &fixture[at..]))
}

/// A crate of nothing the fixtures' crates name, its functions interned before theirs.
pub(super) const PAD: &str = "//- /pad.rs crate:pad deps:core new_source_root:library
pub fn p1() {}
pub fn p2() {}
pub fn p3() {}
pub struct P;
impl P { pub fn p4(&self) {} }
";

const BODIES: [&str; 4] = ["uses_show", "plain", "seven", "closures"];

/// A store in memory, which refuses a second put of a key with other bytes as the store does.
#[derive(Default)]
pub(super) struct Memory {
    images: Mutex<HashMap<ContentId, Arc<Aligned>>>,
}

impl Memory {
    pub(super) fn len(&self) -> usize {
        self.images.lock().len()
    }

    /// Puts `bytes` under `key` whatever it held: a store that went bad.
    pub(super) fn set(&self, key: ContentId, bytes: &[u8]) {
        self.images.lock().insert(key, Arc::new(Aligned::copy_from(bytes)));
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

    fn reads_now(&self, _: &base_db::Env, _: &[u8]) -> Result<ContentId, String> {
        Err(String::from("an item store reads no expansion"))
    }
}

/// The items every test database's keys name: a key is the hash of an item's portable form in
/// `main`'s table (frontend-deps' frozen encoding), kept so another database finds it again.
pub(super) type Defs = Arc<Mutex<HashMap<ContentId, (u8, Vec<u8>)>>>;

/// Lane item-hash's part, for tests, one per database: a body's identity is its key and its HIR
/// printed (no ids, no spans); every read of an impl table is [`IMPLS`], `refuse` is refused, and
/// every other read is covered.
pub(super) struct TestFacts {
    defs: Defs,
    main: Crate,
    identities: Mutex<HashMap<DefWithBodyId, Option<ContentId>>>,
    refuse: Option<&'static str>,
    names: Mutex<Vec<&'static str>>,
}

impl TestFacts {
    fn new(db: &Db, defs: &Defs, refuse: Option<&'static str>) -> TestFacts {
        TestFacts {
            defs: defs.clone(),
            main: crate_named(db, "main"),
            identities: Mutex::default(),
            refuse,
            names: Mutex::default(),
        }
    }
}

fn is_impl_table(name: &str) -> bool {
    name.contains("Impls") || name.contains("impls") || name.contains("value_item") || name.contains("lookup_impl_method")
}

fn portable(def: SolverDefId<'_>) -> (u8, PortableIds) {
    let mut ids = PortableIds::new();
    let tag = match def {
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
        _ => panic!("a key asked of a definition that is no item"),
    };
    (tag, ids)
}

impl BodyFacts for TestFacts {
    fn identity(&self, db: &Db, def: DefWithBodyId) -> Option<ContentId> {
        if let Some(&known) = self.identities.lock().get(&def) {
            return known;
        }
        let identity = body_key(db, def).map(|key| {
            let printed = Body::of(db, def).pretty_print(db, def, span::Edition::CURRENT);
            Hasher::new("blaze v2 item store test identity").id(&key).part(printed.as_bytes()).finish()
        });
        self.identities.lock().insert(def, identity);
        identity
    }

    fn classify(
        &self,
        _: &Db,
        _: DefWithBodyId,
        _: Step,
        _: blaze_query::db::DatabaseKeyIndex,
        _: blaze_query::db::ReadKind,
        name: &'static str,
    ) -> Classified {
        self.names.lock().push(name);
        if self.refuse == Some(name) {
            return Classified::Refused;
        }
        match is_impl_table(name) {
            true => Classified::Fact(IMPLS),
            false => Classified::Covered,
        }
    }

    fn value(&self, db: &Db, fact: FactRef) -> Option<ContentId> {
        if fact != IMPLS {
            return None;
        }
        let def_map = crate_def_map(db, self.main);
        let impls: usize = def_map.modules(db).map(|(_, module)| module.scope(db).impls().len()).sum();
        Some(Hasher::new("blaze v2 item store test impls").update(&(impls as u64).to_le_bytes()).finish())
    }

    fn def_key<'db>(&self, db: &'db Db, _: DefWithBodyId, def: SolverDefId<'db>) -> ContentId {
        let (tag, ids) = portable(def);
        let bytes = ids.encode(db, self.main).expect("a test item has a portable form");
        let key = Hasher::new("blaze v2 item store test def").update(&[tag]).part(&bytes).finish();
        self.defs.lock().insert(key, (tag, bytes));
        key
    }

    fn def_of<'db>(&self, db: &'db Db, _: DefWithBodyId, key: ContentId) -> Option<SolverDefId<'db>> {
        let (tag, bytes) = self.defs.lock().get(&key)?.clone();
        let ids = PortableIds::decode(db, self.main, &bytes).ok()?;
        match tag {
            0 => ids.get::<FunctionId>(0).map(SolverDefId::FunctionId),
            1 => ids.get::<StructId>(0).map(|it| SolverDefId::AdtId(AdtId::StructId(it))),
            2 => ids.get::<UnionId>(0).map(|it| SolverDefId::AdtId(AdtId::UnionId(it))),
            3 => ids.get::<EnumId>(0).map(|it| SolverDefId::AdtId(AdtId::EnumId(it))),
            4 => ids.get::<ConstId>(0).map(SolverDefId::ConstId),
            5 => ids.get::<StaticId>(0).map(SolverDefId::StaticId),
            6 => ids.get::<TraitId>(0).map(SolverDefId::TraitId),
            7 => ids.get::<TypeAliasId>(0).map(SolverDefId::TypeAliasId),
            8 => ids.get::<ImplId>(0).map(SolverDefId::ImplId),
            9 => ids.get::<EnumVariantId>(0).map(SolverDefId::EnumVariantId),
            _ => None,
        }
    }
}

/// A database with an item store over `memory`, and its facts.
pub(super) struct Session {
    // Declared first: the store is uninstalled before the database drops.
    pub(super) installed: super::Installed,
    pub(super) facts: Arc<TestFacts>,
    pub(super) db: Db,
}

pub(super) fn session(db: Db, memory: &Arc<Memory>, defs: &Defs, refuse: Option<&'static str>, mode: Mode) -> Session {
    let facts = Arc::new(TestFacts::new(&db, defs, refuse));
    let installed = install(&db, memory.clone(), facts.clone(), BUILD, mode).unwrap();
    Session { installed, facts, db }
}

/// [`Flushed::steps`] with `counts` for `step` and none for the others.
pub(super) fn only(step: Step, counts: StepCounts) -> [StepCounts; 3] {
    let mut steps = [StepCounts::default(); 3];
    steps[step as usize] = counts;
    steps
}

/// `flushed` but for its step counts, for a test that holds those apart.
pub(super) fn without_steps(flushed: Flushed) -> Flushed {
    Flushed { steps: Default::default(), ..flushed }
}

fn ran() -> u32 {
    infer::RAN.with(|ran| ran.get())
}

fn reset_ran() {
    infer::RAN.with(|ran| ran.set(0));
}

/// `def`'s inference result in stable form: its answer's bytes, every id by its key.
fn stable(db: &Db, facts: &TestFacts, def: DefWithBodyId) -> Vec<u8> {
    let mut out = Vec::new();
    infer::encode(db, facts, def, InferenceResult::of(db, def), &mut out).expect("the fixture's bodies encode");
    out
}

/// The first database's bodies, inferred and flushed into a new store.
fn stored(mode: Mode) -> (Arc<Memory>, Defs, HashMap<&'static str, Vec<u8>>) {
    let memory = Arc::new(Memory::default());
    let defs = Defs::default();
    let first = session(db_of(FIXTURE), &memory, &defs, None, mode);
    let mut forms = HashMap::default();
    for name in BODIES {
        let def = function(&first.db, name);
        forms.insert(name, stable(&first.db, &first.facts, def));
    }
    let flushed = first.installed.flush().unwrap();
    assert_eq!(flushed.written, BODIES.len(), "each body's inference is written: {flushed:?}");
    let ran = StepCounts { ran: BODIES.len() as u64, ..StepCounts::default() };
    assert_eq!(flushed.steps, only(Step::Infer, ran), "each body missed and ran");
    assert_eq!(memory.len(), 2 * BODIES.len(), "a shape and an answer each");
    assert_eq!(first.installed.flush().unwrap(), Flushed::default(), "a flush writes each offer once");
    (memory, defs, forms)
}

/// Where `def`'s inference answer is in `memory`, from its shape in slot 0: its store key, its
/// answer key and the step's bytes.
fn answer_of(memory: &Memory, s: &Session, def: DefWithBodyId) -> (ContentId, ContentId, Vec<u8>) {
    let identity = s.facts.identity(&s.db, def).expect("a module's function has an identity");
    let base = record::base_key(&identity, Step::Infer, &infer::SCHEMA, &solver_version(&BUILD));
    let shape_key = record::shape_key(&base, 0);
    let shape = memory.get(&record::shape_at(Step::Infer, &shape_key)).unwrap().expect("a shape in slot 0");
    let shape = ShapeRef::open(shape.bytes(), &base).unwrap();
    let mut key = AnswerKey::new(&shape_key);
    for fact in shape.facts() {
        key.value(&s.facts.value(&s.db, fact).unwrap());
    }
    let answer_key = key.finish();
    let at = record::answer_at(Step::Infer, &answer_key);
    let answer = memory.get(&at).unwrap().expect("an answer under its shape's facts");
    let body = record::open_answer(answer.bytes(), &answer_key).unwrap().to_vec();
    (at, answer_key, body)
}

/// The facts the shape of `def`'s inference in slot 0 lists.
fn shape_facts(memory: &Memory, s: &Session, def: DefWithBodyId) -> Vec<FactRef> {
    let identity = s.facts.identity(&s.db, def).unwrap();
    let base = record::base_key(&identity, Step::Infer, &infer::SCHEMA, &solver_version(&BUILD));
    let shape_key = record::shape_key(&base, 0);
    let shape = memory.get(&record::shape_at(Step::Infer, &shape_key)).unwrap().expect("a shape in slot 0");
    let shape = ShapeRef::open(shape.bytes(), &base).unwrap();
    let facts: Vec<FactRef> = shape.facts().collect();
    assert_eq!(facts.len(), shape.len());
    facts
}

#[test]
fn records_read_back_as_written_and_only_under_their_key() {
    let base = ContentId([1; 32]);
    let facts = [
        FactRef { kind: QueryKind(0x1000), subject: ContentId([2; 32]), object: FactObject::None },
        FactRef { kind: QueryKind(0x1001), subject: ContentId([2; 32]), object: FactObject::Field(3) },
        FactRef { kind: QueryKind(0x1002), subject: ContentId([2; 32]), object: FactObject::Id(ContentId([5; 32])) },
    ];
    for listed in [&facts[..0], &facts[..]] {
        let mut shape = Vec::new();
        record::write_shape(&mut shape, &base, listed);
        let shape = Aligned::copy_from(&shape);
        let opened = ShapeRef::open(&shape, &base).unwrap();
        assert_eq!(opened.facts().collect::<Vec<_>>(), listed);
        assert!(ShapeRef::open(&shape, &ContentId([9; 32])).is_err(), "a shape of another base");
        for len in 0..shape.len() {
            assert!(ShapeRef::open(&Aligned::copy_from(&shape[..len]), &base).is_err(), "cut to {len}");
        }
    }
    let mut reversed = facts;
    reversed.reverse();
    let mut shape = Vec::new();
    record::write_shape(&mut shape, &base, &reversed);
    assert!(ShapeRef::open(&Aligned::copy_from(&shape), &base).is_err(), "facts out of order");

    let key = ContentId([6; 32]);
    for body in [&b""[..], b"the step's bytes"] {
        let answer = Aligned::copy_from(&record::write_answer(&key, body));
        assert_eq!(record::open_answer(&answer, &key).unwrap(), body);
        assert!(record::open_answer(&answer, &ContentId([8; 32])).is_err(), "an answer of another key");
    }
    for step in Step::ALL {
        let shape_key = record::shape_key(&base, 0);
        assert_ne!(record::shape_at(step, &shape_key), record::answer_at(step, &shape_key));
        assert_ne!(record::base_key(&base, step, &infer::SCHEMA, &BUILD), record::base_key(&base, step, &infer::SCHEMA, &key));
    }
    assert_ne!(record::shape_key(&base, 0), record::shape_key(&base, 1));
}

#[test]
fn a_stored_inference_is_read_in_a_fresh_database_without_inferring() {
    let (memory, defs, forms) = stored(Mode::Serve);
    let fresh = session(second(FIXTURE), &memory, &defs, None, Mode::Serve);
    assert_ne!(
        function(&db_of(FIXTURE), "uses_show"),
        function(&fresh.db, "uses_show"),
        "the two databases' ids differ"
    );
    reset_ran();
    for name in BODIES {
        let def = function(&fresh.db, name);
        let _ = InferenceResult::of(&fresh.db, def);
        assert_eq!(ran(), 0, "`{name}` is read, not inferred");
        assert_eq!(stable(&fresh.db, &fresh.facts, def), forms[name], "`{name}` reads back as it was stored");
    }
    let hits = only(Step::Infer, StepCounts { hits: BODIES.len() as u64, ..StepCounts::default() });
    assert_eq!(fresh.installed.steps(), hits, "this build's counts, before the flush");
    let flushed = fresh.installed.flush().unwrap();
    assert_eq!(flushed, Flushed { steps: hits, ..Flushed::default() }, "a hit offers nothing");
    assert_eq!(fresh.installed.steps(), [StepCounts::default(); 3], "a flush takes the counts");
}

#[test]
fn a_changed_body_misses_and_the_others_hit() {
    let (memory, defs, _) = stored(Mode::Serve);
    let fixture = FIXTURE.replace("{ 7 }", "{ 8 }");
    let fresh = session(second(&fixture), &memory, &defs, None, Mode::Serve);
    reset_ran();
    let _ = InferenceResult::of(&fresh.db, function(&fresh.db, "plain"));
    assert_eq!(ran(), 0, "an unchanged body hits");
    let _ = InferenceResult::of(&fresh.db, function(&fresh.db, "seven"));
    assert_eq!(ran(), 1, "the changed body is inferred");
    let flushed = fresh.installed.flush().unwrap();
    assert_eq!(flushed.written, 1, "the changed body is offered and written: {flushed:?}");
    assert_eq!(flushed.steps, only(Step::Infer, StepCounts { hits: 1, ran: 1, refused: 0 }));
}

#[test]
fn an_added_impl_misses_only_where_the_shape_lists_the_impls() {
    let (memory, defs, _) = stored(Mode::Serve);
    {
        let first = session(db_of(FIXTURE), &Arc::new(Memory::default()), &defs, None, Mode::Serve);
        let names = |def| {
            first.facts.names.lock().clear();
            let _ = InferenceResult::of(&first.db, def);
            first.facts.names.lock().clone()
        };
        let show = names(function(&first.db, "uses_show"));
        assert!(show.iter().any(|it| is_impl_table(it)), "a method call reads an impl table: {show:?}");
        let plain = names(function(&first.db, "plain"));
        assert!(!plain.iter().any(|it| is_impl_table(it)), "an empty body reads none: {plain:?}");
        assert_eq!(shape_facts(&memory, &first, function(&first.db, "uses_show")), [IMPLS]);
        assert!(shape_facts(&memory, &first, function(&first.db, "plain")).is_empty());
    }
    let fixture = format!("{FIXTURE}{ADDED_IMPL}");
    let fresh = session(second(&fixture), &memory, &defs, None, Mode::Serve);
    reset_ran();
    let _ = InferenceResult::of(&fresh.db, function(&fresh.db, "plain"));
    assert_eq!(ran(), 0, "a body whose shape lists no impls hits");
    let _ = InferenceResult::of(&fresh.db, function(&fresh.db, "uses_show"));
    assert_eq!(ran(), 1, "a body whose shape lists the impls misses");
    let flushed = fresh.installed.flush().unwrap();
    assert_eq!(flushed.written, 1, "the same shape, a second answer: {flushed:?}");
    assert_eq!(flushed.steps, only(Step::Infer, StepCounts { hits: 1, ran: 1, refused: 0 }));
    assert_eq!(memory.len(), 2 * BODIES.len() + 1);
}

#[test]
fn a_refused_read_is_tallied_by_name_and_nothing_is_stored() {
    let defs = Defs::default();
    let learn = session(db_of(FIXTURE), &Arc::new(Memory::default()), &defs, None, Mode::Serve);
    let _ = InferenceResult::of(&learn.db, function(&learn.db, "seven"));
    let read = *learn.facts.names.lock().first().expect("an inference reads something");
    drop(learn);

    let memory = Arc::new(Memory::default());
    let refusing = session(db_of(FIXTURE), &memory, &defs, Some(read), Mode::Serve);
    let _ = InferenceResult::of(&refusing.db, function(&refusing.db, "seven"));
    let flushed = refusing.installed.flush().unwrap();
    let steps = only(Step::Infer, StepCounts { hits: 0, ran: 1, refused: 1 });
    assert_eq!(flushed, Flushed { refused: vec![(read, 1)], steps, ..Flushed::default() });
    assert_eq!(memory.len(), 0);
}

#[test]
fn a_corrupt_record_is_an_error_naming_the_body() {
    let (memory, defs, forms) = stored(Mode::Serve);
    let probe = session(db_of(FIXTURE), &Arc::new(Memory::default()), &defs, None, Mode::Serve);
    let (at, answer_key, _) = answer_of(&memory, &probe, function(&probe.db, "seven"));
    drop(probe);

    for (bad, why) in [
        (record::write_answer(&answer_key, b"not an inference answer"), "a body that does not decode"),
        (record::write_answer(&ContentId([9; 32]), b""), "an answer under another key"),
    ] {
        memory.set(at, &bad);
        let fresh = session(second(FIXTURE), &memory, &defs, None, Mode::Serve);
        let seven = function(&fresh.db, "seven");
        reset_ran();
        let _ = InferenceResult::of(&fresh.db, seven);
        assert_eq!(ran(), 1, "{why}: the body is inferred, which gives the same result");
        assert_eq!(stable(&fresh.db, &fresh.facts, seven), forms["seven"], "{why}");
        let error = fresh.installed.flush().expect_err(why);
        assert!(error.contains(&format!("{seven:?}")) && error.contains("inference"), "{why}: {error}");
    }
}

#[test]
fn verify_runs_every_hit_and_a_difference_is_an_error_naming_the_body() {
    let (memory, defs, _) = stored(Mode::Serve);
    let same = session(second(FIXTURE), &memory, &defs, None, Mode::Verify);
    reset_ran();
    for name in BODIES {
        let _ = InferenceResult::of(&same.db, function(&same.db, name));
    }
    assert_eq!(ran(), BODIES.len() as u32, "every hit is inferred as well");
    let hits = only(Step::Infer, StepCounts { hits: BODIES.len() as u64, ..StepCounts::default() });
    assert_eq!(same.installed.flush().unwrap(), Flushed { steps: hits, ..Flushed::default() }, "each stored result is the one inferred");
    drop(same);

    let probe = session(db_of(FIXTURE), &Arc::new(Memory::default()), &defs, None, Mode::Serve);
    let (at, plain_key, _) = answer_of(&memory, &probe, function(&probe.db, "plain"));
    let (_, _, seven) = answer_of(&memory, &probe, function(&probe.db, "seven"));
    drop(probe);
    memory.set(at, &record::write_answer(&plain_key, &seven));
    let wrong = session(second(FIXTURE), &memory, &defs, None, Mode::Verify);
    let plain = function(&wrong.db, "plain");
    let _ = InferenceResult::of(&wrong.db, plain);
    let error = wrong.installed.flush().expect_err("a stored result that differs from inferring it");
    assert!(error.contains(&format!("{plain:?}")) && error.contains("differs"), "{error}");
}

/// The checker the stand-in's verdicts are stored for.
const CHECKER: &str = "polonius";

/// A borrow check's stand-in: `def`'s check passes when `passes` says so.
#[blaze_query::input]
struct Verdict {
    #[returns(copy)]
    def: DefWithBodyId,
    #[returns(copy)]
    passes: bool,
}

/// What `hir_ty_borrowck::borrowck` does with the store: the check, captured, and its verdict
/// offered when it passes.
#[blaze_query::tracked(returns(copy))]
fn borrow_checked(db: &Db, verdict: Verdict) -> bool {
    let (passes, reads) = capture(db, || verdict.passes(db));
    if passes {
        offer_passed(db, verdict.def(db), CHECKER, reads);
    }
    passes
}

#[test]
fn only_a_passing_borrow_check_is_stored() {
    let memory = Arc::new(Memory::default());
    let defs = Defs::default();
    let mut first = session(db_of(FIXTURE), &memory, &defs, None, Mode::Serve);
    let seven = function(&first.db, "seven");
    let verdict = Verdict::new(&first.db, seven, false);
    assert!(!borrow_checked(&first.db, verdict));
    assert_eq!(first.installed.flush().unwrap(), Flushed::default(), "a failing verdict is never offered");
    assert!(!lookup_passed(&first.db, seven, CHECKER, || panic!("not verifying")));
    assert_eq!(first.installed.steps(), only(Step::Borrowck, StepCounts { ran: 1, ..StepCounts::default() }));

    verdict.set_passes(&mut first.db).to(true);
    assert!(borrow_checked(&first.db, verdict));
    assert_eq!(first.installed.flush().unwrap().written, 1);
    drop(first);

    let fresh = session(second(FIXTURE), &memory, &defs, None, Mode::Serve);
    let seven = function(&fresh.db, "seven");
    assert!(lookup_passed(&fresh.db, seven, CHECKER, || panic!("not verifying")), "a stored pass skips the check");
    assert!(!lookup_passed(&fresh.db, seven, "nll", || panic!("not stored")), "another checker's pass is not this one's");
    assert!(!lookup_passed(&fresh.db, function(&fresh.db, "plain"), CHECKER, || panic!("not stored")));
    drop(fresh);

    let verify = session(second(FIXTURE), &memory, &defs, None, Mode::Verify);
    let seven = function(&verify.db, "seven");
    assert!(!lookup_passed(&verify.db, seven, CHECKER, || true), "under verify the check runs");
    let hit = only(Step::Borrowck, StepCounts { hits: 1, ..StepCounts::default() });
    assert_eq!(verify.installed.flush().unwrap(), Flushed { steps: hit, ..Flushed::default() });
    assert!(!lookup_passed(&verify.db, seven, CHECKER, || false));
    let error = verify.installed.flush().expect_err("a stored pass whose check fails");
    assert!(error.contains(&format!("{seven:?}")) && error.contains("borrow check"), "{error}");
}

#[test]
fn a_step_value_is_the_same_in_every_database_and_changes_with_a_type() {
    let defs = Defs::default();
    let without = db_of(FIXTURE);
    assert_eq!(step_value(&without, function(&without, "seven"), Step::Infer), None, "no store, no value");

    let first = session(db_of(FIXTURE), &Arc::new(Memory::default()), &defs, None, Mode::Serve);
    let fresh = session(second(FIXTURE), &Arc::new(Memory::default()), &defs, None, Mode::Serve);
    let value = |s: &Session, name: &str| step_value(&s.db, function(&s.db, name), Step::Infer);
    for name in BODIES {
        let here = value(&first, name);
        assert!(here.is_some(), "`{name}` has a stored form");
        assert_eq!(here, value(&fresh, name), "`{name}` has one value in every database");
    }
    assert_eq!(step_value(&first.db, function(&first.db, "seven"), Step::Borrowck), None);

    let edited = session(second(&FIXTURE.replace("{ 7 }", "{ 7u8 as u32 }")), &Arc::new(Memory::default()), &defs, None, Mode::Serve);
    assert_ne!(value(&edited, "seven"), value(&first, "seven"), "an edit that changes a type changes the value");
    assert_eq!(value(&edited, "plain"), value(&first, "plain"), "the other bodies keep theirs");
}

#[test]
fn a_database_finds_only_its_own_store() {
    let (memory, defs, _) = stored(Mode::Serve);
    let with = session(second(FIXTURE), &memory, &defs, None, Mode::Serve);
    let facts: Arc<dyn BodyFacts> = with.facts.clone();
    assert!(install(&with.db, memory.clone(), facts, BUILD, Mode::Serve).is_err(), "one store per database");
    let without = second(FIXTURE);
    assert!(matches!(lookup_inference(&without, function(&without, "seven")), Lookup::Off));
    assert!(matches!(lookup_inference(&with.db, function(&with.db, "seven")), Lookup::Hit { verify: false, .. }));
}

#[test]
fn a_hit_allocates_only_the_decoded_result_and_no_store_allocates_nothing() {
    let (memory, defs, _) = stored(Mode::Serve);
    let fresh = session(second(FIXTURE), &memory, &defs, None, Mode::Serve);
    for name in BODIES {
        let def = function(&fresh.db, name);
        // Warm: the thread's `blaze_reclaim` slot, the identity, the facts' values and the types
        // the result interns.
        assert!(matches!(lookup_inference(&fresh.db, def), Lookup::Hit { .. }));
        let (_, _, body) = answer_of(&memory, &fresh, def);
        let (decoded, decoding) = measure(|| infer::decode(&fresh.db, &*fresh.facts, def, &Aligned::copy_from(&body)));
        drop(decoded.unwrap());
        let copying = measure(|| drop(Aligned::copy_from(&body))).1;
        let (found, hit) = measure(|| lookup_inference(&fresh.db, def));
        assert!(matches!(found, Lookup::Hit { .. }));
        drop(found);
        assert!(
            hit.allocations + copying.allocations <= decoding.allocations,
            "`{name}`: a hit made {} allocations, decoding its answer {}",
            hit.allocations,
            decoding.allocations - copying.allocations
        );
    }

    let without = second(FIXTURE);
    let def = function(&without, "seven");
    assert!(matches!(lookup_inference(&without, def), Lookup::Off));
    assert!(matches!(
        assert_at_most(0, "an item store lookup with no store for the database", || lookup_inference(&without, def)),
        Lookup::Off
    ));
    assert!(!assert_at_most(0, "a borrow check lookup with no store for the database", || {
        lookup_passed(&without, def, CHECKER, || true)
    }));
}
