//! Body entries carried through the kernel (end-state.md, order row 3): a two-crate program's
//! bodies, each with its inference, MIR and borrow check entries in one session, carried
//! across an edit of the leaf crate's file with the change set a driver gives (the file's text
//! differs; the impls fact from the impls maintainer's last write; identities from
//! [`Identities`]' exact delta, or always dirty and counted). Each test counts the entries'
//! runs and checks every entry against a fresh run (the oracle); one control drops the
//! identity read from every trace, another leaves a changed identity out of the delta, and the
//! oracle must catch the stale entry each leaves. A second session over the same store, with a
//! new database, hits every entry.
//!
//! MIR and the borrow check are stand-ins here (hir-ty's tests link no MIR builder and no
//! borrow checker): [`fake_mir`] reads the body's inference and offers it as its answer;
//! [`fake_borrowck`] reads the stand-in MIR and passes. Both offer through the item store as
//! the real ones do, so their traces come the same way.

use std::{cell::Cell, collections::BTreeMap, sync::Arc};

use base_db::{Crate, Db, all_crates};
use blaze_cas::{Key as StoreKey, Kind as StoreKind, Mismatch, content_of};
use blaze_format::{Part, SchemaHash, schema_of};
use blaze_front_items::{
    Reading,
    entry::ItemId,
    steps::{FileKey, Items, Parse, Source, reads as source},
};
use blaze_hash::HashMap;
use blaze_ids::{ContentId, Hasher};
use blaze_kernel::{
    Changed, CodeId, Get, Kernel, Pin, Put, Read, ReadKind, Seen, Session, World, WorldError,
};
use blaze_core::lock::Lock;
use blaze_query::QueryKind;
use blaze_query::db::AsId;
use blaze_store_check::Setting;
use hir_def::{
    AdtId, ConstId, DefWithBodyId, EnumId, EnumVariantId, FunctionId, ImplId, ModuleDefId, StaticId,
    StructId, TraitId, TypeAliasId, UnionId, VariantId,
    deps::{PortableIds, body_key as hir_body_key},
    expr_store::Body,
    nameres::{crate_def_map, demand::crate_body_owners},
    signatures::FunctionSignature,
};
use hir_expand::fixture::{self, Fixture};
use std::alloc::Global;
use syntax::{Edition, Extensions};

use super::{
    ANY_REVISION, BodyDelta, BodyKey, BodyWorld, BorrowckEntry, DROPPED, IDENTITY_NAME, Identities,
    InferEntry, MirEntry, RUNS, always_dirty, bodies, borrowck_schema, def_of, reads, register,
    run_infer, serve,
};
use crate::{
    InferenceResult,
    body_store::{
        BodyFacts, Classified, Installed, Mode, Step, StepCounts, capture, infer, install,
        lookup_passed, offer, offer_passed,
        tests::{BUILD, Memory as Images, OWN, is_impl_table, own, portable},
    },
    db::HirDatabase,
    next_solver::{
        GenericArgs, ParamEnv, SolverDefId,
        goal_store::{FactObject, FactRef},
    },
    traits::ParamEnvAndCrate,
};

const FIXTURE: &str = r#"
//- minicore: sized, copy, add
//- /leaf.rs crate:leaf deps:core new_source_root:library
pub fn leaf() -> u32 { 1 }
pub fn other() -> u32 { 2 }
//- /main.rs crate:main deps:core,leaf new_source_root:local
pub fn caller() -> u32 { let _x = leaf::leaf(); 1 }
pub fn twice() -> u32 { caller() + caller() }
"#;

/// The files as the fixture has them, by stable name, each with its crate, in fixture order.
const FILES: [(&str, &str); 2] = [("leaf.rs", "leaf"), ("main.rs", "main")];
const LEAF: &str = "pub fn leaf() -> u32 { 1 }\npub fn other() -> u32 { 2 }\n";
const MAIN: &str =
    "pub fn caller() -> u32 { let _x = leaf::leaf(); 1 }\npub fn twice() -> u32 { caller() + caller() }\n";

const HASHED: &str = "blaze v2 body entries test 1";
const CHECKER: &str = "body entries test checker";
const MIR_SCHEMA: SchemaHash = schema_of(&[Part::Str("blaze v2 body entries test MIR 1")]);

/// The facts the stand-ins' reads of the body's own steps stand for, and every impl table's.
const OWN_INFER: FactRef =
    FactRef { kind: QueryKind(0x18f0), subject: ContentId([1; 32]), object: FactObject::None };
const OWN_MIR: FactRef =
    FactRef { kind: QueryKind(0x18f1), subject: ContentId([1; 32]), object: FactObject::None };
const IMPLS: FactRef =
    FactRef { kind: QueryKind(0x1800), subject: ContentId([4; 32]), object: FactObject::None };

/// What decides the layout of the leaf crate's struct, where a test gives it one ([`SIZED`]):
/// the fact a read of its field types, met below a layout row the stand-in MIR read, stands for.
const LAYOUT: FactRef =
    FactRef { kind: QueryKind(0x18f2), subject: ContentId([5; 32]), object: FactObject::None };

/// The leaf crate's struct, where its file has one.
fn sized(db: &Db) -> Option<StructId> {
    let def_map = crate_def_map(db, crate_named(db, "leaf"));
    def_map.modules(db).find_map(|(_, module)| {
        module.scope(db).declarations().find_map(|it| match it {
            ModuleDefId::AdtId(AdtId::StructId(it)) => Some(it),
            _ => None,
        })
    })
}

/// The size of the leaf crate's struct in bytes, by its layout: what the stand-in MIR reads.
fn size_of(db: &Db, s: StructId) -> u64 {
    let env = ParamEnvAndCrate { param_env: ParamEnv::empty(), krate: crate_named(db, "leaf") };
    db.layout_of_adt(AdtId::StructId(s), GenericArgs::empty().store(), env.store())
        .map_or(u64::MAX, |it| it.size.bytes())
}

/// [`LAYOUT`]'s value: how many fields the leaf crate's struct has; none where it has no struct.
fn layout(db: &Db) -> Option<ContentId> {
    let fields = VariantId::from(sized(db)?).fields(db).fields().len() as u64;
    Some(Hasher::new("blaze v2 body entries test layout").update(&fields.to_le_bytes()).finish())
}

fn reading() -> Reading {
    Reading { edition: Edition::CURRENT, extensions: Extensions::NONE }
}

fn crate_named(db: &Db, name: &str) -> Crate {
    *all_crates(db)
        .iter()
        .find(|it| it.extra_data(db).display_name.as_ref().is_some_and(|it| it.to_string() == name))
        .unwrap_or_else(|| panic!("no crate `{name}`"))
}

/// How many impls the fixture's crates have: what [`IMPLS`] is, as any crate sees it.
fn impls(db: &Db) -> ContentId {
    let count: usize = FILES
        .iter()
        .map(|(_, krate)| {
            let def_map = crate_def_map(db, crate_named(db, krate));
            def_map.modules(db).map(|(_, module)| module.scope(db).impls().len()).sum::<usize>()
        })
        .sum();
    Hasher::new("blaze v2 body entries test impls").update(&(count as u64).to_le_bytes()).finish()
}

/// Lane item-hash's part for these tests: no memo, so every identity is of the database as it
/// is now. A body's identity is its key, its HIR printed and every function's signature in the
/// fixture (a stand-in for item-hash's "entries" part: what each id the body names is), so a
/// signature edit reaches every body's identity and a body edit only its own.
///
/// An item an answer names is keyed by its kind and its id in this one database ([`def_key`]):
/// the same in every revision of it, so an answer's bytes are a function of what the entry
/// read. (The item store's [`TestFacts`](super::super::tests::TestFacts) keys an item by its
/// portable form, whose header holds the text of the item's file: any edit of `leaf.rs`, a
/// comment's too, gave every caller's answer other bytes than the entry had read, with no read
/// of that file in its trace.)
/// What [`EntryFacts::def_key`] gave each id it named, by its key: shared by two trees, so a
/// second database decodes what the first encoded (the keys are a tag and the interned id's
/// bits, equal in two fixture databases built alike).
type Defs = Arc<Lock<HashMap<ContentId, (u8, PortableIds), Global>>>;

struct EntryFacts {
    defs: Defs,
}

/// `def`'s id in its database, as its interned id's bits.
fn raw(def: SolverDefId<'_>) -> u32 {
    match def {
        SolverDefId::FunctionId(it) => it.as_id().as_u32(),
        SolverDefId::AdtId(AdtId::StructId(it)) => it.as_id().as_u32(),
        SolverDefId::AdtId(AdtId::UnionId(it)) => it.as_id().as_u32(),
        SolverDefId::AdtId(AdtId::EnumId(it)) => it.as_id().as_u32(),
        SolverDefId::ConstId(it) => it.as_id().as_u32(),
        SolverDefId::StaticId(it) => it.as_id().as_u32(),
        SolverDefId::TraitId(it) => it.as_id().as_u32(),
        SolverDefId::TypeAliasId(it) => it.as_id().as_u32(),
        SolverDefId::ImplId(it) => it.as_id().as_u32(),
        SolverDefId::EnumVariantId(it) => it.as_id().as_u32(),
        _ => panic!("a key asked of a definition that is no item"),
    }
}

fn signatures(db: &Db) -> ContentId {
    let mut h = Hasher::new("blaze v2 body entries test signatures");
    for (_, krate) in FILES {
        for def in crate_body_owners(db, crate_named(db, krate)) {
            let DefWithBodyId::FunctionId(it) = def else { continue };
            // The signature as it is written: its name, its types' store (no positions; the
            // source map is not in it), its flags.
            h.part(format!("{:?}", FunctionSignature::of(db, it)).as_bytes());
        }
    }
    h.finish()
}

impl BodyFacts for EntryFacts {
    fn identity(&self, db: &Db, def: DefWithBodyId) -> Option<ContentId> {
        let key = hir_body_key(db, def)?;
        let printed = Body::of(db, def).pretty_print(db, def, span::Edition::CURRENT);
        Some(
            Hasher::new("blaze v2 body entries test identity")
                .id(&key)
                .part(printed.as_bytes())
                .id(&signatures(db))
                .finish(),
        )
    }

    fn classify(
        &self,
        _: &Db,
        _: DefWithBodyId,
        step: Step,
        _: blaze_query::db::DatabaseKeyIndex,
        _: blaze_query::db::ReadKind,
        name: &'static str,
    ) -> Classified {
        if name.contains("layout_of_") {
            // As item-hash's `Rule::Through`: the layout row's own reads in its place.
            Classified::Through
        } else if step == Step::Mir && name.contains("field_types") {
            Classified::Fact(LAYOUT)
        } else if name.contains("infer_body_query") {
            Classified::Fact(OWN_INFER)
        } else if name.contains("fake_mir") {
            Classified::Fact(OWN_MIR)
        } else if is_impl_table(name) {
            Classified::Fact(IMPLS)
        } else {
            Classified::Covered
        }
    }

    fn value(&self, db: &Db, _: DefWithBodyId, fact: FactRef) -> Option<ContentId> {
        match fact {
            IMPLS => Some(impls(db)),
            LAYOUT => layout(db),
            OWN_INFER | OWN_MIR => Some(ContentId([9; 32])),
            _ => None,
        }
    }

    fn own_step(&self, fact: FactRef) -> Option<Step> {
        match fact {
            OWN_INFER => Some(Step::Infer),
            OWN_MIR => Some(Step::Mir),
            _ => None,
        }
    }

    fn def_key<'db>(&self, _: &'db Db, body: DefWithBodyId, def: SolverDefId<'db>) -> ContentId {
        if def == own(body) {
            return OWN;
        }
        let (tag, ids) = portable(def);
        let key = Hasher::new("blaze v2 body entries test def")
            .update(&[tag])
            .update(&raw(def).to_le_bytes())
            .finish();
        self.defs.lock().insert(key, (tag, ids));
        key
    }

    fn def_of<'db>(&self, _: &'db Db, body: DefWithBodyId, key: ContentId) -> Option<SolverDefId<'db>> {
        if key == OWN {
            return Some(own(body));
        }
        let defs = self.defs.lock();
        let (tag, ids) = defs.get(&key)?;
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

/// The MIR stand-in: the body's inference read, and its answer offered as the MIR's. Where
/// the leaf crate has a struct ([`SIZED`]) it reads the struct's size, by its layout, as a
/// real body's MIR reads a literal's type's, and its answer holds that size.
#[blaze_query::tracked(returns(copy))]
fn fake_mir(db: &Db, def: DefWithBodyId) -> bool {
    let (size, reads) = capture(db, || {
        let _ = InferenceResult::of(db, def);
        sized(db).map(|s| size_of(db, s))
    });
    offer(db, def, Step::Mir, MIR_SCHEMA, reads, |facts, out| {
        // The record first, then the size after it: `infer::encode` writes its record over
        // whatever the buffer held, so a size put before it would be lost.
        infer::encode(db, facts, def, InferenceResult::of(db, def), out)?;
        if let Some(size) = size {
            out.extend_from_slice(b"size ");
            out.extend_from_slice(&size.to_le_bytes());
        }
        Ok(())
    });
    true
}

/// The borrow check stand-in: the stand-in MIR read, and a pass offered.
#[blaze_query::tracked(returns(copy))]
fn fake_borrowck(db: &Db, def: DefWithBodyId) -> bool {
    let (passes, reads) = capture(db, || fake_mir(db, def));
    if passes {
        offer_passed(db, def, CHECKER, reads);
    }
    passes
}

/// The pinned files, the database at that pin with its item store, and the map from a body's
/// key to the database's body, made from the syntax.
struct Tree {
    // Declared first: the store is uninstalled before the database drops.
    installed: Installed,
    defs: Defs,
    fixture: Fixture,
    files: BTreeMap<String, String>,
    bodies: HashMap<(String, ItemId), DefWithBodyId, Global>,
    keys: Vec<BodyKey>,
}

impl Tree {
    fn new() -> Tree {
        Tree::with_defs(Arc::new(Lock::new("test.ty.body_entries.defs", HashMap::default())))
    }

    /// A tree whose facts name ids through `defs`, as another tree's do.
    fn with_defs(defs: Defs) -> Tree {
        let fixture = fixture::db(FIXTURE);
        // clone: a counted reference
        let facts = Arc::new(EntryFacts { defs: defs.clone() });
        let installed = install(fixture.db(), Arc::new(Images::default()), facts, BUILD, Mode::Serve)
            .expect("an item store for the test database");
        // The fixture's files as the database has them; `edit` sets both.
        let files = BTreeMap::from([("leaf.rs".to_owned(), LEAF.to_owned()), ("main.rs".to_owned(), MAIN.to_owned())]);
        let mut tree =
            Tree { installed, defs, fixture, files, bodies: HashMap::new_in(Global), keys: Vec::new() };
        tree.map();
        tree
    }

    /// The bodies of every file, from its `items` as the syntax gives them, each with the
    /// database's body of its name.
    fn map(&mut self) {
        self.bodies.clear();
        self.keys.clear();
        for (name, krate) in FILES {
            let text = &self.files[name];
            let record = blaze_front_items::parse(text, reading());
            let parsed = blaze_front_items::tree(&record, text, reading()).expect("a tree");
            let items = blaze_front_items::file_items(parsed.root()).expect("items").items;
            let (named, passed) = bodies(&items).expect("the file's bodies");
            assert_eq!(passed, 0, "the fixture has no body this map passes over");
            let db = self.fixture.db();
            let def_map = crate_def_map(db, crate_named(db, krate));
            for it in named {
                let def = def_of(db, def_map, def_map.crate_root(db), &it).expect("a body by its name");
                self.bodies.insert((name.to_owned(), it.item), def);
                let file = FileKey { name: name.to_owned(), reading: reading() };
                self.keys.push(BodyKey { file, item: it.item, krate: krate.to_owned() });
            }
        }
    }

    fn edit(&mut self, name: &str, text: &str) {
        let at = FILES.iter().position(|(it, _)| *it == name).expect("a fixture file");
        let id = self.fixture.file(at).file_id(self.fixture.db());
        self.fixture.db_mut().set_file_text(id, text);
        self.files.insert(name.to_owned(), text.to_owned());
        self.map();
    }

    /// Whether the impls maintainer's last write changed any crate's impl keys or traits.
    fn impls_written(&mut self) -> bool {
        use crate::method_resolution::delta::LastWrite;
        self.fixture.db_mut().with_state::<LastWrite, _>(|_, last| {
            last.crates.iter().any(|it| !it.keys_changed.is_empty() || !it.traits_changed.is_empty())
        })
    }
}

impl World for Tree {
    fn hash(&self, read: Read<'_>) -> Result<Seen, WorldError> {
        if let Some(hashed) = super::hash(self, read) {
            return hashed;
        }
        let name = std::str::from_utf8(read.key).map_err(|_| WorldError::new(read, "a key that is not UTF-8"))?;
        match read.kind {
            source::FILE => Ok(self.files.get(name).map_or(Seen::Gone, |text| Seen::Is(ContentId::of(HASHED, text.as_bytes())))),
            source::EXISTS => Ok(if self.files.contains_key(name) { Seen::Is(ContentId::of(HASHED, b"there")) } else { Seen::Gone }),
            source::CFG => Ok(Seen::Is(ContentId::of(HASHED, b"on"))),
            _ => Err(WorldError::new(read, "no such kind of read")),
        }
    }

    fn pin(&self) -> Pin {
        let mut hasher = Hasher::new(HASHED);
        for (name, text) in &self.files {
            hasher.part(name.as_bytes()).part(text.as_bytes());
        }
        Pin(hasher.finish())
    }
}

impl Source for Tree {
    fn file(&self, name: &str) -> Result<Option<&str>, WorldError> {
        Ok(self.files.get(name).map(String::as_str))
    }

    fn cfg(&self, _: &str, _: &str) -> Result<bool, WorldError> {
        Ok(true)
    }
}

impl BodyWorld for Tree {
    fn db(&self) -> &Db {
        self.fixture.db()
    }

    fn body(&self, key: &BodyKey) -> Result<DefWithBodyId, String> {
        // clone: a map key of a test
        match self.bodies.get(&(key.file.name.clone(), key.item)) {
            Some(def) => Ok(*def),
            None => Err(format!("no body of {key:?} in this pin's syntax")),
        }
    }

    fn run(&self, def: DefWithBodyId, step: Step) -> Result<SchemaHash, String> {
        let db = self.db();
        match step {
            Step::Infer => Ok(run_infer(db, def)),
            Step::Mir => {
                let _ = fake_mir(db, def);
                Ok(MIR_SCHEMA)
            }
            Step::Borrowck => match fake_borrowck(db, def) {
                true => Ok(borrowck_schema(db, def, CHECKER)),
                false => Err(String::from("its borrow check fails")),
            },
            Step::Const => Err(String::from("a constant's value has no body entry")),
        }
    }

    fn schema(&self, def: DefWithBodyId, step: Step) -> Result<SchemaHash, String> {
        match step {
            Step::Infer => Ok(infer::SCHEMA),
            Step::Mir => Ok(MIR_SCHEMA),
            Step::Borrowck => Ok(borrowck_schema(self.db(), def, CHECKER)),
            Step::Const => Err(String::from("a constant's value has no body entry")),
        }
    }

    fn absolute(&self, _: DefWithBodyId, fact: FactRef) -> FactRef {
        // The fake facts name no body relatively.
        fact
    }

    fn fact(&self, _: &str, fact: FactRef) -> Result<Seen, WorldError> {
        match fact {
            IMPLS => Ok(Seen::Is(impls(self.db()))),
            LAYOUT => Ok(layout(self.db()).map_or(Seen::Gone, Seen::Is)),
            _ => Ok(Seen::Gone),
        }
    }
}

/// A store in memory.
#[derive(Default)]
struct Memory {
    values: BTreeMap<StoreKey, Vec<u8>>,
}

impl Get for Memory {
    fn get(&self, key: &StoreKey) -> Result<Option<&[u8]>, String> {
        Ok(self.values.get(key).map(Vec::as_slice))
    }

    fn has(&self, key: &StoreKey) -> Result<bool, String> {
        Ok(self.values.contains_key(key))
    }
}

impl Put for Memory {
    fn put(&mut self, key: StoreKey, _: StoreKind, bytes: Vec<u8>) -> Result<(), Mismatch> {
        match self.values.get(&key) {
            Some(kept) if *kept != bytes => Err(Mismatch { key, kept: content_of(kept), given: content_of(&bytes) }),
            Some(_) => Ok(()),
            None => {
                self.values.insert(key, bytes);
                Ok(())
            }
        }
    }
}

fn kernel() -> Kernel<Tree> {
    kernel_of(BUILD)
}

/// A kernel whose body entries are of the front end `build`.
fn kernel_of(build: ContentId) -> Kernel<Tree> {
    let mut kernel = Kernel::new();
    kernel.register_ask::<Parse<Tree>>(CodeId(ContentId([61; 32]))).unwrap();
    kernel.register_ask::<Items<Tree>>(CodeId(ContentId([62; 32]))).unwrap();
    register(&mut kernel, CodeId(build)).unwrap();
    kernel
}

fn runs() -> [u32; Step::COUNT] {
    RUNS.with(Cell::get)
}

fn reset_runs() {
    RUNS.with(|it| it.set([0; Step::COUNT]));
}

/// Every body's three entries in `session`, by the body's key: `(infer, mir, borrowck)`.
fn ask_all(
    kernel: &Kernel<Tree>,
    session: &mut Session,
    store: &mut Memory,
    tree: &Tree,
) -> Vec<(String, [Vec<u8>; 3])> {
    // clone: the test's keys, so the tree is lent whole
    let keys = tree.keys.clone();
    keys.iter()
        .map(|key| {
            let off = &Setting::OFF;
            let infer = kernel.get_in::<InferEntry<Tree>, _>(session, store, tree, key, off).unwrap().value().to_vec();
            let mir = kernel.get_in::<MirEntry<Tree>, _>(session, store, tree, key, off).unwrap().value().to_vec();
            let check = kernel.get_in::<BorrowckEntry<Tree>, _>(session, store, tree, key, off).unwrap().value().to_vec();
            (format!("{key:?}"), [infer, mir, check])
        })
        .collect()
}

/// The oracle: every entry as a run with no session and an empty store gives it now.
/// The oracle: every body's entries as a fresh run of `tree`'s files gives them, in a new
/// database, store and session, so nothing `tree`'s database was served stands in for a run.
fn fresh(kernel: &Kernel<Tree>, tree: &Tree) -> Vec<(String, [Vec<u8>; 3])> {
    // clone: a counted reference, so the new tree names ids as `tree`'s facts do
    let mut clean = Tree::with_defs(tree.defs.clone());
    // clone: the test's file texts, so `clean` is edited while `tree` is lent
    for (name, text) in tree.files.clone() {
        clean.edit(&name, &text);
    }
    let mut store = Memory::default();
    let mut session = Session::new(&clean);
    ask_all(kernel, &mut session, &mut store, &clean)
}

/// What one carry across an edit of `leaf.rs` to `edited` did.
struct Carried {
    /// The entries' runs during the carry and the asks after it, by step.
    runs: [u32; Step::COUNT],
    /// The always-dirty count by name, asked before the carry.
    dirty: Vec<(&'static str, usize)>,
    /// The first entry that differs from a fresh run, named with both values; `Ok` when none.
    fresh: Result<(), String>,
}

const STEPS: [&str; 3] = ["infer", "mir", "borrowck"];

/// The first entry of `kept` that is not `fresh`'s: its body's key, its step, both lengths and
/// the first byte where they differ, with a window of both values around it.
fn differs(kept: &[(String, [Vec<u8>; 3])], fresh: &[(String, [Vec<u8>; 3])]) -> Result<(), String> {
    if kept.len() != fresh.len() {
        return Err(format!("{} entries kept, {} fresh", kept.len(), fresh.len()));
    }
    for ((key, kept), (fresh_key, fresh)) in kept.iter().zip(fresh) {
        if key != fresh_key {
            return Err(format!("body {key} kept where the fresh run has {fresh_key}"));
        }
        for (step, (kept, fresh)) in kept.iter().zip(fresh).enumerate() {
            if kept == fresh {
                continue;
            }
            let at = kept.iter().zip(fresh).position(|(a, b)| a != b).unwrap_or(kept.len().min(fresh.len()));
            let window = |it: &[u8]| format!("{:02x?}", &it[at.saturating_sub(8).min(it.len())..(at + 40).min(it.len())]);
            return Err(format!(
                "body {key}, {} entry: kept {} bytes, fresh {} bytes, first difference at byte {at}\n kept  {}\n fresh {}",
                STEPS[step],
                kept.len(),
                fresh.len(),
                window(kept),
                window(fresh),
            ));
        }
    }
    Ok(())
}

/// Where a carry's change set has its identity reads' difference from.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Ids {
    /// No delta: every identity read is always dirty.
    Dirty,
    /// [`Identities`]' exact delta.
    Exact,
    /// The planted bug of the delta's control: [`Identities`]' delta with every body of
    /// `main.rs` left out, as a maintainer that said a changed identity unchanged would.
    MainLeftOut,
}

fn carried(edited: &str, dropped: Option<ReadKind>, ids: Ids) -> Carried {
    let kernel = kernel();
    let mut tree = Tree::new();
    let mut identities = Identities::default();
    identities.delta(&tree, &tree.keys).expect("the first pin's identities");
    let mut store = Memory::default();
    let mut session = Session::new(&tree);
    DROPPED.with(|it| it.set(dropped));
    let before = ask_all(&kernel, &mut session, &mut store, &tree);
    DROPPED.with(|it| it.set(None));
    assert_eq!(before.len(), 4, "four bodies");
    let from = tree.pin();
    tree.edit("leaf.rs", edited);
    let mut changed = Changed::between(from, tree.pin());
    changed.differs(Read { kind: source::FILE, key: b"leaf.rs" });
    changed.exact(source::EXISTS);
    changed.exact(source::CFG);
    // The impls fact, from the impls maintainer's last write: which crate a fact is seen
    // from is the reader's, and both crates see the impls of both.
    // alloc: a test's change set
    let impls_differ: Vec<(FactRef, String)> = match tree.impls_written() {
        true => FILES.iter().map(|(_, krate)| (IMPLS, (*krate).to_owned())).collect(),
        false => Vec::new(),
    };
    let identities = match ids {
        Ids::Dirty => None,
        Ids::Exact => Some(identities.delta(&tree, &tree.keys).expect("the identity delta")),
        Ids::MainLeftOut => {
            let mut differ = identities.delta(&tree, &tree.keys).expect("the identity delta");
            differ.retain(|key| key.file.name != "main.rs");
            Some(differ)
        }
    };
    let delta = BodyDelta { exact: vec![(ReadKind(IMPLS.kind.0), impls_differ)], unknown: Vec::new(), identities };
    delta.say::<Tree>(&mut changed);
    let dirty = always_dirty(&mut session, &store, &delta).unwrap();
    reset_runs();
    let carry = kernel.carry(&mut session, &mut store, &tree, &changed, &Setting::OFF).unwrap();
    assert_eq!(carry.dropped, None, "{carry:?}");
    let after = ask_all(&kernel, &mut session, &mut store, &tree);
    let runs = runs();
    let fresh = differs(&after, &fresh(&kernel, &tree));
    Carried { runs, dirty, fresh }
}

/// The floor of a leaf body edit: the edited body's three entries, and nothing else. With
/// item-hash's identity delta no read is always dirty: the callers' entries are not valued.
#[test]
fn a_leaf_body_edit_reruns_its_three_entries_and_no_callers() {
    let leaf = carried(&LEAF.replace("{ 1 }", "{ 3 }"), None, Ids::Exact);
    assert_eq!(leaf.runs, [1, 1, 1, 0], "the edited body's inference, MIR and borrow check, once each");
    assert!(leaf.dirty.is_empty(), "the identity delta is exact, nothing is always dirty: {:?}", leaf.dirty);
    leaf.fresh.expect("every entry is what a fresh run gives");
}

/// The same edit with no identity delta: every entry's identity is always dirty, counted by
/// name, and still only the edited body's three entries rerun.
#[test]
fn with_no_identity_delta_every_identity_read_is_always_dirty() {
    let leaf = carried(&LEAF.replace("{ 1 }", "{ 3 }"), None, Ids::Dirty);
    assert_eq!(leaf.runs, [1, 1, 1, 0]);
    assert_eq!(leaf.dirty, [(IDENTITY_NAME, 12)], "every entry reads its identity");
    leaf.fresh.expect("every entry is what a fresh run gives");
}

/// The floor of a comment edit: no entry.
#[test]
fn a_comment_edit_reruns_no_entry() {
    let comment = carried(&format!("// a comment\n{LEAF}"), None, Ids::Exact);
    assert_eq!(comment.runs, [0; Step::COUNT], "a comment changes no body's record");
    assert!(comment.dirty.is_empty(), "{:?}", comment.dirty);
    comment.fresh.expect("every entry is what a fresh run gives");
}

/// A signature edit of the leaf reaches its caller through the caller's identity read: the
/// caller's inference names the leaf, whose type is other now.
#[test]
fn a_signature_edit_reaches_the_caller_through_its_identity() {
    let signature = carried(&LEAF.replace("leaf() -> u32", "leaf() -> u64"), None, Ids::Exact);
    assert!(signature.runs[Step::Infer as usize] >= 2, "the leaf and its caller: {:?}", signature.runs);
    signature.fresh.expect("every entry is what a fresh run gives");
}

/// The control: with the identity read left out of every trace, the signature edit reaches
/// no caller entry (its body's record and its facts are as they were), and the oracle finds
/// the stale one.
#[test]
fn the_oracle_catches_a_trace_that_drops_the_identity_read() {
    let planted = carried(&LEAF.replace("leaf() -> u32", "leaf() -> u64"), Some(reads::IDENTITY), Ids::Dirty);
    assert_eq!(planted.dirty, [(IDENTITY_NAME, 0)], "no entry has an identity read to post");
    assert!(planted.fresh.is_err(), "a caller entry that read the leaf's signature stayed as it was");
}

/// The delta's control: a signature edit of the leaf changes the callers' identities, and a
/// delta that says `main.rs`'s bodies' identities are unchanged leaves the caller's entries as
/// they were; the oracle finds the stale one. (The same edit with the exact delta is
/// [`a_signature_edit_reaches_the_caller_through_its_identity`].)
#[test]
fn the_oracle_catches_an_identity_delta_that_leaves_a_change_out() {
    let planted = carried(&LEAF.replace("leaf() -> u32", "leaf() -> u64"), None, Ids::MainLeftOut);
    assert!(planted.dirty.is_empty(), "{:?}", planted.dirty);
    assert!(planted.fresh.is_err(), "the caller's inference, which names the leaf's type, stayed as it was");
}

/// The leaf crate's struct as the first pin has it, and with the field it gains.
const SIZED: &str = "pub struct S { pub a: u32 }\n";
const GROWN: &str = "pub struct S { pub a: u32, pub b: u64 }\n";

/// A carry across the leaf crate's struct gaining a field, every body's stand-in MIR having
/// read the struct's size ([`fake_mir`]) through its layout row; `dropped` is a read kind left
/// out of every trace of the first pin. No body's text, identity or impl changes: only what
/// decides the layout does, which no delta gives yet (always dirty, counted by name).
fn layout_carried(dropped: Option<ReadKind>) -> Carried {
    let kernel = kernel();
    let mut tree = Tree::new();
    tree.edit("leaf.rs", &format!("{LEAF}{SIZED}"));
    let mut identities = Identities::default();
    identities.delta(&tree, &tree.keys).expect("the first pin's identities");
    let mut store = Memory::default();
    let mut session = Session::new(&tree);
    DROPPED.with(|it| it.set(dropped));
    let before = ask_all(&kernel, &mut session, &mut store, &tree);
    DROPPED.with(|it| it.set(None));
    assert_eq!(before.len(), 4, "four bodies");
    let from = tree.pin();
    tree.edit("leaf.rs", &format!("{LEAF}{GROWN}"));
    let mut changed = Changed::between(from, tree.pin());
    changed.differs(Read { kind: source::FILE, key: b"leaf.rs" });
    changed.exact(source::EXISTS);
    changed.exact(source::CFG);
    let differ = identities.delta(&tree, &tree.keys).expect("the identity delta");
    assert!(differ.is_empty(), "a struct's field is in no body's identity here: {differ:?}");
    let delta = BodyDelta {
        exact: vec![(ReadKind(IMPLS.kind.0), Vec::new())],
        unknown: vec![(ReadKind(LAYOUT.kind.0), "layout")],
        identities: Some(differ),
    };
    delta.say::<Tree>(&mut changed);
    let dirty = always_dirty(&mut session, &store, &delta).unwrap();
    reset_runs();
    let carry = kernel.carry(&mut session, &mut store, &tree, &changed, &Setting::OFF).unwrap();
    assert_eq!(carry.dropped, None, "{carry:?}");
    let after = ask_all(&kernel, &mut session, &mut store, &tree);
    let runs = runs();
    let fresh = differs(&after, &fresh(&kernel, &tree));
    Carried { runs, dirty, fresh }
}

/// The invariant: a body's MIR entry that read a type's layout holds only while what decides
/// that layout does. The layout row is read through (`Classified::Through`), so the entry's
/// trace has the fact of the struct's field types the row read; the struct gains a field, and
/// every body's MIR entry (each read the struct's size) misses and runs again, with no
/// inference run.
#[test]
fn a_struct_that_gains_a_field_misses_the_mir_entries_that_read_its_size() {
    let grown = layout_carried(None);
    assert_eq!(grown.dirty, [("layout", 4)], "each body's MIR entry read the layout's fact");
    assert_eq!(grown.runs[Step::Infer as usize], 0, "no inference names the struct: {:?}", grown.runs);
    assert_eq!(grown.runs[Step::Mir as usize], 4, "each MIR entry read the struct's size: {:?}", grown.runs);
    grown.fresh.expect("every entry is what a fresh run gives");
}

/// The control: with the layout's fact left out of every trace (its value in no entry's
/// digest), the grown struct reaches no MIR entry, each wrongly hits with the old size in its
/// answer, and the oracle finds the stale one.
#[test]
fn the_oracle_catches_a_trace_that_drops_the_layout_fact() {
    let planted = layout_carried(Some(ReadKind(LAYOUT.kind.0)));
    assert_eq!(planted.dirty, [("layout", 0)], "no entry has a layout fact to post");
    assert_eq!(planted.runs[Step::Mir as usize], 0, "no MIR entry was reached: {:?}", planted.runs);
    assert!(planted.fresh.is_err(), "a MIR entry that read the struct's size stayed as it was");
}

/// A hit in a second session: a new database of the same files and a new session over the
/// same store find every entry without running one, each the bytes the first session kept.
#[test]
fn a_second_session_with_a_new_database_hits_every_entry() {
    let kernel = kernel();
    let mut store = Memory::default();
    let first = {
        let tree = Tree::new();
        let mut session = Session::new(&tree);
        ask_all(&kernel, &mut session, &mut store, &tree)
    };
    let tree = Tree::new();
    let mut session = Session::new(&tree);
    reset_runs();
    let second = ask_all(&kernel, &mut session, &mut store, &tree);
    assert_eq!(runs(), [0; Step::COUNT], "no entry runs: each is the store's");
    differs(&second, &first).expect("every entry is the first session's");
}

/// Every body's three entries got in `session`, each served to the tree's database
/// (`entries::serve`), as the driver serves every entry it gets.
fn ask_and_serve(kernel: &Kernel<Tree>, session: &mut Session, store: &mut Memory, tree: &Tree) {
    // clone: the test's keys, so the tree is lent whole
    let keys = tree.keys.clone();
    let off = &Setting::OFF;
    for key in &keys {
        let got = kernel.get_in::<InferEntry<Tree>, _>(session, store, tree, key, off).unwrap();
        serve(tree, key, Step::Infer, &got).unwrap();
        let got = kernel.get_in::<MirEntry<Tree>, _>(session, store, tree, key, off).unwrap();
        serve(tree, key, Step::Mir, &got).unwrap();
        let got = kernel.get_in::<BorrowckEntry<Tree>, _>(session, store, tree, key, off).unwrap();
        serve(tree, key, Step::Borrowck, &got).unwrap();
    }
}

/// A first session keeps every entry in a store; a second tree, with a new database and item
/// store, gets them in a new session (each a hit, nothing run) and serves them; with
/// `moved`, its database then moves to another revision (a comment edit, which changes no
/// identity and no fact). Then each body's inference and borrow check is asked of the second
/// database: what its item store counted of those, by step.
fn answered_in_a_second_database(moved: bool) -> [StepCounts; Step::COUNT] {
    let kernel = kernel();
    let mut store = Memory::default();
    let first = Tree::new();
    {
        let mut session = Session::new(&first);
        ask_all(&kernel, &mut session, &mut store, &first);
    }
    // clone: a counted reference
    let mut second = Tree::with_defs(first.defs.clone());
    {
        let mut session = Session::new(&second);
        reset_runs();
        ask_and_serve(&kernel, &mut session, &mut store, &second);
        assert_eq!(runs(), [0; Step::COUNT], "no entry runs in the second session: each is the store's");
    }
    if moved {
        second.edit("leaf.rs", &format!("{LEAF}// a comment\n"));
    }
    let db = second.db();
    for &def in second.bodies.values() {
        let _ = InferenceResult::of(db, def);
        let _ = lookup_passed(db, def, CHECKER, || panic!("no store here verifies"));
    }
    second.installed.errors().expect("no served answer disagrees with the database");
    let steps = second.installed.steps();
    let flushed = second.installed.flush().unwrap();
    assert_eq!(flushed.written, 0, "a served step offers nothing to the item store's flush: {flushed:?}");
    steps
}

#[test]
fn a_query_in_a_second_database_answers_from_the_kernels_entries_without_running() {
    let steps = answered_in_a_second_database(false);
    let bodies = 4;
    assert_eq!(
        steps[Step::Infer as usize],
        StepCounts { hits: bodies, ran: 0, refused: 0 },
        "each body's inference is the kernel's entry, with no inference run"
    );
    assert_eq!(steps[Step::Borrowck as usize].hits, bodies, "each borrow check is the kernel's entry");
}

/// The invariant: a served answer is the query's answer only at the revision it was posted
/// at; at any other, the step runs. `Err` names the steps that answered from another
/// revision's posts.
fn served_only_at_its_revision() -> Result<(), String> {
    let steps = answered_in_a_second_database(true);
    let (infer, check) = (steps[Step::Infer as usize].hits, steps[Step::Borrowck as usize].hits);
    match (infer, check) {
        (0, 0) => Ok(()),
        _ => Err(format!("{infer} inferences and {check} borrow checks answered at another revision")),
    }
}

#[test]
fn a_served_answer_is_used_only_at_the_revision_it_was_posted_at() {
    served_only_at_its_revision().unwrap();
}

/// The control: a store that gives a served answer at any revision (`ANY_REVISION`) must be
/// caught.
#[test]
fn the_oracle_catches_a_served_answer_used_at_another_revision() {
    ANY_REVISION.with(|it| it.set(true));
    let caught = served_only_at_its_revision();
    ANY_REVISION.with(|it| it.set(false));
    assert!(caught.is_err(), "the planted bug went unseen");
}

/// The served steps' lookup of the item store's own records is gone: the one record lookup
/// left is a constant's value's (`Store::find_constant`).
#[test]
fn the_served_steps_have_no_lookup_of_the_item_stores_records() {
    let source = include_str!("../../body_store.rs");
    assert_eq!(source.matches("blaze_record::lookup(").count(), 1, "one record lookup");
    let at = source.find("blaze_record::lookup(").unwrap();
    let constant = source.find("fn find_constant(").expect("the constant's lookup");
    let next = source[constant + 1..].find("\n    fn ").map_or(source.len(), |it| it + constant + 1);
    assert!((constant..next).contains(&at), "the record lookup is the constant's");
    for gone in ["fn find(", "self.find("] {
        assert!(!source.contains(gone), "`{gone}` is deleted");
    }
}

/// Rule 21, the solver's code: a body's entries kept by the front end before an edit of the
/// solver's source (each front-end identity as blaze-engine's build script works it out,
/// `blaze_codegen_code::FRONT_END`, the code the driver registers the entries with) miss in a
/// kernel of the front end after it, and are found again by one of the front end that kept
/// them.
#[test]
fn an_entry_kept_by_one_solver_misses_after_a_solver_source_edit() {
    let (before, after) = blaze_codegen_code::fixture::front_end_around_a_solver_edit();
    let (before, after) = (ContentId(before), ContentId(after));
    assert_ne!(before, after, "an edit of the solver's source is another front-end identity");
    let mut store = Memory::default();
    {
        let tree = Tree::new();
        let mut session = Session::new(&tree);
        ask_all(&kernel_of(before), &mut session, &mut store, &tree);
    }
    for (build, ran, what) in [(after, true, "the edited solver finds none"), (before, false, "the solver that kept them finds each")] {
        let tree = Tree::new();
        let mut session = Session::new(&tree);
        reset_runs();
        ask_all(&kernel_of(build), &mut session, &mut store, &tree);
        assert_eq!(runs()[Step::Infer as usize] > 0, ran, "{what}: {:?}", runs());
    }
}
