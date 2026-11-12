//! Body entries carried through the kernel (end-state.md, order row 3): a two-crate program's
//! bodies, each with its inference, MIR and borrow check entries in one session, carried
//! across an edit of the leaf crate's file with the change set a driver gives (the file's text
//! differs; the impls fact from the impls maintainer's last write; identities always dirty,
//! counted). Each test counts the entries' runs and checks every entry against a fresh run
//! (the oracle); the control drops the identity read from every trace and the oracle must
//! catch the stale entry it leaves.
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
    AdtId, ConstId, DefWithBodyId, EnumId, EnumVariantId, FunctionId, ImplId, StaticId, StructId,
    TraitId, TypeAliasId, UnionId,
    deps::{PortableIds, body_key as hir_body_key},
    expr_store::Body,
    nameres::{crate_def_map, demand::crate_body_owners},
    signatures::FunctionSignature,
};
use hir_expand::fixture::{self, Fixture};
use std::alloc::Global;
use syntax::{Edition, Extensions};

use super::{
    BodyDelta, BodyKey, BodyWorld, BorrowckEntry, DROPPED, IDENTITY_NAME, InferEntry, MirEntry,
    RUNS, always_dirty, bodies, borrowck_schema, def_of, reads, register, run_infer,
};
use crate::{
    InferenceResult,
    body_store::{
        BodyFacts, Classified, Installed, Mode, Step, capture, infer, install, offer, offer_passed,
        tests::{BUILD, Memory as Images, OWN, is_impl_table, own, portable},
    },
    next_solver::{
        SolverDefId,
        goal_store::{FactObject, FactRef},
    },
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
struct EntryFacts {
    defs: Lock<HashMap<ContentId, (u8, PortableIds), Global>>,
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
        _: Step,
        _: blaze_query::db::DatabaseKeyIndex,
        _: blaze_query::db::ReadKind,
        name: &'static str,
    ) -> Classified {
        if name.contains("infer_body_query") {
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

/// The MIR stand-in: the body's inference read, and its answer offered as the MIR's.
#[blaze_query::tracked(returns(copy))]
fn fake_mir(db: &Db, def: DefWithBodyId) -> bool {
    let (_, reads) = capture(db, || {
        let _ = InferenceResult::of(db, def);
    });
    offer(db, def, Step::Mir, MIR_SCHEMA, reads, |facts, out| {
        out.extend_from_slice(b"mir ");
        infer::encode(db, facts, def, InferenceResult::of(db, def), out)
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
    fixture: Fixture,
    files: BTreeMap<String, String>,
    bodies: HashMap<(String, ItemId), DefWithBodyId, Global>,
    keys: Vec<BodyKey>,
}

impl Tree {
    fn new() -> Tree {
        let fixture = fixture::db(FIXTURE);
        let facts = Arc::new(EntryFacts { defs: Lock::new("test.ty.body_entries.defs", HashMap::default()) });
        let installed = install(fixture.db(), Arc::new(Images::default()), facts, BUILD, Mode::Serve)
            .expect("an item store for the test database");
        // The fixture's files as the database has them; `edit` sets both.
        let files = BTreeMap::from([("leaf.rs".to_owned(), LEAF.to_owned()), ("main.rs".to_owned(), MAIN.to_owned())]);
        let mut tree =
            Tree { installed, fixture, files, bodies: HashMap::new_in(Global), keys: Vec::new() };
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

    fn fact(&self, _: &str, fact: FactRef) -> Result<Seen, WorldError> {
        match fact {
            IMPLS => Ok(Seen::Is(impls(self.db()))),
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
    let mut kernel = Kernel::new();
    kernel.register_ask::<Parse<Tree>>(CodeId(ContentId([61; 32]))).unwrap();
    kernel.register_ask::<Items<Tree>>(CodeId(ContentId([62; 32]))).unwrap();
    register(&mut kernel, CodeId(BUILD)).unwrap();
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
fn fresh(kernel: &Kernel<Tree>, tree: &Tree) -> Vec<(String, [Vec<u8>; 3])> {
    let mut store = Memory::default();
    let mut session = Session::new(tree);
    ask_all(kernel, &mut session, &mut store, tree)
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

fn carried(edited: &str, dropped: Option<ReadKind>) -> Carried {
    let kernel = kernel();
    let mut tree = Tree::new();
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
    let delta = BodyDelta {
        exact: vec![(ReadKind(IMPLS.kind.0), impls_differ)],
        unknown: Vec::new(),
        identities: None,
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

/// The floor of a leaf body edit: the edited body's three entries, and nothing else; its
/// callers' entries are valued again (their identity is always dirty) and stay.
#[test]
fn a_leaf_body_edit_reruns_its_three_entries_and_no_callers() {
    let leaf = carried(&LEAF.replace("{ 1 }", "{ 3 }"), None);
    assert_eq!(leaf.runs, [1, 1, 1, 0], "the edited body's inference, MIR and borrow check, once each");
    assert_eq!(leaf.dirty, [(IDENTITY_NAME, 12)], "every entry reads its identity, which no delta gives yet");
    leaf.fresh.expect("every entry is what a fresh run gives");
}

/// The floor of a comment edit: no entry.
#[test]
fn a_comment_edit_reruns_no_entry() {
    let comment = carried(&format!("// a comment\n{LEAF}"), None);
    assert_eq!(comment.runs, [0; Step::COUNT], "a comment changes no body's record");
    assert_eq!(comment.dirty, [(IDENTITY_NAME, 12)]);
    comment.fresh.expect("every entry is what a fresh run gives");
}

/// A signature edit of the leaf reaches its caller through the caller's identity read: the
/// caller's inference names the leaf, whose type is other now.
#[test]
fn a_signature_edit_reaches_the_caller_through_its_identity() {
    let signature = carried(&LEAF.replace("leaf() -> u32", "leaf() -> u64"), None);
    assert!(signature.runs[Step::Infer as usize] >= 2, "the leaf and its caller: {:?}", signature.runs);
    signature.fresh.expect("every entry is what a fresh run gives");
}

/// The control: with the identity read left out of every trace, the signature edit reaches
/// no caller entry (its body's record and its facts are as they were), and the oracle finds
/// the stale one.
#[test]
fn the_oracle_catches_a_trace_that_drops_the_identity_read() {
    let planted = carried(&LEAF.replace("leaf() -> u32", "leaf() -> u64"), Some(reads::IDENTITY));
    assert_eq!(planted.dirty, [(IDENTITY_NAME, 0)], "no entry has an identity read to post");
    assert!(planted.fresh.is_err(), "a caller entry that read the leaf's signature stayed as it was");
}
