//! THE LIBRARY-FEATURES RECORD IN THE COMMAND'S STORE (scripts.rs `CommandFeatures`, blaze-world
//! lib_features_record.rs). A sample of a one-function edit on tempo had blaze-scope's
//! `lib_features::Lex::next` on the edit path: the walk lexed about 1,500 toolchain files once a
//! command, from the first `Script` task. Now each side's listing is a record keyed by the tool,
//! the triple and the toolchain's packs: the first command walks and puts it, and every later
//! command reads it by key and walks nothing (rule 77). The record is of the toolchain alone, so
//! it is kept under the toolchain's root (`Store::keep_with_toolchain`, store/roots.rs
//! `keep_toolchain`), not under one checkout's answer.
//!
//! Two commands on one home (engine_answer_rows_tests.rs `with_store`: the command's store
//! opened, then committed). A walk is the one way a record is made: blaze-world puts the record
//! after a miss's walk and never otherwise (script_setting.rs `feature_table_by`; its own test
//! `library_features_hit_never_lexes` shows a hit never calls the walk), so the puts count the
//! walks. The first command walks once (a put); the answer's save keeps it under the
//! toolchain's root. The second reads it (a hit) and puts nothing, its rows equal the first's,
//! and the root names the record. CONTROL: under another tool the second command walks again (a
//! miss and a put): the key holds the tool (rule 52).
//! A child of main.rs's tests: its helpers are theirs.

use std::boxed::Box as StdBox;
use std::collections::BTreeMap as StdMap;
use std::string::String as StdString;
use std::vec::Vec as StdVec;

use blaze_ids::ContentId;
use blaze_manifest::lock::{Lock, LockId};
use blaze_own::{AString, AVec, Alloc};
use blaze_toolchain::{Shelf, ToolchainId};
use blaze_unit_graph::{CfgAtom, Resolver, Unit, UnitGraph, UnitKind};
use blaze_world::lib_features_record::FeatureRecords;
use blaze_world::{Packages, Place, Side, Tree, World, WorldError};
use edition::{Edition as Syntax, PackageEdition};

use super::engine_answer_rows_tests::with_store;
use super::*;

const TRIPLE: &str = "aarch64-apple-darwin";
const TARGET_CFG: &[(&str, Option<&str>)] = &[("target_os", Some("macos")), ("unix", None)];
/// The `Script` kind's tool, as the command's plan hands it (engine_scripts.rs `plan`).
const TOOL: blaze_drive::Tool = blaze_drive::Tool(ContentId([3; 32]));
/// Another build of blaze: the CONTROL's tool.
const OTHER_TOOL: blaze_drive::Tool = blaze_drive::Tool(ContentId([4; 32]));

/// The fixture's std, staged, declaring a library feature of its own, as library/std does.
const STAGED_STD: &str = "#![no_std]\n#![feature(staged_api)]\n#![stable(feature = \"xp_rust1\", since = \"1.0.0\")]\npub mod prelude { pub mod rust_2021 {} pub mod rust_2024 {} }\n#[unstable(feature = \"xp_probe\", issue = \"none\")]\npub struct Probe;\n";

fn a() -> Alloc<'static> {
    blaze_own::testing::heap().alloc()
}

struct Memory(StdMap<StdString, StdVec<u8>>);

impl Shelf for Memory {
    type Error = &'static str;
    type Bytes = StdVec<u8>;
    fn place(&self) -> &str {
        "/shelf"
    }
    fn size(&mut self, name: &str) -> Result<Option<u64>, &'static str> {
        Ok(self.0.get(name).map(|b| b.len() as u64))
    }
    fn read(&mut self, name: &str, _size: u64) -> Result<StdVec<u8>, &'static str> {
        self.0.get(name).cloned().ok_or("gone")
    }
}

struct Member;

impl Packages<'static> for Member {
    fn place(&mut self, _package: LockId) -> Result<Place<'static>, WorldError<'static>> {
        Ok(Place::Checkout { dir: "hello" })
    }
}

struct NoFiles;

impl Tree for NoFiles {
    fn file(&self, _path: &str) -> Option<&[u8]> {
        None
    }
}

/// The world of `hello` (Rust 2021, so it opens std) over the fixture's shelf with
/// [`STAGED_STD`], and the toolchain's id (blaze-world script_setting_tests.rs `hello`).
fn hello() -> (World<'static>, ToolchainId) {
    let text = "version = 4\n\n[[package]]\nname = \"hello\"\nversion = \"0.1.0\"\n";
    let lock: &'static Lock<'static> = StdBox::leak(StdBox::new(Lock::read(text.as_bytes(), a()).expect("the lock reads")));
    let s = |text: &str| AString::from_str_in(text, a());
    let mut cfg = AVec::new_in(a());
    cfg.push(CfgAtom::pair("target_os", "macos", a()));
    cfg.push(CfgAtom::flag("unix", a()));
    let member = Unit {
        package: lock.named("hello").next().expect("in the lock"),
        name: s("hello"),
        version: s("0.1.0"),
        kind: UnitKind::Lib,
        for_host: false,
        edition: PackageEdition { syntax: Syntax::Edition2021, blaze: None },
        crate_name: s("hello"),
        root: s("src/lib.rs"),
        features: AVec::new_in(a()),
        cfg,
        deps: AVec::new_in(a()),
        build_script: None,
        links: None,
    };
    let mut units = AVec::new_in(a());
    units.push(member);
    let graph: &'static UnitGraph<'static> = StdBox::leak(StdBox::new(UnitGraph { units, resolver: Resolver::V2, meta: AVec::new_in(a()), left_out: AVec::new_in(a()) }));
    let (id, files) = blaze_world::fixture::shelf(TRIPLE, STAGED_STD, TARGET_CFG, a());
    let mut shelf = Memory(files.into_iter().collect());
    let b = blaze_own::testing::bump();
    let listed = blaze_world::open_list(&mut shelf, id, TRIPLE, graph, lock, &mut Member, b).unwrap_or_else(|e| panic!("{e}"));
    let listed = StdBox::leak(StdBox::new(listed));
    (blaze_world::world(graph, lock, listed, &mut Member, &NoFiles, b).unwrap_or_else(|e| panic!("{e}")), id)
}

/// [`crate::scripts::CommandFeatures`] counted: its hits and its puts (a put is a walk), and
/// the key put.
struct Counted<'f, 'c, 's, 'm> {
    inner: &'f mut crate::scripts::CommandFeatures<'c, 's, 'm>,
    hits: u32,
    puts: u32,
    put: Option<ContentId>,
}

impl FeatureRecords for Counted<'_, '_, '_, '_> {
    type Error = CliError;

    fn get(&mut self, key: ContentId) -> Result<Option<&[u8]>, CliError> {
        let got = self.inner.get(key)?;
        if got.is_some() {
            self.hits += 1;
        }
        Ok(got)
    }

    fn put(&mut self, key: ContentId, bytes: &[u8]) -> Result<(), CliError> {
        self.puts += 1;
        self.put = Some(key);
        self.inner.put(key, bytes)
    }
}

/// One command's target-side table under `tool`: its rows (debug-printed), its hits, its puts
/// and the key put; then the answer's save keeps the toolchain's records under its root.
fn command(io: &Io, world: &World<'static>, toolchain: ToolchainId, tool: blaze_drive::Tool) -> (StdVec<StdString>, u32, u32, Option<ContentId>) {
    with_store(io, |cx, store, _, _| {
        let mut features = crate::scripts::CommandFeatures { cx, store: &mut *store, held: None };
        let mut counted = Counted { inner: &mut features, hits: 0, puts: 0, put: None };
        let rows = blaze_world::script_setting::feature_table(world, Side::Target, TRIPLE, tool.0, &mut counted, a()).unwrap_or_else(|e| panic!("the table: {e}"));
        let rows: StdVec<StdString> = rows.iter().map(|row| std::format!("{row:?}")).collect();
        let (hits, puts, put) = (counted.hits, counted.puts, counted.put);
        drop(counted);
        drop(features);
        // The answer's save (engine_answer.rs `save`): the toolchain's records under its root.
        store.keep_toolchain(toolchain, &[]).expect("the toolchain's root is kept");
        (rows, hits, puts, put)
    })
}

#[test]
fn the_second_command_reads_the_library_features_record_and_never_walks() {
    let io = Io::simulated(SimWorld::new(41, Limits::USUAL));
    write(&io, "Cargo.toml", "[package]\nname = \"hello\"\nversion = \"0.1.0\"\nedition = \"2021\"\n");
    write(&io, "src/lib.rs", "pub fn f() -> u8 {\n    1\n}\n");
    let (world, toolchain) = hello();
    let (first, first_hits, first_puts, key) = command(&io, &world, toolchain, TOOL);
    assert_eq!((first_hits, first_puts), (0, 1), "the first command walks once and puts the record");
    let key = key.expect("the record's key");
    let (second, second_hits, second_puts, _) = command(&io, &world, toolchain, TOOL);
    println!("FEATURES: first walked {first_puts}, second hit {second_hits} and walked {second_puts}; {} rows", second.len());
    assert_eq!((second_hits, second_puts), (1, 0), "the second command reads the record and never walks");
    assert_eq!(first, second, "the record answers the walk's rows");
    // The toolchain's root names the record through one of its indexes (store/roots.rs).
    let named = with_store(&io, |_, _, _, lent| {
        let root = crate::store::roots::RootName::toolchain(toolchain);
        let Some(at) = lent.root_named(root.as_str()) else { return false };
        let indexes = lent.get(&at).expect("the root reads").expect("the root's record");
        indexes.bytes.chunks_exact(32).any(|index| {
            let index = blaze_cas::Key(ContentId(index.try_into().expect("32 bytes")));
            let held = lent.get(&index).expect("an index reads").expect("an index's record");
            held.bytes.chunks_exact(32).any(|named| named == key.0.as_slice())
        })
    });
    assert!(named, "the record is kept under the toolchain's root, not swept by cleanup");
    // CONTROL: another tool is another key; the walk runs again.
    let (_, other_hits, other_puts, _) = command(&io, &world, toolchain, OTHER_TOOL);
    assert_eq!((other_hits, other_puts), (0, 1), "CONTROL: under another tool the command walks again");
}
