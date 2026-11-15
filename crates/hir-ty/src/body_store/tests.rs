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
use blaze_kernel::Seen;
use blaze_query::QueryKind;
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
use blaze_core::lock::Lock;
use blaze_record::{AnswerKey, Digest};
use std::alloc::Global;

use super::{
    BodyFacts, Classified, Flushed, Lookup, Mode, Step, StepCounts, Traced, capture,
    entries, infer, install, install_trace, lookup_inference, lookup_passed, offer_passed,
    offer_verdict,
    record::{self, ShapeRef},
    step_value,
};
use crate::{
    InferenceResult,
    next_solver::{
        SolverDefId,
        goal_store::{FactObject, FactRef},
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
const IMPLS: FactRef =
    FactRef { kind: QueryKind(0x1800), subject: ContentId([4; 32]), object: FactObject::None };

/// The database of `fixture`, its queries keeping read lists (what [`capture`] answers from)
/// and the impl tables its fact boundaries ([`impl_table_boundaries`]), as lane item-hash's
/// install marks its own: the boundaries are the session's, fixed in its first snapshot.
pub(super) fn db_of(fixture: &str) -> Db {
    fixture::db_with_boundaries(fixture, impl_table_boundaries, blaze_memo::Reads::Kept).into_db()
}

pub(super) fn crate_named(db: &Db, name: &str) -> Crate {
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
            DefWithBodyId::FunctionId(it) => {
                FunctionSignature::of(db, *it).name == Name::new_root(name)
            }
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
pub(super) struct Memory {
    images: Lock<HashMap<ContentId, Arc<Aligned>, Global>>,
    /// The served steps' entries the kernel keeps beside the store ([`kept_by_the_kernel`]).
    entries: Lock<Vec<Kept>>,
}

impl Default for Memory {
    fn default() -> Memory {
        Memory {
            images: Lock::new("test.ty.body_store.memory", HashMap::default()),
            entries: Lock::new("test.ty.body_store.entries", Vec::new()),
        }
    }
}

/// A served step of a body of `main`, by its function's name, as its kernel entry keeps it:
/// the identity and the facts its run read, with what each was, and its answer.
pub(super) struct Kept {
    name: Name,
    step: Step,
    schema: blaze_format::SchemaHash,
    identity: ContentId,
    facts: Box<[(FactRef, Seen)]>,
    answer: Arc<[u8]>,
}

/// What the kernel's body entries keep of `first`'s runs of the served steps, put beside
/// `memory`: each run's trace the item store kept (what an entry's run reads,
/// `entries::compute`), with the identity and facts as `first` has them. [`session`] posts to
/// each new database what the kernel's lookup gives there: each entry whose identity and
/// facts are the same in it ([`served_by_the_kernel`]). The kernel itself, over a world of
/// syntax, is driven in `entries::tests`; this is its verdict for these tests' databases,
/// which have no syntax world.
pub(super) fn kept_by_the_kernel(first: &Session, memory: &Memory) {
    let db = &first.db;
    let traces: Vec<_> = super::with_store(db, |store| {
        store.traces.lock().iter().map(|(&key, ran)| (key, ran.clone())).collect()
    })
    .expect("the first database's item store");
    for ((def, step, schema), ran) in traces {
        let (DefWithBodyId::FunctionId(f), super::Traced::Kept { facts, answer, .. }) = (def, &*ran) else {
            continue;
        };
        if !step.served() {
            continue;
        }
        let Some(identity) = first.facts.identity(db, def) else { continue };
        let facts = facts
            .iter()
            .map(|&fact| (fact, first.facts.value(db, def, fact).map_or(Seen::Gone, Seen::Is)))
            .collect();
        memory.entries.lock().push(Kept {
            name: FunctionSignature::of(db, f).name.clone(),
            step,
            schema,
            identity,
            facts,
            answer: answer[..].into(),
        });
    }
}

/// Posts to `s`'s database each entry beside `memory` the kernel's lookup verifies there: its
/// body's identity and each fact it read are what they were.
fn served_by_the_kernel(s: &Session, memory: &Memory) {
    let db = &s.db;
    let main = crate_named(db, "main");
    for kept in memory.entries.lock().iter() {
        let Some(def) = crate_body_owners(db, main).into_iter().find(|def| {
            matches!(def, DefWithBodyId::FunctionId(it) if FunctionSignature::of(db, *it).name == kept.name)
        }) else {
            continue;
        };
        let same = s.facts.identity(db, def) == Some(kept.identity)
            && kept.facts.iter().all(|&(fact, seen)| s.facts.value(db, def, fact).map_or(Seen::Gone, Seen::Is) == seen);
        if !same {
            continue;
        }
        let served = super::entries::Served {
            identity: kept.identity,
            facts: kept.facts.clone(),
            answer: kept.answer.clone(),
        };
        let name = super::entries::BodyName::of(db, def).expect("a body of `main` outside a block has a name");
        super::with_store(db, |store| store.served.lock().post(name, def, kept.step, kept.schema, served))
            .expect("the database's item store");
    }
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

    // dyn: `ImageStore::pages`'s signature, once per crate read from pages
    fn pages(&self, key: &ContentId) -> Result<Option<std::sync::Arc<dyn hir_def::deps::PageSource>>, String> {
        // A test's store holds each value whole: no part of one is read alone.
        Ok(hir_def::deps::whole_pages(self.get(key)?, std::alloc::Global))
    }

    // dyn: `ImageStore::for_each`'s signature, one call per batch of crates
    fn for_each(&self, count: usize, task: &(dyn Fn(usize) + Sync)) {
        // A test's store runs a batch in order on its own thread: the same result.
        (0..count).for_each(task);
    }
}

/// The items every test database's keys name: a key is the hash of an item's portable form in
/// `main`'s table (frontend-deps' frozen encoding), kept so another database finds it again.
#[derive(Clone)]
pub(super) struct Defs(Arc<Lock<HashMap<ContentId, (u8, Vec<u8>), Global>>>);

impl Default for Defs {
    fn default() -> Defs {
        Defs(Arc::new(Lock::new("test.ty.body_store.defs", HashMap::default())))
    }
}

impl std::ops::Deref for Defs {
    type Target = Lock<HashMap<ContentId, (u8, Vec<u8>), Global>>;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

/// Lane item-hash's part, for tests, one per database: a body's identity is its key and its HIR
/// printed (no ids, no spans); every read of an impl table is [`IMPLS`], `refuse` is refused, and
/// every other read is covered. A record's own body is [`OWN`], as [`BodyFacts::def_key`] says
/// ("self"); any other item is the hash of its portable form, whose header holds the content of
/// each file it names (`Enc::finish`), so it is found again only by a database whose copy of
/// the item's file is the same text.
pub(super) struct TestFacts {
    defs: Defs,
    main: Crate,
    identities: Lock<HashMap<DefWithBodyId, Option<ContentId>, Global>>,
    refuse: Option<&'static str>,
    names: Lock<Vec<&'static str>>,
}

impl TestFacts {
    pub(super) fn new(db: &Db, defs: &Defs, refuse: Option<&'static str>) -> TestFacts {
        // The test's fact boundaries, the impl tables, were marked as the database was made
        // ([`db_of`]): a read of one is a row of its own and not its table's inputs.
        TestFacts {
            defs: defs.clone(),
            main: crate_named(db, "main"),
            identities: Lock::new("test.ty.body_store.identities", Default::default()),
            refuse,
            names: Lock::new("test.ty.body_store.names", Default::default()),
        }
    }
}

/// Marks each impl table's query kind ([`crate::method_resolution::IMPL_TABLE_KINDS`]) a fact
/// boundary (port decision 2): a call of one is a `Read::Query` row of its asker's list. A
/// database's first snapshot is given it ([`db_of`]).
pub(super) fn impl_table_boundaries(builder: &mut blaze_memo::Builder) {
    for &name in crate::method_resolution::IMPL_TABLE_KINDS {
        builder.mark_boundary(name);
    }
}

/// The invariant: every kind [`impl_table_boundaries`] marks is one [`is_impl_table`] calls an
/// impl table, so a boundary row of one is classified as the impl fact.
#[test]
fn every_impl_table_boundary_is_classified_as_one() {
    for &name in crate::method_resolution::IMPL_TABLE_KINDS {
        assert!(is_impl_table(name), "`{name}` is marked an impl table and not classified as one");
    }
}

/// The name of the kind of `read`, a query's or an input's: what the test classifiers go by.
pub(super) fn kind_name(read: blaze_memo::Read) -> &'static str {
    match read {
        blaze_memo::Read::Input { kind, .. } | blaze_memo::Read::Query { kind, .. } => kind.name(),
    }
}

pub(super) fn is_impl_table(name: &str) -> bool {
    name.contains("Impls")
        || name.contains("impls")
        || name.contains("value_item")
        || name.contains("lookup_impl_method")
}

/// The key of a body's own item in its records: no hash, so no item's portable form has it.
pub(super) const OWN: ContentId = ContentId(*b"body store test: the body itself");

/// `body`'s own item as a record names it.
pub(super) fn own<'db>(body: DefWithBodyId) -> SolverDefId<'db> {
    match body {
        DefWithBodyId::FunctionId(it) => SolverDefId::FunctionId(it),
        DefWithBodyId::ConstId(it) => SolverDefId::ConstId(it),
        DefWithBodyId::StaticId(it) => SolverDefId::StaticId(it),
        DefWithBodyId::VariantId(it) => SolverDefId::EnumVariantId(it),
    }
}

pub(super) fn portable(def: SolverDefId<'_>) -> (u8, PortableIds) {
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
            Hasher::new("blaze v2 item store test identity")
                .id(&key)
                .part(printed.as_bytes())
                .finish()
        });
        self.identities.lock().insert(def, identity);
        identity
    }

    fn fact_of(&self, _: &Db, _: DefWithBodyId, _: Step, read: blaze_memo::Read) -> Classified {
        let name = kind_name(read);
        self.names.lock().push(name);
        if self.refuse == Some(name) {
            return Classified::Refused(name);
        }
        match read {
            blaze_memo::Read::Query { .. } if is_impl_table(name) => Classified::Fact(IMPLS),
            _ => Classified::Covered,
        }
    }

    fn value(&self, db: &Db, _: DefWithBodyId, fact: FactRef) -> Option<ContentId> {
        if fact != IMPLS {
            return None;
        }
        let def_map = crate_def_map(db, self.main);
        let impls: usize =
            def_map.modules(db).map(|(_, module)| module.scope(db).impls().len()).sum();
        Some(
            Hasher::new("blaze v2 item store test impls")
                .update(&(impls as u64).to_le_bytes())
                .finish(),
        )
    }

    fn def_key<'db>(&self, db: &'db Db, body: DefWithBodyId, def: SolverDefId<'db>) -> ContentId {
        if def == own(body) {
            return OWN;
        }
        let (tag, ids) = portable(def);
        let bytes = ids.encode(db, self.main).expect("a test item has a portable form");
        let key = Hasher::new("blaze v2 item store test def").update(&[tag]).part(&bytes).finish();
        self.defs.lock().insert(key, (tag, bytes));
        key
    }

    fn def_of<'db>(
        &self,
        db: &'db Db,
        body: DefWithBodyId,
        key: ContentId,
    ) -> Option<SolverDefId<'db>> {
        if key == OWN {
            return Some(own(body));
        }
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

pub(super) fn session(
    db: Db,
    memory: &Arc<Memory>,
    defs: &Defs,
    refuse: Option<&'static str>,
    mode: Mode,
) -> Session {
    let facts = Arc::new(TestFacts::new(&db, defs, refuse));
    let installed = install(&db, memory.clone(), facts.clone(), BUILD, mode).unwrap();
    let s = Session { installed, facts, db };
    served_by_the_kernel(&s, memory);
    s
}

/// [`Flushed::steps`] with `counts` for `step` and none for the others.
pub(super) fn only(step: Step, counts: StepCounts) -> [StepCounts; Step::COUNT] {
    let mut steps = [StepCounts::default(); Step::COUNT];
    steps[step as usize] = counts;
    steps
}

/// `flushed` but for what is counted by step (its step counts and its lookups' counts), for a
/// test that holds those apart.
pub(super) fn without_steps(flushed: Flushed) -> Flushed {
    without_lookups(Flushed { steps: Default::default(), ..flushed })
}

/// `flushed` but for what its lookups came to ([`Flushed::lookups`]), for a test that states
/// the rest whole.
pub(super) fn without_lookups(flushed: Flushed) -> Flushed {
    Flushed { lookups: Default::default(), ..flushed }
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
    infer::encode(db, facts, def, InferenceResult::of(db, def), &mut out)
        .expect("the fixture's bodies encode");
    out
}

/// The first database's bodies, inferred and flushed into a new store.
fn stored(mode: Mode) -> (Arc<Memory>, Defs, HashMap<&'static str, Vec<u8>, Global>) {
    let memory = Arc::new(Memory::default());
    let defs = Defs::default();
    let first = session(db_of(FIXTURE), &memory, &defs, None, mode);
    let mut forms = HashMap::new_in(Global);
    for name in BODIES {
        let def = function(&first.db, name);
        forms.insert(name, stable(&first.db, &first.facts, def));
    }
    kept_by_the_kernel(&first, &memory);
    let flushed = first.installed.flush().unwrap();
    assert_eq!(flushed.written, 0, "a served step's record is the kernel's entry: {flushed:?}");
    let ran = StepCounts { ran: BODIES.len() as u64, ..StepCounts::default() };
    assert_eq!(flushed.steps, only(Step::Infer, ran), "each body missed and ran");
    assert_eq!(
        flushed.lookups,
        [blaze_record::Counts::default(); Step::COUNT],
        "a served step searches none of the item store's records"
    );
    assert_eq!(memory.len(), 0, "nothing is filed in the item store's records");
    assert_eq!(memory.entries.lock().len(), BODIES.len(), "an entry each");
    assert_eq!(
        first.installed.flush().unwrap(),
        Flushed::default(),
        "a flush writes each offer once"
    );
    (memory, defs, forms)
}

impl Memory {
    /// `f` of the kernel's entry of `step` of the function `name`, under `schema` where it is
    /// given (a body's MIR and its closure's are two).
    fn kept<R>(
        &self,
        name: &str,
        step: Step,
        schema: Option<blaze_format::SchemaHash>,
        f: impl FnOnce(&mut Kept) -> R,
    ) -> R {
        let mut entries = self.entries.lock();
        let kept = entries
            .iter_mut()
            .find(|it| {
                it.name == Name::new_root(name) && it.step == step && schema.is_none_or(|schema| it.schema == schema)
            })
            .unwrap_or_else(|| panic!("no entry of the {step:?} of `{name}`"));
        f(kept)
    }

    /// The answer of the kernel's entry of `step` of the function `name`.
    pub(super) fn answer(&self, name: &str, step: Step) -> Vec<u8> {
        self.kept(name, step, None, |it| it.answer.to_vec())
    }

    /// The answer of its entry under `schema`.
    pub(super) fn answer_under(&self, name: &str, step: Step, schema: blaze_format::SchemaHash) -> Vec<u8> {
        self.kept(name, step, Some(schema), |it| it.answer.to_vec())
    }

    /// The facts the run that made it read.
    pub(super) fn facts(&self, name: &str, step: Step) -> Vec<FactRef> {
        self.kept(name, step, None, |it| it.facts.iter().map(|&(fact, _)| fact).collect())
    }

    /// Makes its answer `bytes`: an entry whose answer does not decode, or another body's.
    pub(super) fn set_answer(&self, name: &str, step: Step, bytes: &[u8]) {
        self.kept(name, step, None, |it| it.answer = bytes.into());
    }

    /// Makes its answer under `schema` `bytes`.
    pub(super) fn set_answer_under(&self, name: &str, step: Step, schema: blaze_format::SchemaHash, bytes: &[u8]) {
        self.kept(name, step, Some(schema), |it| it.answer = bytes.into());
    }

    /// How many entries the kernel keeps beside the store.
    pub(super) fn entries(&self) -> usize {
        self.entries.lock().len()
    }
}

#[test]
fn records_read_back_as_written_and_only_under_their_key() {
    let base = ContentId([1; 32]);
    let facts = [
        FactRef { kind: QueryKind(0x1000), subject: ContentId([2; 32]), object: FactObject::None },
        FactRef {
            kind: QueryKind(0x1001),
            subject: ContentId([2; 32]),
            object: FactObject::Field(3),
        },
        FactRef {
            kind: QueryKind(0x1002),
            subject: ContentId([2; 32]),
            object: FactObject::Id(ContentId([5; 32])),
        },
    ];
    for listed in [&facts[..0], &facts[..]] {
        let mut shape = Vec::new();
        record::write_shape(&mut shape, &base, listed);
        let shape = Aligned::copy_from(&shape);
        let opened = ShapeRef::open(&shape, &base).unwrap();
        assert_eq!(opened.facts().collect::<Vec<_>>(), listed);
        assert!(ShapeRef::open(&shape, &ContentId([9; 32])).is_err(), "a shape of another base");
        for len in 0..shape.len() {
            assert!(
                ShapeRef::open(&Aligned::copy_from(&shape[..len]), &base).is_err(),
                "cut to {len}"
            );
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
        assert!(
            record::open_answer(&answer, &ContentId([8; 32])).is_err(),
            "an answer of another key"
        );
    }
    for step in Step::ALL {
        let shape_key = record::shape_key(&base, 0);
        assert_ne!(record::shape_at(step, &shape_key), record::answer_at(step, &shape_key));
        assert_ne!(
            record::base_key(&base, step, &infer::SCHEMA, &BUILD),
            record::base_key(&base, step, &infer::SCHEMA, &key)
        );
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
        assert_eq!(
            stable(&fresh.db, &fresh.facts, def),
            forms[name],
            "`{name}` reads back as it was stored"
        );
    }
    let hits = only(Step::Infer, StepCounts { hits: BODIES.len() as u64, ..StepCounts::default() });
    assert_eq!(fresh.installed.steps(), hits, "this build's counts, before the flush");
    let flushed = fresh.installed.flush().unwrap();
    assert_eq!(flushed, Flushed { steps: hits, ..Flushed::default() }, "a hit searches no record and offers nothing");
    assert_eq!(
        fresh.installed.steps(),
        [StepCounts::default(); Step::COUNT],
        "a flush takes the counts"
    );
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
    assert_eq!(flushed.written, 0, "a served step's record is the kernel's entry: {flushed:?}");
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
        assert!(
            show.iter().any(|it| is_impl_table(it)),
            "a method call reads an impl table: {show:?}"
        );
        let plain = names(function(&first.db, "plain"));
        assert!(!plain.iter().any(|it| is_impl_table(it)), "an empty body reads none: {plain:?}");
        assert_eq!(memory.facts("uses_show", Step::Infer), [IMPLS]);
        assert!(memory.facts("plain", Step::Infer).is_empty());
    }
    let fixture = format!("{FIXTURE}{ADDED_IMPL}");
    let fresh = session(second(&fixture), &memory, &defs, None, Mode::Serve);
    reset_ran();
    let _ = InferenceResult::of(&fresh.db, function(&fresh.db, "plain"));
    assert_eq!(ran(), 0, "a body whose shape lists no impls hits");
    let _ = InferenceResult::of(&fresh.db, function(&fresh.db, "uses_show"));
    assert_eq!(ran(), 1, "a body whose shape lists the impls misses");
    let flushed = fresh.installed.flush().unwrap();
    assert_eq!(flushed.written, 0, "a served step's record is the kernel's entry: {flushed:?}");
    assert_eq!(flushed.steps, only(Step::Infer, StepCounts { hits: 1, ran: 1, refused: 0 }));
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
    let flushed = without_lookups(refusing.installed.flush().unwrap());
    let steps = only(Step::Infer, StepCounts { hits: 0, ran: 1, refused: 1 });
    assert_eq!(flushed, Flushed { refused: vec![(read, 1)], steps, ..Flushed::default() });
    assert_eq!(memory.len(), 0);
}

#[test]
fn a_served_answer_that_does_not_decode_is_an_error_naming_the_body() {
    let (memory, defs, forms) = stored(Mode::Serve);
    for (bad, why) in [
        (&b"not an inference answer"[..], "a body that does not decode"),
        (&b""[..], "an empty answer"),
    ] {
        memory.set_answer("seven", Step::Infer, bad);
        let fresh = session(second(FIXTURE), &memory, &defs, None, Mode::Serve);
        let seven = function(&fresh.db, "seven");
        reset_ran();
        let _ = InferenceResult::of(&fresh.db, seven);
        assert_eq!(ran(), 1, "{why}: the body is inferred, which gives the same result");
        assert_eq!(stable(&fresh.db, &fresh.facts, seven), forms["seven"], "{why}");
        let error = fresh.installed.flush().expect_err(why);
        assert!(
            error.contains(&format!("{seven:?}")) && error.contains("inference"),
            "{why}: {error}"
        );
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
    assert_eq!(
        without_lookups(same.installed.flush().unwrap()),
        Flushed { steps: hits, ..Flushed::default() },
        "each stored result is the one inferred"
    );
    drop(same);

    let seven = memory.answer("seven", Step::Infer);
    memory.set_answer("plain", Step::Infer, &seven);
    let wrong = session(second(FIXTURE), &memory, &defs, None, Mode::Verify);
    let plain = function(&wrong.db, "plain");
    let _ = InferenceResult::of(&wrong.db, plain);
    let error =
        wrong.installed.flush().expect_err("a stored result that differs from inferring it");
    assert!(error.contains(&format!("{plain:?}")) && error.contains("differs"), "{error}");
}

/// The checker the stand-in's verdicts are stored for.
const CHECKER: &str = "polonius";

/// The function every [`Verdict`] is of.
const CHECKED: &str = "seven";

/// A borrow check's stand-in: [`CHECKED`]'s check passes when `passes` says so. Made with its
/// snapshot ([`with_verdicts`]), before any id of it can be taken, so it names its body by the
/// function's name.
#[blaze_memo::input]
struct Verdict {
    #[returns(copy)]
    passes: bool,
}

/// Both [`Verdict`]s of a database [`with_verdicts`] made, failing then passing.
#[blaze_memo::input(singleton)]
struct Verdicts {
    #[returns(copy)]
    verdicts: [Verdict; 2],
}

/// The fact the verdicts are made under ([`with_verdicts`]).
fn verdicts_fact() -> blaze_memo::Fact {
    blaze_memo::Fact {
        id: ContentId::of("body store test: the verdicts", CHECKED.as_bytes()),
        digest: ContentId::of("body store test: the verdicts' fields", CHECKED.as_bytes()),
    }
}

/// `db` with both [`Verdict`]s made in its session arena, held by [`Verdicts`]: inputs are made
/// only while a snapshot is built, so the verdicts are a maker of the session arena
/// (`Edit::set_given`) and its commit makes them. Each verdict's fact is known by `passes`,
/// which is its digest.
fn with_verdicts(mut db: Db) -> Db {
    let make = |_: &blaze_memo::Builder, inputs: &blaze_memo::Inputs<'_>| {
        let verdict = |passes: bool| {
            let fact = blaze_memo::Fact {
                id: ContentId::of("body store test: a verdict", &[u8::from(passes)]),
                digest: ContentId::of("body store test: a verdict's pass", &[u8::from(passes)]),
            };
            Verdict::new(inputs, fact, passes)
        };
        Verdicts::set(inputs, verdicts_fact(), [verdict(false), verdict(true)]);
    };
    let mut edit = db.edit();
    edit.set_given(None, verdicts_fact(), Box::new_in(make, Global));
    _ = edit.commit();
    db
}

/// The [`Verdict`] of `db` ([`with_verdicts`]) that passes where `passes` says.
fn new_verdict(db: &Db, passes: bool) -> Verdict {
    Verdicts::get(db).verdicts(db)[usize::from(passes)]
}

/// What `hir_ty_borrowck::borrowck` does with the store: the check, captured, and its verdict
/// offered when it passes. Keyed by the crate [`CHECKED`] is in as well
/// ([`borrow_checked_in`]): the check reads that crate's arena, and a key of the session's
/// [`Verdict`] alone is homed at the session arena, which may read no crate's.
fn borrow_checked(db: &Db, verdict: Verdict) -> bool {
    borrow_checked_in(db, verdict, crate_named(db, "main"))
}

#[blaze_memo::tracked(returns(copy))]
fn borrow_checked_in(db: &Db, verdict: Verdict, _krate: Crate) -> bool {
    let (passes, reads) = capture(db, || verdict.passes(db));
    if passes {
        offer_passed(db, function(db, CHECKED), CHECKER, reads);
    }
    passes
}

#[test]
fn only_a_passing_borrow_check_is_stored() {
    let memory = Arc::new(Memory::default());
    let defs = Defs::default();
    let first = session(with_verdicts(db_of(FIXTURE)), &memory, &defs, None, Mode::Serve);
    let seven = function(&first.db, CHECKED);
    let verdict = new_verdict(&first.db, false);
    assert!(!borrow_checked(&first.db, verdict));
    assert_eq!(
        first.installed.flush().unwrap(),
        Flushed::default(),
        "a failing verdict is never offered"
    );
    assert!(!lookup_passed(&first.db, seven, CHECKER, || panic!("not verifying")));
    assert_eq!(
        first.installed.steps(),
        only(Step::Borrowck, StepCounts { ran: 1, ..StepCounts::default() })
    );

    // The passing verdict: another input, made with the snapshot as both are
    // ([`with_verdicts`]), so another key of the check.
    let verdict = new_verdict(&first.db, true);
    assert!(borrow_checked(&first.db, verdict));
    kept_by_the_kernel(&first, &memory);
    assert_eq!(first.installed.flush().unwrap().written, 0, "the pass is the kernel's entry");
    drop(first);

    let fresh = session(second(FIXTURE), &memory, &defs, None, Mode::Serve);
    let seven = function(&fresh.db, "seven");
    assert!(
        lookup_passed(&fresh.db, seven, CHECKER, || panic!("not verifying")),
        "a stored pass skips the check"
    );
    assert!(
        !lookup_passed(&fresh.db, seven, "nll", || panic!("not stored")),
        "another checker's pass is not this one's"
    );
    assert!(!lookup_passed(&fresh.db, function(&fresh.db, "plain"), CHECKER, || panic!(
        "not stored"
    )));
    drop(fresh);

    let verify = session(second(FIXTURE), &memory, &defs, None, Mode::Verify);
    let seven = function(&verify.db, "seven");
    assert!(!lookup_passed(&verify.db, seven, CHECKER, || true), "under verify the check runs");
    let hit = only(Step::Borrowck, StepCounts { hits: 1, ..StepCounts::default() });
    assert_eq!(
        without_lookups(verify.installed.flush().unwrap()),
        Flushed { steps: hit, ..Flushed::default() }
    );
    assert!(!lookup_passed(&verify.db, seven, CHECKER, || false));
    let error = verify.installed.flush().expect_err("a stored pass whose check fails");
    assert!(error.contains(&format!("{seven:?}")) && error.contains("borrow check"), "{error}");
}

#[test]
fn a_step_value_is_the_same_in_every_database_and_changes_with_a_type() {
    let defs = Defs::default();
    let without = db_of(FIXTURE);
    assert_eq!(
        step_value(&without, function(&without, "seven"), Step::Infer),
        None,
        "no store, no value"
    );

    let first = session(db_of(FIXTURE), &Arc::new(Memory::default()), &defs, None, Mode::Serve);
    let fresh = session(second(FIXTURE), &Arc::new(Memory::default()), &defs, None, Mode::Serve);
    let value = |s: &Session, name: &str| step_value(&s.db, function(&s.db, name), Step::Infer);
    for name in BODIES {
        let here = value(&first, name);
        assert!(here.is_some(), "`{name}` has a stored form");
        assert_eq!(here, value(&fresh, name), "`{name}` has one value in every database");
    }
    assert_eq!(step_value(&first.db, function(&first.db, "seven"), Step::Borrowck), None);

    let edited = session(
        second(&FIXTURE.replace("{ 7 }", "{ 7u8 as u32 }")),
        &Arc::new(Memory::default()),
        &defs,
        None,
        Mode::Serve,
    );
    assert_ne!(
        value(&edited, "seven"),
        value(&first, "seven"),
        "an edit that changes a type changes the value"
    );
    assert_eq!(value(&edited, "plain"), value(&first, "plain"), "the other bodies keep theirs");
}

#[test]
fn a_database_finds_only_its_own_store() {
    let (memory, defs, _) = stored(Mode::Serve);
    let with = session(second(FIXTURE), &memory, &defs, None, Mode::Serve);
    let facts: Arc<dyn BodyFacts> = with.facts.clone();
    assert!(
        install(&with.db, memory.clone(), facts, BUILD, Mode::Serve).is_err(),
        "one store per database"
    );
    let without = second(FIXTURE);
    assert!(matches!(lookup_inference(&without, function(&without, "seven")), Lookup::Off));
    assert!(matches!(
        lookup_inference(&with.db, function(&with.db, "seven")),
        Lookup::Hit { verify: false, .. }
    ));
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
        let body = memory.answer(name, Step::Infer);
        let (decoded, decoding) =
            measure(|| infer::decode(&fresh.db, &*fresh.facts, def, &Aligned::copy_from(&body)));
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
        assert_at_most(0, "an item store lookup with no store for the database", || {
            lookup_inference(&without, def)
        }),
        Lookup::Off
    ));
    assert!(!assert_at_most(0, "a borrow check lookup with no store for the database", || {
        lookup_passed(&without, def, CHECKER, || true)
    }));
}

/// A store that shows a put only once its driver made it, as the daemon's does (blaze-cargo
/// `serve/unit/deps.rs`, `Images`: every put of a flush is buffered until the flush has ended):
/// a get reads what was committed, and a second put of a key with other bytes is refused
/// whether the first was committed or not.
struct Buffered {
    kept: Memory,
    puts: Lock<Vec<(ContentId, Vec<u8>)>>,
}

impl Default for Buffered {
    fn default() -> Buffered {
        Buffered {
            kept: Memory::default(),
            puts: Lock::new("test.ty.body_store.buffered", Vec::new()),
        }
    }
}

impl Buffered {
    /// Makes every buffered put, as the driver does once the flush has ended.
    fn commit(&self) {
        for (key, bytes) in std::mem::take(&mut *self.puts.lock()) {
            self.kept.put(key, bytes).expect("a buffered put is of a key the store lacks");
        }
    }
}

impl ImageStore for Buffered {
    fn get(&self, key: &ContentId) -> Result<Option<ImageBuf>, String> {
        self.kept.get(key)
    }

    fn put(&self, key: ContentId, value: Vec<u8>) -> Result<(), String> {
        match self.kept.get(&key)? {
            Some(kept) if kept.bytes() == value.as_slice() => return Ok(()),
            Some(_) => return Err(format!("the store holds other bytes under {key:?}")),
            None => {}
        }
        let mut puts = self.puts.lock();
        match puts.iter().find(|it| it.0 == key) {
            Some(put) if put.1 == value => {}
            Some(_) => return Err(format!("two images were made under {key:?}")),
            None => puts.push((key, value)),
        }
        Ok(())
    }

    fn reads_now(&self, _: &base_db::Env, _: &[u8]) -> Result<ContentId, String> {
        Err(String::from("an item store reads no expansion"))
    }

    // dyn: `ImageStore::pages`'s signature, once per crate read from pages
    fn pages(&self, key: &ContentId) -> Result<Option<std::sync::Arc<dyn hir_def::deps::PageSource>>, String> {
        self.kept.pages(key)
    }

    // dyn: `ImageStore::for_each`'s signature, one call per batch of crates
    fn for_each(&self, count: usize, task: &(dyn Fn(usize) + Sync)) {
        self.kept.for_each(count, task);
    }
}

/// One flush of two steps of one base key whose shapes differ (two bodies alike enough to share
/// an identity, each reading a fact the other does not), into a store that shows no put before
/// its driver made it; then what the store holds, checked: `Err` says what the flush lost.
///
/// The steps are offered as a worker's offer buffers them, under a base key of the test's own.
fn two_shapes_of_one_base_key_flushed_at_once() -> Result<(), String> {
    let store = Arc::new(Buffered::default());
    let db = db_of(FIXTURE);
    let facts = Arc::new(TestFacts::new(&db, &Defs::default(), None));
    let installed = install(&db, store.clone(), facts, BUILD, Mode::Serve).unwrap();
    let def = function(&db, "plain");
    let base = ContentId([3; 32]);
    let fact = |subject: u8| FactRef {
        kind: QueryKind(0x1803),
        subject: ContentId([subject; 32]),
        object: FactObject::None,
    };
    let offers: [(FactRef, ContentId, &[u8]); 2] =
        [(fact(5), ContentId([6; 32]), b"one"), (fact(7), ContentId([8; 32]), b"two")];
    let mut shapes = Vec::new();
    for (fact, value, answer) in offers {
        let mut shape = Vec::new();
        record::write_shape(&mut shape, &base, &[fact]);
        installed.store().pending.push(super::pending::Offered {
            step: Step::Mir,
            def,
            base: &base,
            shape: &shape,
            values: &[value],
            answer,
        });
        shapes.push((shape, value, answer));
    }
    let flushed = installed.flush().map_err(|why| format!("the flush stopped: {why}"))?;
    if let Some(why) = &flushed.failure {
        return Err(format!("{} of 2 steps were not kept: {why}", flushed.failed));
    }
    store.commit();
    let mut taken = Vec::new();
    for (shape, value, answer) in &shapes {
        let slot = (0..blaze_record::Bounds::STANDARD.width()).find(|&n| {
            let at = record::shape_at(Step::Mir, &record::shape_key(&base, n));
            store.get(&at).unwrap().is_some_and(|kept| kept.bytes() == shape.as_slice())
        });
        let Some(n) = slot else {
            return Err(format!("the shape of answer {answer:?} is in no slot"));
        };
        if taken.contains(&n) {
            return Err(format!("two shapes in slot {n}"));
        }
        taken.push(n);
        let mut key = AnswerKey::new(shape);
        key.value(Digest::Is(*value));
        let answer_key = key.finish();
        let Some(kept) = store.get(&record::answer_at(Step::Mir, &answer_key)).unwrap() else {
            return Err(format!("no answer {answer:?} under its shape, of slot {n}"));
        };
        if record::open_answer(kept.bytes(), &answer_key)? != *answer {
            return Err(format!("another answer than {answer:?} under its key"));
        }
    }
    Ok(())
}

/// The invariant: every step a flush is given is kept, whatever else the same flush holds; two
/// shapes of one base key take two slots, in a store that shows no put before its driver made
/// it. Until 2026-11-10 the second shape was placed against the store alone, found slot 0
/// free, and was refused by the put the first had buffered ("keeping the shape of the MIR of
/// StaticId(..): two images were made under .."), which lost everything the build left for
/// the store and its front end with it: every build of ripgrep, whose crates have statics and
/// functions alike but for their names.
#[test]
fn a_flush_keeps_two_shapes_of_one_base_key() {
    two_shapes_of_one_base_key_flushed_at_once().unwrap();
}

/// The control: a flush that forgets what it placed (`slots::FORGETFUL`) loses the second shape,
/// and the oracle says so in the daemon's words.
#[test]
fn the_oracle_catches_a_flush_that_forgets_what_it_placed() {
    use crate::next_solver::goal_store::slots::FORGETFUL;
    FORGETFUL.set(true);
    let lost = two_shapes_of_one_base_key_flushed_at_once();
    FORGETFUL.set(false);
    let why = lost.expect_err("the second shape of a base key is placed against the store alone");
    assert!(
        why.contains("1 of 2 steps were not kept")
            && why.contains("keeping the shape of the MIR")
            && why.contains("two images were made under"),
        "{why}"
    );
}

/// One step's refusal is not the flush's: a step whose answer the store refuses is named in
/// what the flush did, and the step offered beside it is kept.
#[test]
fn a_step_the_store_refuses_keeps_no_other_step_out() {
    let store = Arc::new(Buffered::default());
    let db = db_of(FIXTURE);
    let facts = Arc::new(TestFacts::new(&db, &Defs::default(), None));
    let installed = install(&db, store.clone(), facts, BUILD, Mode::Serve).unwrap();
    let def = function(&db, "plain");
    let (clashing, other) = (ContentId([3; 32]), ContentId([4; 32]));
    let value = ContentId([6; 32]);
    // Two answers under one key of fact values, and one step of another base key.
    let offers: [(ContentId, &[u8]); 3] = [(clashing, b"one"), (clashing, b"two"), (other, b"three")];
    for (base, answer) in offers {
        let mut shape = Vec::new();
        record::write_shape(&mut shape, &base, &[IMPLS]);
        installed.store().pending.push(super::pending::Offered {
            step: Step::Infer,
            def,
            base: &base,
            shape: &shape,
            values: &[value],
            answer,
        });
    }
    let flushed = installed.flush().expect("a step's refusal stops no flush");
    assert_eq!((flushed.written, flushed.failed), (2, 1), "{flushed:?}");
    let why = flushed.failure.expect("the refusal is named");
    assert!(why.contains("the answer of the inference of") && why.contains("FunctionId"), "{why}");
    store.commit();
    let mut shape = Vec::new();
    record::write_shape(&mut shape, &other, &[IMPLS]);
    let mut key = AnswerKey::new(&shape);
    key.value(Digest::Is(value));
    let at = record::answer_at(Step::Infer, &key.finish());
    assert!(store.get(&at).unwrap().is_some(), "the other step's answer is kept");
}

/// `main`'s bodies inferred in a fresh database over a store that holds them all, under
/// `mode`, with the hits of `left_out` not noted (the planted bug): how many of the
/// inferences' read lists (`blaze_memo::record`, around each ask) list the stored answer's
/// own fact, under the step's base key (`Store::lookup`). The test snapshot keeps read lists.
fn hits_listed(mode: Mode, left_out: Option<Step>) -> usize {
    let (memory, defs, _) = stored(Mode::Serve);
    let fresh = session(second(FIXTURE), &memory, &defs, None, mode);
    let db = &fresh.db;
    super::UNREPORTED.with(|it| it.set(left_out));
    reset_ran();
    let mut listed = 0;
    for name in BODIES {
        let def = function(db, name);
        let base = super::with_store(db, |store| {
            let identity = store.facts.identity(db, def)?;
            Some(super::record::base_key(&identity, Step::Infer, &super::infer::SCHEMA, &store.version))
        })
        .flatten()
        .expect("a body of the fixture has an identity");
        let ((), reads) = blaze_memo::record(blaze_memo::Database::memo(db), || {
            let _ = InferenceResult::of(db, def);
        })
        .expect("the test snapshot keeps read lists");
        listed += usize::from(reads.iter().any(|read| {
            matches!(read, blaze_memo::Read::Input { kind, fact } if *kind == super::STORED_ANSWER && fact.id == base)
        }));
    }
    super::UNREPORTED.with(|it| it.set(None));
    let hits = only(Step::Infer, StepCounts { hits: BODIES.len() as u64, ..StepCounts::default() });
    assert_eq!(fresh.installed.steps(), hits, "every body's inference was found in the store");
    listed
}

/// The invariant: a query whose value the item store answered lists the stored answer as a
/// fact it read, each one, so its read list is never taken for every read its value rests
/// on alone: after every body of `main` is found in the store, exactly as many read lists
/// list it as there were hits. What else those lists hold is their record's key: the body's
/// identity and each fact of its shape, read in the hit.
#[test]
fn every_row_answered_from_the_store_is_told_to_the_engine() {
    assert_eq!(hits_listed(Mode::Serve, None), BODIES.len());
    assert_eq!(ran(), 0, "none was inferred");
}

/// Under `Mode::Verify` a hit runs the step as well, in its own query and by its own reads:
/// no read list lists the stored answer.
#[test]
fn a_verified_hit_ran_by_its_own_reads_and_is_not_told() {
    assert_eq!(hits_listed(Mode::Verify, None), 0);
    assert_eq!(ran(), BODIES.len() as u32, "each was inferred as well");
}

/// The control (test-the-cause): with the hit site's note left out for the step, the same
/// hits list no stored answer, which the count of
/// `every_row_answered_from_the_store_is_told_to_the_engine` fails on; left out for another
/// step, which made no hit here, nothing changes.
#[test]
fn the_count_catches_a_hit_site_that_does_not_tell_the_engine() {
    assert_eq!(hits_listed(Mode::Serve, Some(Step::Infer)), 0);
    assert_eq!(hits_listed(Mode::Serve, Some(Step::Mir)), BODIES.len());
}

/// A database with an item store in `Mode::Trace` ([`install_trace`]), and its facts: no store
/// of records is given to it.
fn trace_session(db: Db) -> Session {
    let facts = Arc::new(TestFacts::new(&db, &Defs::default(), None));
    let installed = install_trace(&db, facts.clone(), BUILD).unwrap();
    Session { installed, facts, db }
}

// `a_trace_is_the_same_in_two_fresh_databases` asks built MIR, which this test binary cannot
// build: it is hir-ty-mir's (`tests::mir::body_store`), with its own facts.

/// What `hir_ty_borrowck::borrowck` does with the store, for [`Verdict`]'s stand-in: a stored
/// pass answers; else the check runs, captured, and its verdict is offered (traced whatever it
/// is under `Mode::Trace`, `b"fails"` for a failure as the stand-in's encoding).
/// Keyed by the crate [`CHECKED`] is in as well ([`verdict_of_in`]), as [`borrow_checked`] is.
fn verdict_of(db: &Db, verdict: Verdict) -> bool {
    verdict_of_in(db, verdict, crate_named(db, "main"))
}

#[blaze_memo::tracked(returns(copy))]
fn verdict_of_in(db: &Db, verdict: Verdict, _krate: Crate) -> bool {
    let def = function(db, CHECKED);
    if lookup_passed(db, def, CHECKER, || panic!("no store here verifies")) {
        return true;
    }
    let (passes, reads) = capture(db, || verdict.passes(db));
    offer_verdict(db, def, CHECKER, reads, passes, |out| {
        if !passes {
            out.extend_from_slice(b"fails");
        }
    });
    passes
}

/// Plants in `s`'s store a served answer of [`CHECKED`]'s borrow check that says it passes,
/// valid while its identity is what it is now (it read no fact): an answer that differs from
/// the run's, whose verdict fails. Gives the step's schema.
fn plant_a_pass(s: &Session) -> blaze_format::SchemaHash {
    let db = &s.db;
    let def = function(db, CHECKED);
    let schema = entries::borrowck_schema(db, def, CHECKER);
    let identity = s.facts.identity(db, def).expect("the checked body has an identity");
    let planted = super::entries::Served { identity, facts: Box::new([]), answer: Arc::from(&[][..]) };
    let name = super::entries::BodyName::of(db, def).expect("the checked body has a name");
    super::with_store(db, |store| store.served.lock().post(name, def, Step::Borrowck, schema, planted))
        .expect("the database's item store");
    schema
}

/// The invariant of `Mode::Trace` and its control: no step's answer comes from anywhere but
/// its own run in this process. With a stored answer planted that differs from the true one
/// (a pass of a check that fails), `Mode::Serve` answers with the stored one; `Mode::Trace`
/// with the run's, and its trace is the run's: the planted pass never reaches it.
#[test]
fn under_trace_no_answer_comes_from_anywhere_but_its_run() {
    let serve = session(with_verdicts(db_of(FIXTURE)), &Arc::new(Memory::default()), &Defs::default(), None, Mode::Serve);
    plant_a_pass(&serve);
    let failing = new_verdict(&serve.db, false);
    assert!(verdict_of(&serve.db, failing), "under `Mode::Serve` the planted pass is the answer");
    drop(serve);

    let trace = trace_session(with_verdicts(db_of(FIXTURE)));
    let schema = plant_a_pass(&trace);
    let seven = function(&trace.db, CHECKED);
    assert!(
        !lookup_passed(&trace.db, seven, CHECKER, || panic!("nothing is looked up")),
        "under `Mode::Trace` nothing is looked up"
    );
    let failing = new_verdict(&trace.db, false);
    assert!(!verdict_of(&trace.db, failing), "under `Mode::Trace` the run's verdict is the answer");
    let traced = trace.installed.trace(seven, Step::Borrowck, schema).expect("the failing verdict is traced");
    let Traced::Kept { answer, .. } = &*traced else { panic!("refused: {traced:?}") };
    assert_eq!(&answer[..], b"fails", "the trace is the run's, not the planted pass");
}
