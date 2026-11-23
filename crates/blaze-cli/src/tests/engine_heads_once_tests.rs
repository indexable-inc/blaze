//! EACH HEAD FROZEN ONCE A REQUEST (engine_heads_once.rs, lane RH6), at the engine's seam and
//! under a race. A child of main.rs's tests, over engine_check_tests.rs's helpers.
//!
//! THE SEAM: two members, `b`'s heads reading `a`'s struct. Cold, the answer equals a fresh
//! check's and the one a source a head gives (`Sources::Head`, the measure before the table),
//! every cell is frozen once (`frozen` = `cells` + the races' `twice`), and the cold read sets
//! differ only at witnessed same-crate head cutoffs. Then each edit row is compared with a fresh check:
//! a body-only edit of `a`, a spelling change to an alias in `b` (equal frozen bytes stop
//! its readers), a width change to that alias, and a field added to `a`'s struct.
//!
//! THE RACE (rule 43, `blaze_sim`): two workers that both miss a cell and freeze it, interleaved
//! every way the tape draws (each a look, then a put on a miss): one entry, the first's bytes, an
//! equal second counted `twice`, other bytes refused. CONTROL: the planted "second write wins"
//! (`OncePlant::SecondWins`) is caught.

use super::engine_check_tests::{engine_opts, entries, opts};
use super::*;

use std::collections::{BTreeMap, BTreeSet};
use std::string::String as Label;

use crate::engine_heads::HeadUnits;
use crate::engine_heads::ListingsPlant;
use crate::engine_heads_once::{Entry, HeadsOnce, OnceCounts, OnceError, OncePlant, Put, Sources};
use blaze_drive::{Kind, Name};
use blaze_engine::{CellId, Stage};
use blaze_ladder::front2::heads::Want;
use blaze_ladder::front2::tables::head::{FrozenHead, want_bytes};
use blaze_sim::{Broke, Config, Tape, check as sim_check, control};

/// The ids a request listed, by the helpers' own form.
type Ids = std::vec::Vec<(std::string::String, std::string::String)>;

const WORKSPACE: &str = "[workspace]\nmembers = [\"a\", \"b\"]\nresolver = \"2\"\n";
const A_MANIFEST: &str = "[package]\nname = \"a\"\nversion = \"0.1.0\"\nedition = \"2024\"\n";
const B_MANIFEST: &str = "[package]\nname = \"b\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\na = { path = \"../a\" }\n";
const A_LIB: &str = "pub struct Point {\n    pub x: u32,\n}\n\npub fn make(p: Point) -> Point {\n    p\n}\n\npub fn twice(p: Point, q: Point) -> u32 {\n    p.x + q.x\n}\n";
/// A body-only edit: `make`'s body.
const A_LIB_BODY: &str = "pub struct Point {\n    pub x: u32,\n}\n\npub fn make(p: Point) -> Point {\n    let q = p;\n    q\n}\n\npub fn twice(p: Point, q: Point) -> u32 {\n    p.x + q.x\n}\n";
/// `Point` gains a field.
const A_LIB_FIELD: &str = "pub struct Point {\n    pub x: u32,\n    pub y: u32,\n}\n\npub fn make(p: Point) -> Point {\n    p\n}\n\npub fn twice(p: Point, q: Point) -> u32 {\n    p.x + q.x\n}\n";
const B_LIB: &str = "pub type Num = u32;\npub type Word = u32;\npub struct Holder {\n    pub p: a::Point,\n}\n\npub fn wrap(p: a::Point) -> Holder {\n    Holder { p }\n}\n\npub fn first(h: &Holder) -> Word {\n    h.p.x\n}\n\npub fn again(p: a::Point) -> a::Point {\n    a::make(p)\n}\n";

fn two_members(seed: u64) -> Io {
    let io = Io::simulated(SimWorld::new(seed, Limits::USUAL));
    write(&io, "Cargo.toml", WORKSPACE);
    write(&io, "a/Cargo.toml", A_MANIFEST);
    write(&io, "a/src/lib.rs", A_LIB);
    write(&io, "b/Cargo.toml", B_MANIFEST);
    write(&io, "b/src/lib.rs", B_LIB);
    io
}

const DIRECTORY_ITEMS: &str = concat!(
    "pub trait Storage { fn number() -> u8; }\n",
    "pub struct Value;\n",
    "impl Storage for Value { fn number() -> u8 { N } }\n",
    "pub const N: u8 = 1;\n",
    "pub fn read() -> u8 { Value::number() }\n",
);

pub(super) fn module_members(physical: &str, items: &str, a_lib: &str, b_lib: &str) -> Io {
    let io = Io::simulated(SimWorld::new(47, Limits::USUAL));
    write(&io, "Cargo.toml", WORKSPACE);
    write(&io, "a/Cargo.toml", A_MANIFEST);
    write(&io, "b/Cargo.toml", B_MANIFEST);
    write(&io, "a/src/lib.rs", a_lib);
    write(&io, "b/src/lib.rs", b_lib);
    write(&io, physical, items);
    io
}

/// Every committed Signature read has a defining cell AND a frozen run with its want and
/// meaning. Retain the unit prefix to check which instance each head or body actually read.
pub(super) struct ModuleReads {
    pub(super) owners: BTreeMap<CellId, [u8; 8]>,
    readers: BTreeMap<Name, BTreeSet<CellId>>,
    pub(super) a_library: [u8; 8],
    pub(super) a_test: [u8; 8],
}

pub(super) fn module_reads(io: &Io, physical: &str, slot: &str, text: &str) -> ModuleReads {
    use crate::store::Store as _;
    use blaze_world::crate_list::CrateGraph as _;

    super::engine_answer_rows_tests::with_store(io, |cx, store, opts, _| {
        let root = crate::engine_answer::root_name(opts.place.checkout, cx.frame());
        let pointer = crate::engine_answer::load_pointer(cx, store, &root).expect("read pointer").expect("stored answer").expect("valid pointer");
        let (kept, _) = crate::engine_answer::load_answer(cx, store, pointer.root, opts.heap.shared()).expect("open answer").expect("read answer");
        let rows = &kept.answer.rows;
        let outline = blaze_file_items::outline(text, blaze_surface::Edition::E2024, opts.heap.alloc());
        let planned: BTreeSet<_> = outline.items().iter()
            .filter(|item| crate::signature::is_signature_entry(&outline, item))
            .map(|item| crate::signature::signature_name(crate::signature::file_subject(physical), &outline, item, text))
            .collect();
        let in_slot: BTreeSet<_> = rows.iter().filter(|(name, row)| name.kind == Kind::Signature && row.file == Name::file(slot.as_bytes())).map(|(name, _)| *name).collect();
        assert_eq!(in_slot, planned, "Signature rows in {slot} must have the covering names of the entries in {physical}");
        assert!(!planned.is_empty());

        let mut owners = BTreeMap::new();
        for (name, row) in rows.iter().filter(|(name, _)| name.kind == Kind::Signature) {
            assert!(!row.failed && row.findings == 0, "signature {name} failed");
            let held = store.get(cx, kept.records[name]).expect("open signature record").expect("stored signature");
            let blaze_drive::record::Stored::Whole(view) = blaze_drive::record::open(held.bytes).expect("read signature record") else { panic!("expected a whole Signature record") };
            crate::engine_seeds::each_run(view.payload, &mut |tag, cell, at, len| {
                let frozen = FrozenHead::read(&view.payload[at..at + len]).expect("frozen run");
                assert_eq!(tag, want_bytes(frozen.want()).as_slice());
                let cell = blaze_drive::cells::Cell(cell.try_into().expect("cell width"));
                assert!(row.defines.contains(&(cell, frozen.meaning())), "the run agrees with the row's definition");
                let id = CellId { of: *name, cell };
                assert!(owners.insert(id, tag[1..9].try_into().expect("unit prefix")).is_none(), "duplicate run for {id:?}");
            }).expect("whole frozen runs");
        }
        let mut readers = BTreeMap::new();
        for (name, row) in rows.iter().filter(|(name, _)| matches!(name.kind, Kind::BodyCheck | Kind::Signature)) {
            let mut reads = BTreeSet::new();
            for &(id, digest) in row.reads.iter().filter(|(id, _)| id.of.kind == Kind::Signature) {
                assert!(rows[&id.of].defines.contains(&(id.cell, digest)), "{name} read the defined digest of {id:?}");
                assert!(owners.contains_key(&id), "{name} read {id:?} with no stored run");
                reads.insert(id);
            }
            readers.insert(*name, reads);
        }

        // Both b units depend on the same ordinary library of a. The other unit rooted at
        // a/src/lib.rs is a's test harness. This labels owners independently of the run tags.
        let graph = store.get(cx, kept.records[&Name::unit_graph()]).expect("open graph").expect("stored graph");
        let blaze_drive::record::Stored::Whole(view) = blaze_drive::record::open(graph.bytes).expect("read graph") else { panic!("expected a whole unit graph") };
        let list = crate::graph_payload::sections(view.payload).expect("graph sections").crates().expect("crate list").expect("workspace members");
        let members = |path: &'static str| (0..list.len()).filter(|&at| list.member(at) && list.root(at).strip_prefix("/0/") == Some(path)).collect::<std::vec::Vec<_>>();
        let a = members("a/src/lib.rs");
        let b = members("b/src/lib.rs");
        assert_eq!((a.len(), b.len()), (2, 2), "each crate has library and test units");
        let dep = |at| list.deps_named(at).find(|(alias, _)| *alias == "a").expect("b depends on a").1;
        let library = dep(b[0]);
        assert_eq!(library, dep(b[1]));
        assert!(a.contains(&library));
        let test = *a.iter().find(|&&at| at != library).expect("a's test unit");
        ModuleReads { owners, readers, a_library: list.key(library).prefix(), a_test: list.key(test).prefix() }
    })
}

pub(super) fn fixture_body(path: &str, text: &str, word: &str) -> Name {
    let outline = blaze_file_items::outline(text, blaze_surface::Edition::E2024, blaze_own::testing::heap().alloc());
    let item = outline.items().iter().find(|item| item.name.is_some_and(|name| item.text(text, name) == word)).expect("fixture reader");
    crate::work::body_name(Name::file(path.as_bytes()), &outline, item, text)
}

pub(super) fn fixture_signature(path: &str, text: &str, word: &str) -> Name {
    let outline = blaze_file_items::outline(text, blaze_surface::Edition::E2024, blaze_own::testing::heap().alloc());
    let item = outline.items().iter().find(|item| crate::signature::is_signature_entry(&outline, item) && crate::item_name::shown_name(item, text).starts_with(word)).expect("fixture head");
    crate::signature::signature_name(crate::signature::file_subject(path), &outline, item, text)
}

impl ModuleReads {
    pub(super) fn check_read(&self, reader: Name, entry: Name, owner: [u8; 8]) {
        let prefixes: BTreeSet<_> = self.readers[&reader].iter().filter(|id| id.of == entry).map(|id| self.owners[id]).collect();
        assert_eq!(prefixes, [owner].into_iter().collect(), "{reader} imports {entry} from its defining unit");
    }
}

pub(super) fn module_checked(io: &Io, units: HeadUnits, listings: ListingsPlant) -> engine_check::Checked {
    let mut o = opts(io, 2, true);
    // The directory fixtures use ADTs. Their borrow checks need core's Copy and Drop facts.
    let (id, files) = blaze_world::fixture::shelf_with_langs(graph::TRIPLE, blaze_world::fixture::STD, SHELF_CFG, o.heap.alloc());
    for (name, bytes) in files {
        let cx = io.cx(BUDGET, TASK_FRAME).expect("fixture shelf write context");
        let path = Path::new(&format!("h/{}/{name}", toolchain::SHELF)).expect("fixture shelf path");
        io.need_fs().expect("files").root().publish(&cx, &path, Content::bytes(&bytes), Durable::No).expect("write fixture shelf");
    }
    o.toolchain = id;
    o.head_units = units;
    o.head_jobs.listings = listings;
    engine_opts(io, o).1
}

pub(super) fn clean_module(checked: &engine_check::Checked) {
    assert_eq!(checked.seeds.bodies.refused, 0, "{:?}", checked.bodies.0);
    assert!(checked.bodies.1 >= 3, "at least the three reader bodies checked clean: {:?}", checked.bodies.0);
    assert!(checked.bodies.0.is_empty(), "{:?}", checked.bodies.0);
    assert_ne!(checked.status.0, Status::FAILED.0);
}

pub(super) fn path_plant_refused(checked: &engine_check::Checked, slot: &str, physical: &str) {
    assert!(checked.seeds.bodies.refused > 0);
    let slot_name = format!("(slot `{slot}`, {})", Name::file(slot.as_bytes()));
    assert!(checked.bodies.0.iter().any(|note| note.contains("its entry has no head task record") && note.contains("Signature(") &&
        note.contains(physical) && note.contains(&slot_name) && note.contains("requested defining unit")), "{:?}", checked.bodies.0);
}

#[test]
fn directory_module_heads_use_the_planned_signature() {
    // An explicit #[path] uses its literal file as the slot too (work.rs outline_file).
    for (slot, physical, declaration, module) in [
        ("a/src/storage.rs", "a/src/storage/mod.rs", "pub mod storage;", "storage"),
        ("a/src/x/y.rs", "a/src/x/y/mod.rs", "pub mod x;", "x::y"),
        ("a/src/other.rs", "a/src/other.rs", "#[path = \"other.rs\"] pub mod x;", "x"),
        ("a/src/storage.rs", "a/src/storage.rs", "pub mod storage;", "storage"),
    ] {
        let a_lib = format!("{declaration}\npub fn local() -> u8 {{ {module}::read() }}\n");
        let b_lib = format!("pub fn dependent() -> u8 {{ a::{module}::read() }}\n#[cfg(test)] pub fn dependent_test() -> u8 {{ a::{module}::read() + a::{module}::N }}\n");
        for listings in [ListingsPlant::Rule, ListingsPlant::SlotSignature] {
            let io = module_members(physical, DIRECTORY_ITEMS, &a_lib, &b_lib);
            if module == "x::y" {
                write(&io, "a/src/x/mod.rs", "pub mod y;\n");
            }
            let checked = module_checked(&io, HeadUnits::Each, listings);
            if listings == ListingsPlant::SlotSignature && physical != slot {
                path_plant_refused(&checked, slot, physical);
                continue;
            }
            clean_module(&checked);
            let reads = module_reads(&io, physical, slot, DIRECTORY_ITEMS);
            let read = fixture_signature(physical, DIRECTORY_ITEMS, "read");
            for (file, text, word) in [("a/src/lib.rs", a_lib.as_str(), "local"), ("b/src/lib.rs", b_lib.as_str(), "dependent"), ("b/src/lib.rs", b_lib.as_str(), "dependent_test")] {
                reads.check_read(fixture_body(file, text, word), read, reads.a_library);
            }
            for word in ["Storage", "Value", "impl Storage", "number", "N", "read"] {
                // A trait method belongs to the trait's entry. Its impl method has its own.
                let entry = fixture_signature(physical, DIRECTORY_ITEMS, word);
                assert!(reads.owners.keys().any(|id| id.of == entry), "{physical}: {word} has a frozen head");
            }
        }
    }
    println!("directory module: all required heads defined; seed refusals 0");
}

#[test]
fn a_head_selecting_a_trait_constant_reads_the_dependency_head_cells() {
    const PATH: &str = "a/src/width.rs";
    const WIDTH: &str = "pub struct Marker;\npub trait Width { const LEN: usize; }\nimpl Width for Marker { const LEN: usize = 3; }\n";
    const READER: &str = concat!(
        "use a::width::{Marker, Width};\n",
        "pub fn take(xs: [u8; <Marker as Width>::LEN]) -> [u8; 3] { xs }\n",
        "pub fn caller(xs: [u8; 3]) -> [u8; 3] { take(xs) }\n",
    );
    let io = module_members(PATH, WIDTH, "pub mod width;\n", READER);
    let checked = module_checked(&io, HeadUnits::Each, ListingsPlant::Rule);
    assert_eq!(checked.seeds.bodies.refused, 0, "{:?}", checked.bodies.0);
    assert!(checked.bodies.0.is_empty() && checked.bodies.1 >= 2, "{:?}", checked.bodies);
    assert_ne!(checked.status.0, Status::FAILED.0);

    // Selecting the impl reads Marker's coherence marks and Width's parameters. Both
    // units of b depend on the ordinary library of a. The caller consumes the frozen
    // signature whose array parameter has the selected length.
    let reads = module_reads(&io, PATH, PATH, WIDTH);
    let take = fixture_signature("b/src/lib.rs", READER, "take");
    for name in ["Marker", "Width"] {
        reads.check_read(take, fixture_signature(PATH, WIDTH, name), reads.a_library);
    }
    let caller = fixture_body("b/src/lib.rs", READER, "caller");
    assert!(reads.readers[&caller].iter().any(|id| id.of == take), "caller must read take's frozen signature");
    println!("caller reads take with length 3; take reads the library heads of Marker and Width");
}

/// What one request answered: the findings listed, the printed finding and note lines (sorted:
/// their order is not promised), the status and the counts.
struct Said {
    ids: Ids,
    lines: std::vec::Vec<std::string::String>,
    checked: engine_check::Checked,
}

impl Said {
    fn same_answer(&self, other: &Said) -> bool {
        self.ids == other.ids && self.lines == other.lines && self.checked.status.0 == other.checked.status.0
    }

    fn once(&self) -> OnceCounts {
        self.checked.seeds.once
    }

    fn heads(&self) -> u32 {
        self.checked.stages[Stage::Heads.index()].entered
    }
}

/// One request on 2 workers, its head tasks over `sources`.
fn request(io: &Io, fresh: bool, sources: Sources, units: HeadUnits) -> Said {
    let mut o = opts(io, 2, fresh);
    o.head_sources = sources;
    o.head_units = units;
    let before = stdout(io).len();
    let (ids, checked) = engine_opts(io, o);
    let text = stdout(io).split_off(before);
    let mut lines: std::vec::Vec<std::string::String> = text.lines().filter(|l| l.contains("error[") || l.contains("warning[") || l.starts_with("note:")).map(str::to_owned).collect();
    lines.sort_unstable();
    Said { ids, lines, checked }
}

type ReadSet = BTreeSet<CellId>;
type TaskUnit = (Name, [u8; 8]);
type UnitReads = BTreeMap<TaskUnit, std::vec::Vec<ReadSet>>;

std::thread_local! {
    // The coordinator joins the workers and takes their lists on the request thread.
    static UNIT_READS: std::cell::RefCell<Option<UnitReads>> = const { std::cell::RefCell::new(None) };
}

impl OnceCounts {
    /// Called on the coordinator before it merges this result into the stored row.
    pub(crate) fn observe_unit_reads(task: Name, unit: [u8; 8], reads: &[CellId]) {
        UNIT_READS.with_borrow_mut(|active| {
            if let Some(all) = active {
                assert!(reads.windows(2).all(|pair| pair[0] < pair[1]), "unsorted or duplicate unit reads for {task:?}, unit {:016x}", u64::from_be_bytes(unit));
                all.entry((task, unit)).or_default().push(reads.iter().copied().collect());
            }
        });
    }
}

fn observed_request(io: &Io, sources: Sources, units: HeadUnits) -> (Said, UnitReads) {
    UNIT_READS.set(Some(UnitReads::new()));
    let said = request(io, false, sources, units);
    (said, UNIT_READS.take().expect("observing this request"))
}


/// The cold Job answer supplies the row size and labels. The task/unit pairs come from the
/// fixture's outlines and unit graph, independently of which lists the observer received.
struct HeadRows {
    longest: usize,
    expected: BTreeSet<TaskUnit>,
    lang_by_unit: BTreeMap<[u8; 8], CellId>,
    head_units: BTreeMap<CellId, [u8; 8]>,
    expansions: BTreeMap<TaskUnit, CellId>,
    owners: BTreeMap<Name, Label>,
    parts: BTreeMap<CellId, Label>,
}

impl HeadRows {
    fn cold(io: &Io, files: &[(&str, &str)]) -> Self {
        use crate::store::Store as _;
        use blaze_world::crate_list::CrateGraph as _;

        super::engine_answer_rows_tests::with_store(io, |cx, store, opts, _| {
            let root = crate::engine_answer::root_name(opts.place.checkout, cx.frame());
            let pointer = crate::engine_answer::load_pointer(cx, store, &root).expect("open the cold pointer").expect("a cold answer").expect("read its pointer");
            let (kept, _) = crate::engine_answer::load_answer(cx, store, pointer.root, opts.heap.shared()).expect("open the cold pages").expect("read the cold rows");
            let longest = kept.answer.rows.iter()
                .filter(|(name, _)| name.kind == Kind::Signature)
                .map(|(_, row)| row.reads.len())
                .max().expect("cold Signature rows");
            let mut rows = Self {
                longest, expected: BTreeSet::new(), lang_by_unit: BTreeMap::new(), head_units: BTreeMap::new(),
                expansions: BTreeMap::new(), owners: BTreeMap::new(), parts: BTreeMap::new(),
            };

            // The graph distinguishes units of the same crate. Its crate keys also let us
            // name fine cells (including absent Lang and binding lookups) by their real keys.
            let graph = kept.records[&Name::unit_graph()];
            let held = store.get(cx, graph).expect("open the unit graph").expect("the stored unit graph");
            let blaze_drive::record::Stored::Whole(view) = blaze_drive::record::open(held.bytes).expect("read the unit graph") else {
                panic!("expected a whole unit graph");
            };
            let list = crate::graph_payload::sections(view.payload).expect("the graph sections").crates().expect("the crate list reads").expect("the fixture has crates");
            for at in 0..list.len() {
                let key = list.key(at);
                let scope = blaze_drive::key::crate_scope_name(&key);
                rows.owners.insert(scope, format!("{}@{:016x}", list.name(at), u64::from_be_bytes(key.prefix())));
                for &(path, text) in files {
                    if list.member(at) && list.root(at).strip_prefix("/0/") == Some(path) {
                        rows.label_member(key, path, text, opts.heap.alloc());
                    }
                }
            }
            // Only a cell the Signature row actually defines can stand for frozen bytes.
            rows.head_units.retain(|id, _| kept.answer.rows[&id.of].defines.iter().any(|(cell, _)| *cell == id.cell));
            rows
        })
    }

    fn label_member(&mut self, key: blaze_id::CrateItemKey, path: &str, text: &str, alloc: blaze_own::Alloc<'_>) {
        use blaze_scope::names_cells::{NameCell, Section, first_16};

        let scope = blaze_drive::key::crate_scope_name(&key);
        let mut part = |cell: NameCell, label: Label| {
            let id = CellId { of: scope, cell: blaze_drive::cells::Cell(cell.key()) };
            self.parts.insert(id, label);
            id
        };
        let krate = key.prefix();
        let module = blaze_scope::task::root_key(key);
        self.lang_by_unit.insert(krate, part(NameCell::Lang { krate }, "Lang".into()));
        part(NameCell::Root { krate }, "Root".into());
        part(NameCell::Incoherent { krate }, "Incoherent".into());
        part(NameCell::InternalSymbols { krate }, "InternalSymbols".into());
        part(NameCell::Links { krate }, "Links".into());
        part(NameCell::ImplRows { krate }, "ImplRows".into());
        part(NameCell::Files { krate }, "Files".into());
        part(NameCell::Extensions { krate }, "Extensions".into());
        part(NameCell::CfgAtoms { krate }, "CfgAtoms".into());
        part(NameCell::Module { module }, "Module(crate root)".into());
        part(NameCell::Item { key: module }, "Item(crate root)".into());
        part(NameCell::Textual { module }, "Textual(crate root)".into());
        part(NameCell::Beginning { first: first_16(module) }, "Beginning(crate root)".into());
        for section in [Section::Names, Section::Items, Section::Expansions] {
            part(NameCell::Whole { krate, section }, format!("Whole({section:?})"));
        }
        // A path can read a binding absent from this crate, including an extern crate name.
        let words = [A_LIB, B_LIB].into_iter().flat_map(|source| source.split(|c: char| !c.is_alphanumeric() && c != '_'));
        for word in words.filter(|word| !word.is_empty()).collect::<BTreeSet<_>>() {
            for ns in blaze_surface::Namespace::ALL {
                let name = blaze_scope::cell_key::cell_key(ns, word);
                part(NameCell::Binding { module, name }, format!("Binding({ns:?} {word})"));
            }
        }
        let file = Name::file(path.as_bytes());
        self.owners.insert(file, path.into());
        let outline = blaze_file_items::outline(text, blaze_surface::Edition::E2024, alloc);
        for item in outline.items().iter().filter(|item| crate::signature::is_signature_entry(&outline, item)) {
            let word = crate::item_name::shown_name(item, text);
            let name = crate::signature::signature_name(file.subject, &outline, item, text);
            self.expected.insert((name, krate));
            self.owners.insert(name, format!("{path}::{word}"));
            // The top-level fixtures have no duplicate names or calls.
            let tag = format!("{:?}", item.kind());
            let key = blaze_scope::task::key_of(key, module, &tag, item.name.map_or("", |range| item.text(text, range)), 0);
            let want = match item.kind() {
                blaze_surface::ItemKind::Struct => Want::Adt(key),
                blaze_surface::ItemKind::Function => Want::Fn(key),
                blaze_surface::ItemKind::TypeAlias => Want::Head(key),
                blaze_surface::ItemKind::MacroCall => {
                    let expansion = NameCell::Expansion { call: key };
                    self.expansions.insert((name, krate), CellId { of: scope, cell: blaze_drive::cells::Cell(expansion.key()) });
                    part(expansion, format!("Expansion({word})"));
                    continue;
                }
                kind => panic!("unexpected fixture signature {kind:?} at {path}::{word}"),
            };
            let cell = crate::engine_heads::head_cell(want, word, None, crate::engine_tasks::WHOLE);
            self.head_units.insert(CellId { of: name, cell }, krate);
            for (cell, kind) in [
                (NameCell::Item { key }, "Item"), (NameCell::Members { owner: key }, "Members"),
                (NameCell::MemberOf { member: key }, "MemberOf"), (NameCell::VariantOf { variant: key }, "VariantOf"),
                (NameCell::Variants { owner: key }, "Variants"), (NameCell::Beginning { first: first_16(key) }, "Beginning"),
            ] {
                part(cell, format!("{kind}({word})"));
            }
        }
    }

    fn owner(&self, name: Name) -> Label {
        self.owners.get(&name).cloned().unwrap_or_else(|| format!("{:?}@{}", name.kind, name.subject))
    }

    fn describe(&self, id: CellId) -> Label {
        use crate::engine_tasks::{HEADS, SURFACE, WHOLE};
        let kind = match id.cell {
            WHOLE => "WHOLE",
            HEADS => "HEADS",
            SURFACE => "SURFACE",
            _ if self.head_units.contains_key(&id) => "Signature head cell",
            _ => self.parts.get(&id).map(Label::as_str).unwrap_or("cell"),
        };
        format!("{} {kind} [{:032x}]", self.owner(id.of), u128::from_be_bytes(id.cell.0))
    }
}

fn check_counts(rows: &HeadRows, sources: Sources, units: &UnitReads, counts: OnceCounts) {
    assert_eq!(units.keys().copied().collect::<BTreeSet<_>>(), rows.expected, "{sources:?} cold task/unit coverage");
    let lengths = units.values().flatten().map(ReadSet::len);
    let most = lengths.clone().max().expect("the fixture has head tasks");
    assert_eq!(most, counts.read_most as usize, "{sources:?} longest unit list");
    assert_eq!(lengths.clone().count(), counts.read_lists as usize, "{sources:?} unit lists counted");
    assert_eq!(lengths.map(|n| n as u64).sum::<u64>(), counts.read_cells, "{sources:?} total cells counted");
}

struct ReadDifference {
    cutoffs: ReadSet,
    witnesses: ReadSet,
    unexpected: ReadSet,
    missing: ReadSet,
}

fn read_difference(rows: &HeadRows, head: &UnitReads, (task, _unit): TaskUnit, h: &ReadSet, j: &ReadSet) -> ReadDifference {
    // reads_from stops at HeadAt::Cell for another entry in the same crate. The fixture's
    // head_cell includes the unit prefix. Neither an Item lookup nor a file HEADS read qualifies.
    let cutoffs: ReadSet = j.iter().copied().filter(|id| id.of != task && rows.head_units.contains_key(id)).collect();
    let unexpected = j.difference(h).filter(|id| !cutoffs.contains(*id)).copied().collect();
    let mut missing = ReadSet::new();
    let mut witnesses = ReadSet::new();
    for id in h.difference(j) {
        let mut covered = false;
        for &cutoff in &cutoffs {
            // Sources::Head already follows the freeze transitively. Only look through an
            // actual Job edge, in its unit. If the producer ran twice, require the witness
            // in both observations rather than borrowing a read peculiar to another entry.
            let producer = &head[&(cutoff.of, rows.head_units[&cutoff])];
            if producer.iter().all(|reads| reads.contains(id)) {
                witnesses.insert(cutoff);
                covered = true;
            }
        }
        if !covered {
            missing.insert(*id);
        }
    }
    ReadDifference { cutoffs, witnesses, unexpected, missing }
}

fn compare_unit_reads(rows: &HeadRows, head: &UnitReads, job: &UnitReads, head_most: u32, job_most: u32) {
    println!("head reads: longest stored Job row {}, longest single unit: Head {head_most}, Job {job_most}", rows.longest);
    let mut tasks = BTreeSet::new();
    let mut with_cutoff = BTreeSet::new();
    let mut compared = 0;
    let mut cut_entries = 0;
    let mut largest = 0;
    let mut largest_at = Label::from("no Head-only reads");
    let mut violations = std::vec::Vec::new();
    for (&(task, unit), job_lists) in job {
        tasks.insert(task);
        let head_lists = &head[&(task, unit)];
        let name = rows.owner(task);
        let prefix = u64::from_be_bytes(unit);
        assert_eq!(head_lists.len(), job_lists.len(), "{name}, unit {prefix:016x}: number of entries");
        for (entry, (h, j)) in head_lists.iter().zip(job_lists).enumerate() {
            compared += 1;
            let at = format!("{name}, unit {prefix:016x}, entry {}", entry + 1);
            // Every head in this fixture uses the unit's receiver pointers, even when
            // another job froze it. A same-crate cutoff cannot cover this direct read.
            let lang = rows.lang_by_unit[&unit];
            if !j.contains(&lang) {
                violations.push(format!("{at}: missing direct read of {}", rows.describe(lang)));
            }
            let diff = read_difference(rows, head, (task, unit), h, j);
            if !diff.cutoffs.is_empty() {
                with_cutoff.insert(task);
                cut_entries += 1;
            }
            for &id in &diff.cutoffs {
                let side = if h.contains(&id) { "shared" } else { "Job only" };
                println!("head task {at}: {side} cutoff {}", rows.describe(id));
            }
            let omitted = h.difference(j).count();
            if omitted > largest {
                largest = omitted;
                let behind: std::vec::Vec<_> = diff.witnesses.iter().map(|id| rows.describe(*id)).collect();
                largest_at = format!("{at}, behind {behind:?}, {} uncovered", diff.missing.len());
            }
            for id in diff.unexpected {
                violations.push(format!("{at}: unexpected Job-only {}", rows.describe(id)));
            }
            for id in diff.missing {
                violations.push(format!("{at}: missing {} outside any same-crate Job cutoff", rows.describe(id)));
            }
        }
    }
    println!("head reads: {} tasks compared ({} task/unit pairs, {compared} entries), {} tasks with a cutoff ({cut_entries} entries), largest Head-only set {largest}: {largest_at}", tasks.len(), job.len(), with_cutoff.len());
    assert!(violations.is_empty(), "cold head read violations:\n{}", violations.join("\n"));
}

/// The row after an edit: the incremental answer against a fresh check's (rule 21: no row is
/// fast by being stale), printed with its counts.
fn row(io: &Io, name: &str, now: &Said) {
    let fresh = request(io, true, Sources::Job, HeadUnits::Each);
    let o = now.once();
    println!(
        "row {name}: heads {} entered; cells {}, frozen {}, twice {}, found {}; reads a head task: mean {}, most {}; equal a fresh check: {}; {}",
        now.heads(),
        o.cells,
        o.frozen,
        o.twice,
        o.found,
        o.read_mean(),
        o.read_most,
        now.same_answer(&fresh),
        entries(&now.checked)
    );
    assert!(now.same_answer(&fresh), "row {name}: the answer after the edit is not a fresh check's: ids {:?} against {:?}, lines {:?} against {:?}", now.ids, fresh.ids, now.lines, fresh.lines);
}

/// The actual frozen runs, including each unit's record bytes. The row key and the
/// envelope's read list can change when a spelling resolves through an extra alias.
pub(super) fn frozen_runs(io: &Io, task: Name) -> (blaze_drive::Key, std::vec::Vec<u8>) {
    use crate::store::Store as _;

    super::engine_answer_rows_tests::with_store(io, |cx, store, opts, _| {
        let root = crate::engine_answer::root_name(opts.place.checkout, cx.frame());
        let pointer = crate::engine_answer::load_pointer(cx, store, &root).expect("open pointer").expect("stored answer").expect("read pointer");
        let (kept, _) = crate::engine_answer::load_answer(cx, store, pointer.root, opts.heap.shared()).expect("open pages").expect("read rows");
        let row = &kept.answer.rows[&task];
        assert!(!row.failed && row.findings == 0 && !row.defines.is_empty(), "the signature task produced its cells");
        let held = store.get(cx, kept.records[&task]).expect("open signature record").expect("stored signature");
        let blaze_drive::record::Stored::Whole(view) = blaze_drive::record::open(held.bytes).expect("read signature record") else {
            panic!("expected a whole Signature record");
        };
        assert!(!view.payload.is_empty(), "the signature has frozen runs");
        (row.key, view.payload.to_vec())
    })
}

/// For every cold task, unit and entry, each Job-only read must be a same-crate head cutoff,
/// and each Head-only read must occur in the transitive Head reads of a cutoff present in Job.
/// As engine_heads_once.rs states, “A head read at its head task's cell (a head of the same
/// crate) ends there: its cell's digest is its frozen bytes' meaning.”
/// “So when anything a read head was made from moves, either that head's bytes move (its
/// cell's digest moves and the reader is entered at the next wave) or they do not, and then the
/// reader's bytes cannot move either (early cutoff).”
#[test]
fn each_head_is_frozen_once_and_its_readers_follow_its_meaning() {
    // Before: a source a head (the measure's baseline), on a world of its own.
    let before_io = two_members(23);
    let (before, head_units) = observed_request(&before_io, Sources::Head, HeadUnits::Each);
    let io = two_members(23);
    let (cold, job_units) = observed_request(&io, Sources::Job, HeadUnits::Each);
    let rows = HeadRows::cold(&io, &[("a/src/lib.rs", A_LIB), ("b/src/lib.rs", B_LIB)]);
    let fresh = request(&io, true, Sources::Job, HeadUnits::Each);
    assert!(cold.same_answer(&fresh), "cold: the table's answer is not a fresh check's: {:?} against {:?}", cold.lines, fresh.lines);
    assert!(cold.same_answer(&before), "cold: the table's answer is not a source a head's: {:?} against {:?}", cold.lines, before.lines);
    let o = cold.once();
    assert!(o.cells > 0, "cold: the table holds no cell: {o:?}");
    // The floor: each cell frozen once; a race between two jobs that both missed a cell is the
    // only second freeze, and it is counted.
    assert_eq!(o.frozen, o.cells + o.twice, "ORACLE: a cell was frozen more than once outside a race: {o:?}");
    assert_eq!(o.reads_unequal, 0, "two freezes of one cell kept other reads: {o:?}");
    // (`Checked` holds no profile text: the table's profile line is read on the real run.)
    let b = before.once();
    println!(
        "heads once before (a source a head): reads a head task mean {}, most {} (cells), lowered {}, constants evaluated {}; after (the table): mean {}, most {}, lowered {}, constants evaluated {} (distinct {})",
        b.read_mean(),
        b.read_most,
        b.lowered,
        b.evaluated,
        o.read_mean(),
        o.read_most,
        o.lowered,
        o.evaluated,
        o.consts
    );
    check_counts(&rows, Sources::Head, &head_units, b);
    check_counts(&rows, Sources::Job, &job_units, o);
    compare_unit_reads(&rows, &head_units, &job_units, b.read_most, o.read_most);
    assert!(o.read_mean() <= b.read_mean(), "ORACLE: the table's read lists are longer in the mean than a source a head's: {o:?} against {b:?}");
    assert!(o.lowered <= b.lowered, "ORACLE: the table lowered more than a source a head: {o:?} against {b:?}");
    println!("heads once cold: {o:?}; {}", entries(&cold.checked));

    // A body-only edit in `a` leaves every head key alone.
    write(&io, "a/src/lib.rs", A_LIB_BODY);
    let body = request(&io, false, Sources::Job, HeadUnits::Each);
    row(&io, "a body-only edit in a", &body);
    assert_eq!(body.heads(), 0, "ORACLE: a body-only edit entered a head task");

    // Both edits replace the same three-byte type token. Word's reader is in its own
    // crate, where reads_from records the frozen head cell. Check that edge on the cold row.
    let task = |path: &str| *rows.owners.iter().find(|(_, label)| label.as_str() == path).expect("fixture signature").0;
    let word = task("b/src/lib.rs::Word");
    let first = task("b/src/lib.rs::first");
    assert!(job_units.iter().any(|(&(owner, _), lists)| owner == first && lists.iter().flatten().any(|id| id.of == word && rows.head_units.contains_key(id))));
    let (original_key, original_bytes) = frozen_runs(&io, word);
    write(&io, "b/src/lib.rs", &B_LIB.replace("Word = u32", "Word = Num"));
    let (same, entered) = observed_request(&io, Sources::Job, HeadUnits::Each);
    let (same_key, same_bytes) = frozen_runs(&io, word);
    assert_ne!(original_key, same_key, "spelling the type through Num moves Word's key");
    assert_eq!(same_bytes, original_bytes, "Word must freeze byte-for-byte as u32");
    assert_eq!(same.heads(), 1);
    assert_eq!(same.checked.stages[Stage::Heads.index()].identical, 1);
    assert_eq!(entered.keys().map(|&(name, _)| name).collect::<BTreeSet<_>>(), BTreeSet::from([word]));
    assert_eq!(same.checked.stages[Stage::Bodies.index()].entered, 0);
    assert!(!same.lines.iter().any(|line| line.contains("error[types]")), "{:?}", same.lines);
    row(&io, "Word = Num (u32)", &same);
    println!("Word = Num: one head entered, identical frozen bytes, no reader entered");

    write(&io, "b/src/lib.rs", &B_LIB.replace("Word = u32", "Word = u16"));
    let (changed, entered) = observed_request(&io, Sources::Job, HeadUnits::Each);
    let (changed_key, changed_bytes) = frozen_runs(&io, word);
    assert_ne!(same_key, changed_key);
    assert_ne!(same_bytes, changed_bytes);
    assert!(entered.keys().any(|&(name, _)| name == first), "first's head reads Word");
    assert!(changed.checked.because.iter().any(|(g, _)| g.stage == Stage::Heads && g.cause == "read moved" && g.definer == Some(word)));
    assert!(changed.checked.stages[Stage::Bodies.index()].entered > 0);
    assert!(
        changed.lines.iter().any(|line| line.contains("b/src/lib.rs:") && line.contains("error[types]") && line.contains("found `u32`, expected `u16`")),
        "first returns a u32 field where u16 is required: {:?}", changed.lines
    );
    row(&io, "Word = u16", &changed);
    println!("Word = u16: frozen bytes changed, first entered and reported E0308");

    // `Point` gains a field: its head moves, and `b`'s heads that read it are entered.
    write(&io, "a/src/lib.rs", A_LIB_FIELD);
    let field = request(&io, false, Sources::Job, HeadUnits::Each);
    row(&io, "a's struct gains a field", &field);
    assert!(field.heads() >= 2, "ORACLE: `Point` gained a field and no head task of `b` that reads it was entered ({} entered)", field.heads());
}

/// CONTROL (rule 23). This call writes a macro definition, so its task reads the expansion
/// record and freezes no head. Removing that Job read leaves no cutoff that can cover it.
#[test]
fn the_cutoff_oracle_catches_a_dropped_expansion_read() {
    const LIB: &str = "macro_rules! later { () => { macro_rules! nothing { () => {}; } }; }\nlater!();\n";
    let fixture = || {
        let io = Io::simulated(SimWorld::new(23, Limits::USUAL));
        write(&io, "Cargo.toml", A_MANIFEST);
        write(&io, "src/lib.rs", LIB);
        io
    };
    let (baseline, head) = observed_request(&fixture(), Sources::Head, HeadUnits::Each);
    for units in [HeadUnits::Each, HeadUnits::NoExpansionReads] {
        let io = fixture();
        let (answer, job) = observed_request(&io, Sources::Job, units);
        let rows = HeadRows::cold(&io, &[("src/lib.rs", LIB)]);
        check_counts(&rows, Sources::Head, &head, baseline.once());
        check_counts(&rows, Sources::Job, &job, answer.once());
        for (&task_unit, lists) in &job {
            let baseline = &head[&task_unit];
            let expansion = rows.expansions[&task_unit];
            assert_eq!(baseline.len(), lists.len(), "control entries for {task_unit:?}");
            for (h, j) in baseline.iter().zip(lists) {
                assert!(h.contains(&expansion), "the call reads its expansion in {task_unit:?}");
                let diff = read_difference(&rows, &head, task_unit, h, j);
                assert!(diff.unexpected.is_empty() && diff.cutoffs.is_empty(), "{task_unit:?}: {units:?} adds a read or reaches another head");
                if units == HeadUnits::NoExpansionReads {
                    assert_eq!(diff.missing, ReadSet::from([expansion]), "the oracle must identify the dropped expansion");
                    println!("cutoff control caught missing {}", rows.describe(expansion));
                } else {
                    assert!(diff.missing.is_empty(), "{task_unit:?}: the rule misses {:?}", diff.missing);
                }
            }
        }
    }
}

/// When one member adds a private `fn`, `Heads` opens only that member's units (not unrelated
/// standing members or non-member toolchain crates) and runs only its new `Signature` task.
/// When a public struct gains a field, `Heads` still wakes downstream `Signature` tasks whose
/// read cell moved while leaving unrelated standing members unopened, matching a fresh oracle.
#[test]
fn incremental_heads_opens_only_active_member_scopes_and_wakes_moved_readers() {
    const FOUR_WORKSPACE: &str = "[workspace]\nmembers = [\"a\", \"b\", \"c\", \"d\"]\nresolver = \"2\"\n";
    const C_MANIFEST: &str = "[package]\nname = \"c\"\nversion = \"0.1.0\"\nedition = \"2024\"\n";
    const D_MANIFEST: &str = "[package]\nname = \"d\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\nc = { path = \"../c\" }\n";
    const C_LIB: &str = "pub struct Tag {\n    pub n: u32,\n}\n\npub fn tag(n: u32) -> Tag {\n    Tag { n }\n}\n";
    const D_LIB: &str = "pub struct Boxed {\n    pub t: c::Tag,\n}\n\npub fn keep(t: c::Tag) -> Boxed {\n    Boxed { t }\n}\n";
    const A_BASE: &str = "pub type Num = u32;\npub struct Point {\n    pub x: u32,\n}\n\npub fn make(p: Point) -> Point {\n    p\n}\n";
    const A_PRIVATE: &str = "pub type Num = u32;\npub struct Point {\n    pub x: u32,\n}\n\npub fn make(p: Point) -> Point {\n    p\n}\n\nfn _private_helper() -> u32 {\n    1\n}\n";
    const A_MOVED: &str = "pub type Num = u16;\npub struct Point {\n    pub x: u32,\n}\n\npub fn make(p: Point) -> Point {\n    p\n}\n";
    const B_READER: &str = "pub fn take(n: a::Num) -> u32 {\n    n\n}\n\npub fn again(p: a::Point) -> a::Point {\n    a::make(p)\n}\n";

    let build_world = |a_lib: &str| {
        let io = Io::simulated(SimWorld::new(71, Limits::USUAL));
        write(&io, "Cargo.toml", FOUR_WORKSPACE);
        write(&io, "a/Cargo.toml", A_MANIFEST);
        write(&io, "a/src/lib.rs", a_lib);
        write(&io, "b/Cargo.toml", B_MANIFEST);
        write(&io, "b/src/lib.rs", B_READER);
        write(&io, "c/Cargo.toml", C_MANIFEST);
        write(&io, "c/src/lib.rs", C_LIB);
        write(&io, "d/Cargo.toml", D_MANIFEST);
        write(&io, "d/src/lib.rs", D_LIB);
        io
    };
    let run_with_shelf = |io: &Io, fresh: bool| {
        let mut o = opts(io, 2, fresh);
        let (id, files) = blaze_world::fixture::shelf_with_langs(graph::TRIPLE, blaze_world::fixture::STD, SHELF_CFG, o.heap.alloc());
        for (name, bytes) in files {
            let cx = io.cx(BUDGET, TASK_FRAME).expect("fixture shelf write context");
            let path = Path::new(&format!("h/{}/{name}", toolchain::SHELF)).expect("fixture shelf path");
            io.need_fs().expect("files").root().publish(&cx, &path, Content::bytes(&bytes), Durable::No).expect("write fixture shelf");
        }
        o.toolchain = id;
        o.head_sources = Sources::Job;
        o.head_units = HeadUnits::Each;
        let before = stdout(io).len();
        let (ids, checked) = engine_opts(io, o);
        let text = stdout(io).split_off(before);
        let mut lines: std::vec::Vec<std::string::String> = text.lines().filter(|l| l.contains("error[") || l.contains("warning[") || l.starts_with("note:")).map(str::to_owned).collect();
        lines.sort_unstable();
        Said { ids, lines, checked }
    };

    let io = build_world(A_BASE);
    let cold = run_with_shelf(&io, false);
    assert_eq!(cold.checked.seeds.bodies.refused, 0, "{:?}", cold.checked.bodies.0);
    assert!(cold.checked.bodies.0.is_empty(), "{:?}", cold.checked.bodies.0);

    let cold_reads = cold.checked.records.reads[Stage::Heads.index()];
    assert!(
        cold_reads.records >= 11,
        "cold Heads opens all 4 members' units plus the 3 lang crates: {cold_reads:?}"
    );

    // 1. Add an unused private fn in `a`: only 1 new Signature task enters in `Heads`,
    //    and `Heads` opens only `a`'s 2 units (lib + test) plus the 3 lazily queried lang crates
    //    (`core`, `alloc`, `std`) — NOT standing members `b`, `c`, `d`.
    write(&io, "a/src/lib.rs", A_PRIVATE);
    let private = run_with_shelf(&io, false);
    assert_eq!(private.heads(), 1, "only the new private fn's Signature enters Heads");
    let heads_reads = private.checked.records.reads[Stage::Heads.index()];
    assert!(
        heads_reads.records <= 5 && heads_reads.records < cold_reads.records,
        "Heads must open only a's 2 units plus queried lang crates (<= 5), not standing members b/c/d: {heads_reads:?} (cold={cold_reads:?})"
    );
    let fresh_private = run_with_shelf(&build_world(A_PRIVATE), true);
    assert!(private.same_answer(&fresh_private), "incremental private-fn edit must match fresh oracle");

    // 2. Change `a::Num` from `u32` to `u16`: `a::Num` AND `b::take`'s downstream Signature task
    //    wake up via read-moved edges, while unrelated `c` and `d` stay unopened (<= 7: a + b's 4
    //    units + 3 queried lang crates).
    write(&io, "a/src/lib.rs", A_MOVED);
    let moved = run_with_shelf(&io, false);
    let num = fixture_signature("a/src/lib.rs", A_MOVED, "Num");
    assert!(moved.heads() >= 2, "a::Num and b::take must enter Heads ({} entered)", moved.heads());
    assert!(
        moved.checked.because.iter().any(|(g, _)| g.stage == Stage::Heads && g.cause == "read moved" && g.definer == Some(num)),
        "b::take must wake in Heads with read moved on a::Num: {:?}",
        moved.checked.because
    );
    assert!(
        moved.lines.iter().any(|line| line.contains("b/src/lib.rs:") && line.contains("error[types]")),
        "b::take must report the type mismatch after a::Num changed to u16: {:?}",
        moved.lines
    );
    let moved_reads = moved.checked.records.reads[Stage::Heads.index()];
    assert!(
        moved_reads.records <= 7 && moved_reads.records < cold_reads.records,
        "Heads must open only a and b's 4 units plus queried lang crates (<= 7), leaving c and d unopened: {moved_reads:?} (cold={cold_reads:?})"
    );
    let fresh_moved = run_with_shelf(&build_world(A_MOVED), true);
    assert!(moved.same_answer(&fresh_moved), "incremental cross-crate head edit must match fresh oracle");
}

// THE RACE.

/// One cell of the race, and its two freezes' bytes.
fn cell() -> Want {
    Want::Head(blaze_id::ItemKey(blaze_id::ContentId::of("blaze-cli heads once race", b"the cell")))
}

const ONE: &[u8] = b"the first freeze's bytes";
const OTHER: &[u8] = b"another freeze's bytes!!";

fn entry(bytes: &'static [u8]) -> Entry {
    Entry { want: cell(), member: false, bytes, out: blaze_ids::ContentId::of("blaze-cli heads once race", bytes), reads: &[], heads: &[], consts: &[], record: None }
}

/// The request's heap, by the helpers' own form (engine_check_tests.rs `opts`).
fn heap() -> &'static blaze_own::Heap {
    let io = Io::simulated(SimWorld::new(1, Limits::USUAL));
    opts(&io, 2, false).heap
}

/// Two workers, each a look then (on a miss) a put of its freeze, interleaved as the tape draws;
/// the second's bytes equal or other. The oracle over the table after both.
fn race(tape: &Tape, plant: OncePlant, heap: &'static blaze_own::Heap) -> Result<(), Broke> {
    let table = HeadsOnce::new(heap, plant, Sources::Job);
    let other = tape.bool("the second freeze made other bytes");
    let bytes = [ONE, if other { OTHER } else { ONE }];
    // Each worker's next step: 0 look, 1 put, 2 done.
    let mut step = [0u8; 2];
    let mut missed = [false; 2];
    let mut puts: std::vec::Vec<(usize, Result<Put, OnceError>)> = std::vec::Vec::new();
    while step != [2, 2] {
        let w = match (step[0] < 2, step[1] < 2) {
            (true, true) => usize::from(tape.bool("the second worker steps now")),
            (true, false) => 0,
            _ => 1,
        };
        match step[w] {
            0 => {
                missed[w] = table.look(cell()).is_none();
                step[w] = if missed[w] { 1 } else { 2 };
            }
            _ => {
                puts.push((w, table.put(entry(bytes[w]))));
                step[w] = 2;
            }
        }
    }
    let counts = table.counts();
    let oracle = |what: std::string::String| Err(Broke::Oracle(what));
    if counts.cells != 1 {
        return oracle(std::format!("the table holds {} entries of one cell", counts.cells));
    }
    let Some((first, _)) = puts.first().copied() else {
        return Err(Broke::Harness(std::string::String::from("no worker put")));
    };
    let held = table.look(cell()).map(|e| e.bytes);
    if held != Some(bytes[first]) {
        return oracle(std::string::String::from("the table holds other bytes than the first freeze's"));
    }
    match puts.as_slice() {
        [(_, Ok(Put::Kept))] if counts.twice == 0 => Ok(()),
        [(_, Ok(Put::Kept)), (_, Ok(Put::Twice))] if !other && counts.twice == 1 => Ok(()),
        [(_, Ok(Put::Kept)), (_, Err(OnceError::Unequal { .. }))] if other && counts.twice == 0 => Ok(()),
        seen => oracle(std::format!("the puts answered {seen:?} with twice {} (other bytes: {other})", counts.twice)),
    }
}

/// Expected: passes over every interleaving the tape draws.
#[test]
fn two_freezes_of_one_cell_leave_one_entry() {
    let heap = heap();
    sim_check(Config::cases(500), |tape| race(tape, OncePlant::Rule, heap));
}

/// CONTROL (rule 23): a second write that wins is caught by the same oracle.
#[test]
fn the_race_oracle_catches_a_second_write_that_wins() {
    let heap = heap();
    control(Config::cases(2000), OncePlant::Rule, OncePlant::SecondWins, "other bytes than the first freeze's", |tape, plant| race(tape, plant, heap));
}
