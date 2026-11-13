//! Body entries carried through the kernel (end-state.md, order row 3): a two-crate program's
//! bodies, each with its inference, MIR and borrow check entries in one session, carried
//! across an edit of the leaf crate's file with the change set a driver gives (the file's text
//! differs; the impls fact from the impls maintainer's last write; the identities that differ
//! from a test's column, each exact; every other kind valued, or every kind valued). Each test
//! counts the entries' runs and checks every entry against a fresh run (the oracle); one
//! control drops the identity read from every trace, another leaves a changed identity out of
//! the column, and the oracle must catch the stale entry each leaves. A second session over the
//! same store, with a new database, hits every entry; a row the carry did not reach answers at
//! the next revision under its crate's stamp.
//!
//! The map from a file's syntax to the database's bodies is the driver's ([`keyed`],
//! [`written`], [`join`]): the fixture has an `impl`'s method and a trait's default method,
//! keyed by where they are written as a function at a file's top is. A claim of the runs to
//! keep lets go when its keep fails, and a served borrow check's row depends on what its MIR
//! read.
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
    ITEMS, Reading, Table,
    entry::{Entry, ItemId, Kind},
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
    signatures::{ConstSignature, FunctionSignature},
};
use hir_expand::fixture::{self, Fixture};
use std::alloc::Global;
use syntax::{Edition, Extensions};

use super::{
    ANY_KIND, ANY_REVISION, BodyDelta, answering, BodyKey, BodyWorld, BorrowckEntry, DROPPED, Exact, HELD,
    How, InferEntry, KEEP_ALL, Keyed, MirEntry, OWN_FACTS_ONLY, Of, OwnerKey, Place, RUNS,
    Written, borrowck_schema, claim_keeps, fact_read, join, key_of, keyed, places, reads,
    register, run_infer, serve, stamp, unpost, written,
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
pub struct T;
impl T { pub fn method(&self) -> u32 { 5 } }
pub trait D { fn default_method(&self) -> u32 { 6 } }
"#;

/// The files as the fixture has them, by stable name, each with its crate, in fixture order.
const FILES: [(&str, &str); 2] = [("leaf.rs", "leaf"), ("main.rs", "main")];
const LEAF: &str = "pub fn leaf() -> u32 { 1 }\npub fn other() -> u32 { 2 }\n";
const MAIN: &str = "pub fn caller() -> u32 { let _x = leaf::leaf(); 1 }\npub fn twice() -> u32 { caller() + caller() }\npub struct T;\nimpl T { pub fn method(&self) -> u32 { 5 } }\npub trait D { fn default_method(&self) -> u32 { 6 } }\n";

/// How many bodies the fixture has: two functions in each file, an `impl`'s method and a
/// trait's default method.
const BODIES: usize = 6;

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

/// [`LAYOUT`]'s value: how many fields each struct of the leaf crate has, and each variant of
/// each of its enums, in declaration order; none where it has no struct or enum. What a read
/// of any of their field types below a layout row stands for here.
fn layout(db: &Db) -> Option<ContentId> {
    let def_map = crate_def_map(db, crate_named(db, "leaf"));
    let mut h = Hasher::new("blaze v2 body entries test layout");
    let mut any = false;
    for (_, module) in def_map.modules(db) {
        for it in module.scope(db).declarations() {
            // alloc: a test's fact value, one list per ADT
            let variants: Vec<VariantId> = match it {
                ModuleDefId::AdtId(AdtId::StructId(it)) => vec![it.into()],
                ModuleDefId::AdtId(AdtId::EnumId(it)) => {
                    crate::layout::enum_variant_ids(db, it).iter().map(|&v| v.into()).collect()
                }
                _ => continue,
            };
            any = true;
            h.update(&(variants.len() as u64).to_le_bytes());
            for variant in variants {
                let fields = crate::layout::variant_field_ids(db, variant).len() as u64;
                h.update(&fields.to_le_bytes());
            }
        }
    }
    any.then(|| h.finish())
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

    /// The bodies of every file as the driver maps them: the file's body parts by where they
    /// start ([`keyed`]), joined with where each body of its crate is written ([`written`],
    /// [`join`]).
    fn map(&mut self) {
        self.bodies.clear();
        self.keys.clear();
        let db = self.fixture.db();
        for (at, (name, krate)) in FILES.into_iter().enumerate() {
            let file = self.fixture.file(at).file_id(db);
            let keyed = keyed_of(&self.files[name]);
            for (item, def) in join(&keyed, &written_in(db, krate, file)) {
                self.bodies.insert((name.to_owned(), item), def);
                let file = FileKey { name: name.to_owned(), reading: reading() };
                self.keys.push(BodyKey { file, item, krate: krate.to_owned() });
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

/// The body parts of `text`'s `items`, by where they start ([`keyed`]).
fn keyed_of(text: &str) -> Vec<Keyed> {
    let record = blaze_front_items::parse(text, reading());
    let parsed = blaze_front_items::tree(&record, text, reading()).expect("a tree");
    let items = blaze_front_items::file_items(parsed.root()).expect("items");
    keyed(&items.items, &items.lines).expect("the file's bodies")
}

/// The bodies of the crate `krate` written in `file`, each where ([`written`]).
fn written_in(db: &Db, krate: &str, file: base_db::FileId) -> Vec<(DefWithBodyId, Written)> {
    crate_body_owners(db, crate_named(db, krate))
        .into_iter()
        .filter_map(|def| Some((def, written(db, def)?)))
        .filter(|(_, at)| at.file == file)
        .collect()
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

    fn run(&self, def: DefWithBodyId, of: Of) -> Result<SchemaHash, String> {
        let db = self.db();
        match of {
            Of::Infer => Ok(run_infer(db, def)),
            // The stand-in MIR is the body's alone: its owners are the body (the fixture's
            // bodies have no closure).
            Of::Mir(Place::BODY) => {
                let _ = fake_mir(db, def);
                Ok(MIR_SCHEMA)
            }
            Of::Mir(place) => Err(format!("no stand-in MIR at {place:?}")),
            Of::Borrowck => match fake_borrowck(db, def) {
                true => Ok(borrowck_schema(db, def, CHECKER)),
                false => Err(String::from("its borrow check fails")),
            },
        }
    }

    fn schema(&self, def: DefWithBodyId, of: Of) -> Result<SchemaHash, String> {
        match of {
            Of::Infer => Ok(infer::SCHEMA),
            Of::Mir(Place::BODY) => Ok(MIR_SCHEMA),
            Of::Mir(place) => Err(format!("no stand-in MIR at {place:?}")),
            Of::Borrowck => Ok(borrowck_schema(self.db(), def, CHECKER)),
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

/// The MIR entries' inputs of the body `key`: one per place of its owners ([`places`]).
fn owners(tree: &Tree, key: &BodyKey) -> Vec<OwnerKey> {
    let def = tree.body(key).expect("a body of the tree");
    // clone: the body's key, one owner's input each
    places(tree.db(), def).into_iter().map(|place| OwnerKey { body: key.clone(), place }).collect()
}

/// Every body's entries in `session`, by the body's key: `(infer, mir, borrowck)`, the MIR
/// each owner's entry's place and value in turn.
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
            let mut mir = Vec::new();
            for owner in owners(tree, key) {
                mir.extend_from_slice(&owner.place.bytes());
                mir.extend_from_slice(kernel.get_in::<MirEntry<Tree>, _>(session, store, tree, &owner, off).unwrap().value());
            }
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
    /// What the kernel's carry counted (`valued`, `differed`, ...).
    carry: blaze_kernel::Carried,
    /// Each kind the session read, as [`BodyDelta::say`] said it.
    said: Vec<(ReadKind, How)>,
    /// The distinct layout reads the session had before the carry ([`Session::names_of`]).
    layout: usize,
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

/// How a carry's change set says what differs.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Says {
    /// No producer: every kind the session reads is valued by the kernel.
    Valued,
    /// The test's producers: the edited file, the impls maintainer's last write and the
    /// bodies whose identity differs between the pins, each exact; every other kind valued.
    Exact,
    /// The planted bug of the producer's control: [`Says::Exact`] with every body of `main.rs`
    /// left out of the identity column, as a producer that said a changed identity unchanged.
    MainLeftOut,
}

/// Each body's identity at `tree`'s pin, by its key's bytes ([`key_of`], what its identity
/// read is keyed by), with its file: what a producer of the identity column keeps from a pin to
/// the next.
fn identities(tree: &Tree) -> BTreeMap<Vec<u8>, (String, Option<ContentId>)> {
    let db = tree.db();
    tree.keys
        .iter()
        .map(|key| {
            let def = tree.body(key).expect("a mapped body");
            let identity = crate::body_store::with_store(db, |store| store.facts.identity(db, def))
                .expect("the test database's item store");
            let mut bytes = Vec::new();
            key_of::<Tree>(key, &mut bytes);
            // clone: a body's file name, kept with its identity
            (bytes, (key.file.name.clone(), identity))
        })
        .collect()
}

/// The producers' word on a carry of `tree` from the pin whose identities were `before`, as
/// `says` gives it.
fn delta(tree: &mut Tree, before: &BTreeMap<Vec<u8>, (String, Option<ContentId>)>, says: Says) -> BodyDelta {
    if says == Says::Valued {
        return BodyDelta::default();
    }
    let after = identities(tree);
    let mut differ: Vec<Vec<u8>> = Vec::new();
    for (bytes, (file, identity)) in &after {
        let moved = before.get(bytes).is_none_or(|(_, was)| was != identity);
        if moved && !(says == Says::MainLeftOut && file == "main.rs") {
            // clone: one body of the column
            differ.push(bytes.clone());
        }
    }
    // clone: each body gone, of the column
    differ.extend(before.keys().filter(|it| !after.contains_key(*it)).cloned());
    // The impls fact, from the impls maintainer's last write: which crate a fact is seen from
    // is the reader's, and both crates see the impls of both.
    let mut impls = Vec::new();
    let mut impls_kind = ReadKind(IMPLS.kind.0);
    if tree.impls_written() {
        for (_, krate) in FILES {
            let mut key = Vec::new();
            impls_kind = fact_read(IMPLS, krate, &mut key);
            impls.push(key);
        }
    }
    let by = "a test's column";
    BodyDelta {
        exact: vec![
            Exact { kind: source::FILE, by, keys: vec![b"leaf.rs".to_vec()] },
            Exact { kind: source::EXISTS, by, keys: Vec::new() },
            Exact { kind: source::CFG, by, keys: Vec::new() },
            Exact { kind: reads::IDENTITY, by, keys: differ },
            Exact { kind: impls_kind, by, keys: impls },
        ],
    }
}

/// A carry of `leaf.rs` from `first` to `then`, every entry asked before and after; `dropped`
/// is a read kind left out of every trace of the first pin.
fn carried_from(first: Option<&str>, then: &str, dropped: Option<ReadKind>, says: Says) -> Carried {
    let kernel = kernel();
    let mut tree = Tree::new();
    if let Some(first) = first {
        tree.edit("leaf.rs", first);
    }
    let before_ids = identities(&tree);
    let mut store = Memory::default();
    let mut session = Session::new(&tree);
    DROPPED.with(|it| it.set(dropped));
    let before = ask_all(&kernel, &mut session, &mut store, &tree);
    DROPPED.with(|it| it.set(None));
    assert_eq!(before.len(), BODIES, "every body of the fixture");
    let layout = session.names_of(&store, ReadKind(LAYOUT.kind.0)).unwrap();
    let from = tree.pin();
    tree.edit("leaf.rs", then);
    let delta = delta(&mut tree, &before_ids, says);
    let mut changed = Changed::between(from, tree.pin());
    let said = delta.say(&session, &mut changed);
    reset_runs();
    let carry = kernel.carry(&mut session, &mut store, &tree, &changed, &Setting::OFF).unwrap();
    assert_eq!(carry.dropped, None, "{carry:?}");
    let after = ask_all(&kernel, &mut session, &mut store, &tree);
    let runs = runs();
    let fresh = differs(&after, &fresh(&kernel, &tree));
    Carried { runs, carry, said, layout, fresh }
}

fn carried(edited: &str, dropped: Option<ReadKind>, says: Says) -> Carried {
    carried_from(None, edited, dropped, says)
}

/// How `carried` said `kind`, if the session read it.
fn said(carried: &Carried, kind: ReadKind) -> Option<How> {
    carried.said.iter().find(|(it, _)| *it == kind).map(|(_, how)| *how)
}

/// The floor of a leaf body edit: the edited body's three entries, and nothing else. The
/// identity column is exact: no caller's identity read is valued.
#[test]
fn a_leaf_body_edit_reruns_its_three_entries_and_no_callers() {
    let leaf = carried(&LEAF.replace("{ 1 }", "{ 3 }"), None, Says::Exact);
    // By `Step` as an index: no constant and no lowering is a kernel entry, so neither runs one.
    assert_eq!(leaf.runs, [1, 1, 1, 0, 0], "the edited body's inference, MIR and borrow check, once each");
    assert_eq!(said(&leaf, reads::IDENTITY), Some(How::Exact("a test's column")), "{:?}", leaf.said);
    leaf.fresh.expect("every entry is what a fresh run gives");
}

/// The invariant: a carry whose producers say some kinds exactly ends where a carry that
/// values every read ends: the same runs, and every entry what a fresh run gives, for a body
/// edit, a comment edit and a signature edit. Valuing costs one valuation a distinct read
/// (each body's identity among them), exactness none. The control is
/// [`the_oracle_catches_a_producer_that_leaves_a_change_out`].
#[test]
fn a_producers_exact_delta_ends_where_valuing_every_read_ends() {
    for edited in [
        LEAF.replace("{ 1 }", "{ 3 }"),
        format!("// a comment\n{LEAF}"),
        LEAF.replace("leaf() -> u32", "leaf() -> u64"),
    ] {
        let valued = carried(&edited, None, Says::Valued);
        let exact = carried(&edited, None, Says::Exact);
        assert_eq!(valued.runs, exact.runs, "the same entries rerun either way");
        assert_eq!(said(&valued, reads::IDENTITY), Some(How::Valued), "{:?}", valued.said);
        assert!(
            valued.carry.valued >= exact.carry.valued + BODIES,
            "valuing reads each body's identity once: {:?} against {:?}",
            valued.carry,
            exact.carry
        );
        assert!(valued.carry.differed <= valued.carry.valued, "{:?}", valued.carry);
        valued.fresh.expect("every valued entry is what a fresh run gives");
        exact.fresh.expect("every exact entry is what a fresh run gives");
    }
}

/// The floor of a comment edit: no entry.
#[test]
fn a_comment_edit_reruns_no_entry() {
    let comment = carried(&format!("// a comment\n{LEAF}"), None, Says::Exact);
    assert_eq!(comment.runs, [0; Step::COUNT], "a comment changes no body's record");
    comment.fresh.expect("every entry is what a fresh run gives");
}

/// A signature edit of the leaf reaches its caller through the caller's identity read: the
/// caller's inference names the leaf, whose type is other now.
#[test]
fn a_signature_edit_reaches_the_caller_through_its_identity() {
    let signature = carried(&LEAF.replace("leaf() -> u32", "leaf() -> u64"), None, Says::Exact);
    assert!(signature.runs[Step::Infer as usize] >= 2, "the leaf and its caller: {:?}", signature.runs);
    signature.fresh.expect("every entry is what a fresh run gives");
}

/// The control: with the identity read left out of every trace, the session reads no identity
/// (no kind to say), the signature edit reaches no caller entry (its body's record and its
/// facts are as they were), and the oracle finds the stale one, every read valued.
#[test]
fn the_oracle_catches_a_trace_that_drops_the_identity_read() {
    let planted = carried(&LEAF.replace("leaf() -> u32", "leaf() -> u64"), Some(reads::IDENTITY), Says::Valued);
    assert_eq!(said(&planted, reads::IDENTITY), None, "no entry has an identity read to post");
    assert!(planted.fresh.is_err(), "a caller entry that read the leaf's signature stayed as it was");
}

/// The producer's control: a signature edit of the leaf changes the callers' identities, and a
/// producer that says `main.rs`'s bodies' identities are unchanged leaves the caller's entries
/// as they were; the oracle finds the stale one. (The same edit with the whole column is
/// [`a_signature_edit_reaches_the_caller_through_its_identity`].)
#[test]
fn the_oracle_catches_a_producer_that_leaves_a_change_out() {
    let planted = carried(&LEAF.replace("leaf() -> u32", "leaf() -> u64"), None, Says::MainLeftOut);
    assert!(planted.fresh.is_err(), "the caller's inference, which names the leaf's type, stayed as it was");
}

/// The leaf crate's struct as the first pin has it, and with the field it gains.
const SIZED: &str = "pub struct S { pub a: u32 }\n";
const GROWN: &str = "pub struct S { pub a: u32, pub b: u64 }\n";

/// A carry across the leaf crate's struct gaining a field, every body's stand-in MIR having
/// read the struct's size ([`fake_mir`]) through its layout row; `dropped` is a read kind left
/// out of every trace of the first pin. No body's text, identity or impl changes: only what
/// decides the layout does, which no producer gives: it is valued.
fn layout_carried(dropped: Option<ReadKind>) -> Carried {
    adt_carried(&format!("{LEAF}{SIZED}"), &format!("{LEAF}{GROWN}"), dropped)
}

/// A carry of `leaf.rs` from `first` to `then`, every body's stand-in MIR having read the leaf
/// crate's struct's size ([`fake_mir`]) through its layout row; `dropped` is a read kind left
/// out of every trace of the first pin. The layout kind is valued, every other said by the
/// test's producers.
fn adt_carried(first: &str, then: &str, dropped: Option<ReadKind>) -> Carried {
    carried_from(Some(first), then, dropped, Says::Exact)
}

/// The invariant: a body's MIR entry that read a type's layout holds only while what decides
/// that layout does. The layout row is read through (`Classified::Through`), so the entry's
/// trace has the fact of the struct's field types the row read; the struct gains a field, and
/// every body's MIR entry (each read the struct's size) misses and runs again, with no
/// inference run.
#[test]
fn a_struct_that_gains_a_field_misses_the_mir_entries_that_read_its_size() {
    let grown = layout_carried(None);
    assert!(grown.layout > 0, "each body's MIR entry read the layout's fact");
    assert_eq!(said(&grown, ReadKind(LAYOUT.kind.0)), Some(How::Valued), "{:?}", grown.said);
    assert_eq!(grown.runs[Step::Infer as usize], 0, "no inference names the struct: {:?}", grown.runs);
    assert_eq!(grown.runs[Step::Mir as usize], BODIES as u32, "each MIR entry read the struct's size: {:?}", grown.runs);
    grown.fresh.expect("every entry is what a fresh run gives");
}

/// The control: with the layout's fact left out of every trace (its value in no entry's
/// digest), the grown struct reaches no MIR entry, each wrongly hits with the old size in its
/// answer, and the oracle finds the stale one.
#[test]
fn the_oracle_catches_a_trace_that_drops_the_layout_fact() {
    let planted = layout_carried(Some(ReadKind(LAYOUT.kind.0)));
    assert_eq!(planted.layout, 0, "no entry has a layout fact to post");
    assert_eq!(planted.runs[Step::Mir as usize], 0, "no MIR entry was reached: {:?}", planted.runs);
    assert!(planted.fresh.is_err(), "a MIR entry that read the struct's size stayed as it was");
}

/// The leaf crate's enum, read by every stand-in MIR through the struct that holds one (the
/// struct's layout reads the enum's), as the first pin has it and with a field its variant
/// gains.
const ENUMED: &str = "pub enum E { A, B(u8) }\npub struct S { pub a: E }\n";
const ENUM_GROWN: &str = "pub enum E { A, B(u8, u64) }\npub struct S { pub a: E }\n";

/// The invariant: a body's MIR entry that read a struct's layout is kept across an edit that
/// leaves what decides the layout as it was. Another body of the leaf crate is edited: its
/// three entries run again, and every MIR entry that read the struct's size hits, its layout
/// fact valued again and found the same. The control is
/// [`a_struct_that_gains_a_field_misses_the_mir_entries_that_read_its_size`]: the same trace
/// misses when the struct gains a field.
#[test]
fn a_mir_entry_that_read_a_struct_s_size_is_kept_when_the_struct_is_not_edited() {
    let first = format!("{LEAF}{SIZED}");
    let kept = adt_carried(&first, &first.replace("{ 2 }", "{ 5 }"), None);
    assert!(kept.layout > 0, "each body's MIR entry read the layout's fact");
    assert_eq!(kept.runs, [1, 1, 1, 0, 0], "the edited body's three entries alone: {:?}", kept.runs);
    kept.fresh.expect("every entry is what a fresh run gives");
}

/// The invariant, for an enum: a MIR entry that read a layout an enum decides is kept while the
/// enum is as it was (another body edited), and misses when a variant gains a field, the
/// enum's layout row read through to its variants' field types.
#[test]
fn a_mir_entry_that_read_an_enum_s_layout_is_kept_until_a_variant_gains_a_field() {
    let first = format!("{LEAF}{ENUMED}");
    let kept = adt_carried(&first, &first.replace("{ 2 }", "{ 5 }"), None);
    assert!(kept.layout > 0, "each body's MIR entry read the layout's fact");
    assert_eq!(kept.runs, [1, 1, 1, 0, 0], "the edited body's three entries alone: {:?}", kept.runs);
    kept.fresh.expect("every kept entry is what a fresh run gives");
    let grown = adt_carried(&first, &format!("{LEAF}{ENUM_GROWN}"), None);
    assert_eq!(grown.runs[Step::Infer as usize], 0, "no inference names the enum: {:?}", grown.runs);
    assert_eq!(grown.runs[Step::Mir as usize], BODIES as u32, "each MIR entry read the enum's layout: {:?}", grown.runs);
    grown.fresh.expect("every entry is what a fresh run gives");
}

/// The control for an enum: with the field types' fact left out of every trace, a variant's new
/// field reaches no MIR entry, each wrongly hits with the old size in its answer, and the
/// oracle finds the stale one.
#[test]
fn the_oracle_catches_a_trace_that_drops_an_enum_s_field_types_fact() {
    let planted = adt_carried(&format!("{LEAF}{ENUMED}"), &format!("{LEAF}{ENUM_GROWN}"), Some(ReadKind(LAYOUT.kind.0)));
    assert_eq!(planted.layout, 0, "no entry has a layout fact to post");
    assert_eq!(planted.runs[Step::Mir as usize], 0, "no MIR entry was reached: {:?}", planted.runs);
    assert!(planted.fresh.is_err(), "a MIR entry that read the enum's layout stayed as it was");
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
/// (`entries::serve`), as the driver serves every entry it gets. The MIR owners' places are
/// found in `places_of`'s database (a tree of the same files): finding them infers the body
/// (`mir::owners`), and in `tree`'s database that is a lookup of the row just posted, a hit
/// counted in its item store and a memo of the served answer, so a test that counts what its
/// later asks found, or asks at a later revision, gives a tree other than `tree`.
fn ask_and_serve(kernel: &Kernel<Tree>, session: &mut Session, store: &mut Memory, tree: &Tree, places_of: &Tree) {
    // clone: the test's keys, so the tree is lent whole
    let keys = tree.keys.clone();
    let off = &Setting::OFF;
    for key in &keys {
        let got = kernel.get_in::<InferEntry<Tree>, _>(session, store, tree, key, off).unwrap();
        serve(tree, key, Of::Infer, &got).unwrap();
        for owner in owners(places_of, key) {
            let got = kernel.get_in::<MirEntry<Tree>, _>(session, store, tree, &owner, off).unwrap();
            serve(tree, key, Of::Mir(owner.place), &got).unwrap();
        }
        let got = kernel.get_in::<BorrowckEntry<Tree>, _>(session, store, tree, key, off).unwrap();
        serve(tree, key, Of::Borrowck, &got).unwrap();
    }
}

/// A first session keeps every entry in a store; a second tree, with a new database and item
/// store, gets them in a new session (each a hit, nothing run) and serves them; with
/// `moved`, its database then moves to another revision (a comment edit, which changes no
/// identity and no fact). Then each body's inference and borrow check is asked of the second
/// database: what its item store counted of those asks alone, by step.
///
/// The count is of the asks alone ([`counted_since`]), and the serve infers nothing in the
/// second database (its MIR places are found in the first's, [`ask_and_serve`]): before,
/// finding the places there looked up each inference just posted, six hits at the serve's
/// revision that every later count included, and whose memos then answered the asks with no
/// lookup at all, so a stale row could neither show in the count nor be ruled out by it.
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
        ask_and_serve(&kernel, &mut session, &mut store, &second, &first);
        assert_eq!(runs(), [0; Step::COUNT], "no entry runs in the second session: each is the store's");
    }
    if moved {
        second.edit("leaf.rs", &format!("{LEAF}// a comment\n"));
    }
    let before = second.installed.steps();
    let db = second.db();
    for &def in second.bodies.values() {
        let _ = InferenceResult::of(db, def);
        let _ = lookup_passed(db, def, CHECKER, || panic!("no store here verifies"));
    }
    second.installed.errors().expect("no served answer disagrees with the database");
    let steps = counted_since(before, second.installed.steps());
    let flushed = second.installed.flush().unwrap();
    assert_eq!(flushed.written, 0, "a served step offers nothing to the item store's flush: {flushed:?}");
    steps
}

/// What an item store counted between two reads of its counts (`Installed::steps`, which
/// counts from the last flush): each step's hits, runs and refusals since `before`.
fn counted_since(before: [StepCounts; Step::COUNT], after: [StepCounts; Step::COUNT]) -> [StepCounts; Step::COUNT] {
    std::array::from_fn(|at| StepCounts {
        hits: after[at].hits - before[at].hits,
        ran: after[at].ran - before[at].ran,
        refused: after[at].refused - before[at].refused,
    })
}

/// A second database served every entry (as [`answered_in_a_second_database`]), then
/// `leaf.rs`'s first body edited and the session carried to the new pin; as the driver does,
/// `leaf.rs`'s bodies are unposted (the edit reached their file) and each crate stamped at the
/// new revision since the serve. Then each body's inference and borrow check is asked of the
/// second database. The invariant: every row the carry did not reach (`main.rs`'s bodies)
/// answers at the new revision with no step run, every other runs, and no answer disagrees
/// with the database (`Installed::errors`, which checks each served answer's identity and
/// facts again). And, asked of the rows themselves with no query run ([`answering`]): every
/// row that answers at the new revision was verified with the body's identity now, so a
/// stale row is caught whether or not the engine still holds a memo of its step (the carry
/// runs the edited bodies' steps before the stamp, and their memos would hide a stale row
/// from any count). `Err` names what broke it.
fn carried_rows_answer() -> Result<(), String> {
    let kernel = kernel();
    let mut store = Memory::default();
    let first = Tree::new();
    {
        let mut session = Session::new(&first);
        ask_all(&kernel, &mut session, &mut store, &first);
    }
    // clone: a counted reference
    let mut second = Tree::with_defs(first.defs.clone());
    let mut session = Session::new(&second);
    ask_and_serve(&kernel, &mut session, &mut store, &second, &first);
    let (_, since) = second.db().nonce_and_revision();
    let edited: Vec<DefWithBodyId> =
        second.bodies.iter().filter(|((file, _), _)| file == "leaf.rs").map(|(_, def)| *def).collect();
    let others = BODIES - edited.len();
    let from = second.pin();
    second.edit("leaf.rs", &LEAF.replace("{ 1 }", "{ 3 }"));
    let before = second.installed.steps();
    let mut changed = Changed::between(from, second.pin());
    BodyDelta::default().say(&session, &mut changed);
    let carry = kernel.carry(&mut session, &mut store, &second, &changed, &Setting::OFF).map_err(|it| it.to_string())?;
    if carry.dropped.is_some() {
        return Err(format!("the carry dropped the session: {carry:?}"));
    }
    let db = second.db();
    for &def in &edited {
        unpost(db, def);
    }
    for (_, krate) in FILES {
        stamp(db, crate_named(db, krate), since);
    }
    for &def in second.bodies.values() {
        let now = crate::body_store::with_store(db, |store| store.facts.identity(db, def))
            .ok_or("the test database has an item store")?;
        for (step, schema) in [(Step::Infer, super::infer_schema()), (Step::Borrowck, borrowck_schema(db, def, CHECKER))] {
            if let Some(verified) = answering(db, def, step, schema)
                && Some(verified) != now
            {
                return Err(format!(
                    "{def:?}'s {step:?} row answers at the new revision, verified with identity {verified:x}; the body's is {now:?}"
                ));
            }
        }
    }
    reset_runs();
    for &def in second.bodies.values() {
        let _ = InferenceResult::of(db, def);
        let _ = lookup_passed(db, def, CHECKER, || panic!("no store here verifies"));
    }
    second.installed.errors().map_err(|it| format!("a served answer disagrees with the database: {it:?}"))?;
    let infer = counted_since(before, second.installed.steps())[Step::Infer as usize];
    if (infer.hits, infer.ran) != (others as u64, edited.len() as u64) {
        return Err(format!(
            "{} inferences answered from rows and {} ran, of {others} the edit did not reach and {} it did",
            infer.hits,
            infer.ran,
            edited.len()
        ));
    }
    Ok(())
}

#[test]
fn a_row_the_carry_did_not_reach_answers_at_the_next_revision() {
    carried_rows_answer().unwrap();
}

/// The control: a driver whose unpost drops nothing (`KEEP_ALL`) leaves the edited bodies'
/// rows under the stamp, and they answer for bodies whose identity moved: caught.
#[test]
fn the_oracle_catches_a_stamp_over_a_moved_entrys_row() {
    KEEP_ALL.with(|it| it.set(true));
    let planted = carried_rows_answer();
    KEEP_ALL.with(|it| it.set(false));
    assert!(planted.is_err(), "an edited body's row answered under the stamp");
}

#[test]
fn a_query_in_a_second_database_answers_from_the_kernels_entries_without_running() {
    let steps = answered_in_a_second_database(false);
    let bodies = BODIES as u64;
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

/// Whether `def` has a body written (a trait's method or `const` may declare none).
fn has_body(db: &Db, def: DefWithBodyId) -> bool {
    match def {
        DefWithBodyId::FunctionId(it) => FunctionSignature::of(db, it).has_body(),
        DefWithBodyId::ConstId(it) => ConstSignature::of(db, it).has_body(),
        DefWithBodyId::StaticId(_) | DefWithBodyId::VariantId(_) => true,
    }
}

/// `keyed` with a decoy at every key's start: an item of a kind no body is, sorted before it,
/// as a file whose items share a start would have. A join that ignores the kind pairs a body
/// with its decoy.
fn with_decoys(keyed: Vec<Keyed>) -> Vec<Keyed> {
    let mut out = Vec::with_capacity(2 * keyed.len());
    for (n, it) in keyed.iter().enumerate() {
        out.push(Keyed { item: ItemId([0xd0 ^ n as u8; 16]), kind: Kind::File, start: it.start });
    }
    out.extend(keyed);
    out.sort_unstable_by_key(|it| (it.start, it.kind));
    out
}

/// The invariant: every body `crate_body_owners` gives that is written in a pinned file has
/// one key, whose body is it: the key's item is in the file's `items` with the body's kind
/// and, for a function, its name. Run with a decoy at every key's start ([`with_decoys`]).
/// `Err` names the first body that breaks it.
fn keyed_by_where_written() -> Result<usize, String> {
    let tree = Tree::new();
    let db = tree.db();
    let mut checked = 0;
    for (at, (name, krate)) in FILES.into_iter().enumerate() {
        let text = &tree.files[name];
        let record = blaze_front_items::parse(text, reading());
        let parsed = blaze_front_items::tree(&record, text, reading()).expect("a tree");
        let items = blaze_front_items::file_items(parsed.root()).expect("items");
        let table = Table::read(ITEMS, &items.items).expect("`items` reads");
        let file = tree.fixture.file(at).file_id(db);
        let bodies = written_in(db, krate, file);
        let pairs = join(&with_decoys(keyed(&items.items, &items.lines).expect("the file's bodies")), &bodies);
        for def in crate_body_owners(db, crate_named(db, krate)) {
            let Some(written_at) = written(db, def).filter(|it| it.file == file) else { continue };
            if !has_body(db, def) {
                continue;
            }
            checked += 1;
            let keys: Vec<ItemId> = pairs.iter().filter(|(_, it)| *it == def).map(|(item, _)| *item).collect();
            let [item] = keys.as_slice() else {
                return Err(format!("{def:?} in `{name}` has {} keys", keys.len()));
            };
            let item = *item;
            let entry = match table.get(&item.0) {
                Ok(Some(bytes)) => Entry::read(bytes)?,
                _ => return Err(format!("{def:?} in `{name}` is keyed by {item:?}, which `items` has no entry of")),
            };
            if entry.kind != written_at.kind {
                return Err(format!("{def:?} in `{name}` is keyed by a {:?}", entry.kind));
            }
            if let DefWithBodyId::FunctionId(it) = def
                && FunctionSignature::of(db, it).name.as_str() != entry.name
            {
                return Err(format!("{def:?} in `{name}` is keyed by the item `{}`", entry.name));
            }
        }
    }
    Ok(checked)
}

#[test]
fn an_impl_and_a_trait_body_are_keyed_by_where_they_are_written() {
    let checked = keyed_by_where_written().unwrap();
    assert_eq!(checked, BODIES, "every body of the fixture, the method and the default method too");
    let tree = Tree::new();
    assert_eq!(tree.keys.len(), BODIES, "the tree keys each body once");
}

/// The control: a join that ignores the kind ([`ANY_KIND`]) pairs a body with the decoy at its
/// start, and the oracle finds it.
#[test]
fn the_oracle_catches_a_join_that_ignores_the_kind() {
    ANY_KIND.with(|it| it.set(true));
    let caught = keyed_by_where_written();
    ANY_KIND.with(|it| it.set(false));
    assert!(caught.is_err(), "the planted bug went unseen: {caught:?}");
}

/// Every served step's trace of `db` the item store holds, by body, step and schema.
fn traced(db: &Db) -> Vec<(DefWithBodyId, Step, SchemaHash)> {
    super::super::with_store(db, |store| {
        store.traces.lock().keys().copied().filter(|(_, step, _)| step.served()).collect()
    })
    .expect("the tree's item store")
}

/// The invariant: every traced run is an entry in the store or is claimed by the next
/// `claim_keeps`. Every body's three steps run in the tree's database; a claim takes them
/// all, and its keep puts the first body's three entries in the store and then fails (the
/// claim is dropped, not kept); the next claim must take every other run. Kept, that claim
/// leaves nothing to a third. `Err` names the runs no one holds.
fn left_to_the_next() -> Result<(), String> {
    let kernel = kernel();
    let tree = Tree::new();
    let db = tree.db();
    for &def in tree.bodies.values() {
        let _ = run_infer(db, def);
        let _ = fake_mir(db, def);
        let _ = fake_borrowck(db, def);
    }
    let runs = traced(db);
    assert_eq!(runs.len(), 3 * BODIES, "each body's inference, MIR and borrow check");
    let claim = claim_keeps(db).expect("an item store");
    assert_eq!(claim.steps.len(), runs.len(), "the first claim takes every run");
    assert!(claim.refused.is_empty(), "no run was refused: {:?}", claim.refused);
    // The keep: the first body's entries reach the store, then it fails.
    let mut store = Memory::default();
    let mut session = Session::new(&tree);
    let off = &Setting::OFF;
    let first = &tree.keys[0];
    let def = tree.body(first).unwrap();
    kernel.get_in::<InferEntry<Tree>, _>(&mut session, &mut store, &tree, first, off).unwrap();
    for owner in owners(&tree, first) {
        kernel.get_in::<MirEntry<Tree>, _>(&mut session, &mut store, &tree, &owner, off).unwrap();
    }
    kernel.get_in::<BorrowckEntry<Tree>, _>(&mut session, &mut store, &tree, first, off).unwrap();
    let kept = [
        (def, Step::Infer, infer::SCHEMA),
        (def, Step::Mir, MIR_SCHEMA),
        (def, Step::Borrowck, borrowck_schema(db, def, CHECKER)),
    ];
    drop(claim);
    let next = claim_keeps(db).expect("an item store");
    let lost: Vec<_> = runs.iter().filter(|run| !kept.contains(*run) && !next.steps.contains(*run)).collect();
    if !lost.is_empty() {
        return Err(format!("{} runs neither kept nor claimed again: {lost:?}", lost.len()));
    }
    next.kept();
    let third = claim_keeps(db).expect("an item store");
    if !third.steps.is_empty() {
        return Err(format!("a kept claim's {} runs were claimed again", third.steps.len()));
    }
    third.kept();
    Ok(())
}

#[test]
fn a_keep_that_fails_leaves_its_runs_to_the_next() {
    left_to_the_next().unwrap();
}

/// The control: a claim that holds its marks when dropped ([`HELD`]) leaves the failed keep's
/// runs to no one, and the oracle finds them.
#[test]
fn the_oracle_catches_a_failed_keep_that_holds_its_claims() {
    HELD.with(|it| it.set(true));
    let caught = left_to_the_next();
    HELD.with(|it| it.set(false));
    assert!(caught.is_err(), "the planted bug went unseen");
}

/// What the database's query of a body's borrow check does (`hir_ty_borrowck`'s `borrowck`):
/// the served verdict, looked up in a query so the engine keeps its row and what it read.
#[blaze_query::tracked(returns(copy))]
fn served_check(db: &Db, def: DefWithBodyId) -> bool {
    lookup_passed(db, def, CHECKER, || panic!("no store here verifies"))
}

/// How many of `tree`'s bodies' borrow checks ran (missed their served verdict) when each is
/// asked of its query.
fn checks_ran(tree: &Tree) -> u64 {
    let before = tree.installed.steps()[Step::Borrowck as usize].ran;
    for &def in tree.bodies.values() {
        let _ = served_check(tree.db(), def);
    }
    tree.installed.steps()[Step::Borrowck as usize].ran - before
}

/// The invariant: the engine's row of a served step depends on every fact it and its earlier
/// steps read. A first tree keeps every entry of `leaf.rs` with a struct, whose size each
/// body's stand-in MIR reads (no inference or borrow check reads it); a second database is
/// served every entry and asks each borrow check: none runs. The struct gains a field there;
/// asked again, each borrow check whose MIR read the size runs, as many as in a fresh
/// database served nothing. `Err` says the two counts.
fn reruns_after_a_mir_fact() -> Result<(), String> {
    let kernel = kernel();
    let mut store = Memory::default();
    let sized = format!("{LEAF}{SIZED}");
    let grown = format!("{LEAF}{GROWN}");
    let mut first = Tree::new();
    first.edit("leaf.rs", &sized);
    {
        let mut session = Session::new(&first);
        ask_all(&kernel, &mut session, &mut store, &first);
    }
    // clone: a counted reference
    let mut second = Tree::with_defs(first.defs.clone());
    second.edit("leaf.rs", &sized);
    {
        let mut session = Session::new(&second);
        reset_runs();
        ask_and_serve(&kernel, &mut session, &mut store, &second, &second);
        assert_eq!(runs(), [0; Step::COUNT], "no entry runs in the second session: each is the store's");
    }
    assert_eq!(checks_ran(&second), 0, "each borrow check is its served verdict");
    second.edit("leaf.rs", &grown);
    let again = checks_ran(&second);
    // clone: a counted reference
    let mut fresh = Tree::with_defs(first.defs.clone());
    fresh.edit("leaf.rs", &grown);
    let expected = checks_ran(&fresh);
    assert_eq!(expected, BODIES as u64, "a database served nothing runs every borrow check");
    second.installed.errors().map_err(|why| format!("a served answer disagreed with the database: {why}"))?;
    match again == expected {
        true => Ok(()),
        false => Err(format!("{again} borrow checks ran after the struct grew, and {expected} in a fresh database")),
    }
}

#[test]
fn a_served_borrow_check_reruns_after_a_fact_only_its_mir_read_changes() {
    reruns_after_a_mir_fact().unwrap();
}

/// The control: a served row that holds its own facts alone ([`OWN_FACTS_ONLY`]) keeps the
/// borrow checks' rows past the struct's new field, and the oracle finds them.
#[test]
fn the_oracle_catches_a_served_row_without_its_earlier_steps_facts() {
    OWN_FACTS_ONLY.with(|it| it.set(true));
    let caught = reruns_after_a_mir_fact();
    OWN_FACTS_ONLY.with(|it| it.set(false));
    assert!(caught.is_err(), "the planted bug went unseen");
}
