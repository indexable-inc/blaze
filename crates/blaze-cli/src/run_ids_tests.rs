//! `RunIds` over a crate table made here (no world, no files): two toolchain crates, two locked
//! crates (`b` depends on `a`) and one checkout crate, over a store that holds nothing. Each test
//! says what it asserts.

use super::*;
use blaze_drive::profile::Profile;
use bstd::io::{Io, Limits, SimWorld};

/// One crate of the table: its key's byte (the whole key that byte, so its prefix too), name,
/// place and dependencies.
struct Row {
    byte: u8,
    name: &'static str,
    place: Place,
    deps: &'static [usize],
}

struct Table(std::vec::Vec<Row>);

impl CrateTable<'static> for Table {
    fn len(&self) -> usize {
        self.0.len()
    }
    fn key(&self, at: usize) -> CrateItemKey {
        CrateItemKey(ContentId([self.0[at].byte; 32]))
    }
    fn name(&self, at: usize) -> &'static str {
        self.0[at].name
    }
    fn place(&self, at: usize) -> Place {
        self.0[at].place
    }
    fn each_dep(&self, at: usize, each: &mut dyn FnMut(usize)) {
        for dep in self.0[at].deps {
            each(*dep);
        }
    }
}

const CORE: u8 = 0x11;
const STD: u8 = 0x12;
const DEP_A: u8 = 0x21;
const DEP_B: u8 = 0x22;
const APP: u8 = 0x31;

fn fetched(byte: u8) -> Place {
    Place::Fetched { source: ContentId([byte; 32]), files: [byte ^ 0xff; 32] }
}

/// The table; `a_files` is crate `a`'s pack files' BLAKE3.
fn table(a_files: [u8; 32]) -> Table {
    Table(std::vec![
        Row { byte: CORE, name: "core", place: Place::Toolchain, deps: &[] },
        Row { byte: STD, name: "std", place: Place::Toolchain, deps: &[0] },
        Row { byte: DEP_A, name: "a", place: Place::Fetched { source: ContentId([DEP_A; 32]), files: a_files }, deps: &[1] },
        Row { byte: DEP_B, name: "b", place: fetched(DEP_B), deps: &[2, 1] },
        Row { byte: APP, name: "app", place: Place::Checkout, deps: &[3, 1] },
    ])
}

/// A checker key of the crate whose key is `byte` everywhere: its prefix, then `low`.
fn item(byte: u8, low: u64) -> ItemKey {
    let mut b = [byte; 16];
    b[8..].copy_from_slice(&low.to_be_bytes());
    ItemKey(u128::from_be_bytes(b))
}

/// A store with no record.
struct Empty;

impl<'m> Store<'m> for Empty {
    fn get(&mut self, _cx: &Cx, _key: Key) -> Result<Option<crate::store::Held<'m>>, CliError> {
        Ok(None)
    }
    fn put(&mut self, _cx: &Cx, _key: Key, _bytes: &[u8]) -> Result<(), CliError> {
        Ok(())
    }
    fn map_all(&mut self, _cx: &Cx) -> Result<(), CliError> {
        Ok(())
    }
    fn after_answer(&mut self, _cx: &Cx, _profile: &mut Profile) -> Result<(), CliError> {
        Ok(())
    }
    fn public_crates(&mut self, _subjects: &[ContentId]) {}
}

fn tools() -> [Tool; blaze_drive::key::KINDS] {
    core::array::from_fn(|at| Tool(ContentId([at as u8; 32])))
}

/// The one workspace body: `app`'s `main`.
fn main_body() -> WorkspaceBody {
    WorkspaceBody {
        item: item(APP, 1),
        check: Name::new(Kind::BodyCheck, ContentId([0x5a; 32])),
        text: ContentId([0x5b; 32]),
        file: Name::new(Kind::FileItems, ContentId([0x5c; 32])),
    }
}

const GRAPH_OUT: ContentId = ContentId([0x61; 32]);
const OWNERS_OUT: ContentId = ContentId([0x62; 32]);

/// Today's outs of the `UnitGraph` and of `main`'s file's `Owners` (when `owners`), sorted.
fn outs(owners: bool) -> std::vec::Vec<(Name, ContentId)> {
    let mut outs = std::vec![(Name::unit_graph(), GRAPH_OUT)];
    if owners {
        outs.push((blaze_drive::key::owners_of(main_body().file), OWNERS_OUT));
    }
    outs.sort_by_key(|(name, _)| *name);
    outs
}

/// `RunIds` of `table` under `toolchain`, the store empty, today `outs`.
fn ids_with(toolchain: ToolchainId, table: &Table, outs: &[(Name, ContentId)]) -> RunIds<'static> {
    ids_after(toolchain, table, outs, None)
}

/// `ids_with`, walked forward from the table `previous`.
fn ids_after(toolchain: ToolchainId, table: &Table, outs: &[(Name, ContentId)], previous: Option<&RunIds<'_>>) -> RunIds<'static> {
    let io = Io::simulated(SimWorld::new(3, Limits::USUAL));
    let cx = io.cx(bstd::test::BUDGET, bstd::mem::TASK_FRAME).expect("a root context");
    let tools = tools();
    let today = SortedToday::new(outs).unwrap_or_else(|e| panic!("{e}"));
    let tree = [(Name::units(), Name::units()), (Name::units(), main_body().file)];
    RunIds::new(&cx, toolchain, table, &tools, &[main_body()], &tree, &today, previous, &mut Empty, blaze_own::testing::heap().alloc()).unwrap_or_else(|e| panic!("{e}"))
}

/// `RunIds` of `table` under `toolchain`, the store empty, the graph and main's owners known.
fn ids(toolchain: ToolchainId, table: &Table) -> RunIds<'static> {
    ids_with(toolchain, table, &outs(true))
}

/// A toolchain item's `BodyId` is equal across two constructions (its key is made from the
/// toolchain, the crate and the item, nothing of the run), reads exactly the toolchain fact, and
/// moves when the toolchain's id moves (key and read digest both).
#[test]
fn a_toolchain_body_id_is_stable_and_moves_with_the_toolchain() {
    let first = ids(ToolchainId([7; 32]), &table([1; 32]));
    let again = ids(ToolchainId([7; 32]), &table([1; 32]));
    let other = ids(ToolchainId([8; 32]), &table([1; 32]));
    let f = item(STD, 42);
    let (a, b, c) = (first.body_check(f).expect("std's item"), again.body_check(f).expect("std's item"), other.body_check(f).expect("std's item"));
    assert_eq!(a.key, b.key, "two constructions under one toolchain give one key");
    assert_eq!(a.reads, b.reads);
    assert_eq!(a.reads, &[Read { fact: toolchain_fact(), digest: [7; 32] }], "one read: the toolchain, its digest the id");
    assert_ne!(a.key, c.key, "another toolchain is another key");
    assert_ne!(a.reads[0].digest, c.reads[0].digest, "another toolchain is another digest of the one fact");
    assert_eq!(first.grain(f), Some(Grain::Toolchain));
}

/// `today` of the toolchain fact is the toolchain's id, the digest its bodies' read carries, so
/// the walk keys them; and `crate_fact` of a toolchain item is that same read.
#[test]
fn today_of_the_toolchain_fact_is_its_digest() {
    let ids = ids(ToolchainId([7; 32]), &table([1; 32]));
    let f = item(CORE, 3);
    let read = ids.body_check(f).expect("core's item").reads[0];
    assert_eq!(ids.today(&read.fact), Some(read.digest));
    assert_eq!(ids.today(&toolchain_fact()), Some([7; 32]));
    assert_eq!(ids.crate_fact(f), Some(read));
}

/// A workspace item with no record in the store has empty reads (the walk then makes it
/// unkeyed, `Unkeyed::NoRecord`), says why (`Missing::NoRecord`), and is not `Clean`, so the
/// parent refuses it; its key is the key check files it under (body_types.rs `body_check_key`,
/// the crate list and the file's owners folded in), never a guess. An item of the checkout the
/// caller did not name has no `BodyId` at all.
#[test]
fn a_workspace_item_with_no_record_has_empty_reads() {
    let ids = ids(ToolchainId([7; 32]), &table([1; 32]));
    let main = main_body();
    let body = ids.body_check(main.item).expect("app's main is named");
    assert!(body.reads.is_empty(), "no record: no reads");
    let tool = tools()[Kind::BodyCheck.index()];
    let key = crate::body_types::body_check_key(tool, main.check, main.text, main.file, GRAPH_OUT, OWNERS_OUT);
    assert_eq!(body.key, key.0, "the key is check's own BodyCheck key");
    let bare = task_key(tool, main.check, &crate::body_types::body_source(main.text));
    assert_ne!(body.key, bare.0, "control: the bare task key is not the one check files under");
    assert_eq!(ids.missing(main.item), Some(Missing::NoRecord));
    assert_eq!(ids.checked(main.item), Some(Checked::Absent(Missing::NoRecord)));
    assert_eq!(ids.grain(main.item), Some(Grain::Workspace));
    assert!(ids.body_check(item(APP, 2)).is_none(), "an unnamed workspace body has no id");
    assert_eq!(ids.crate_fact(main.item), None, "no CrateScope known today: no crate fact");
}

/// Two items of equal text (equal low bytes) in two crates get different keys, for two locked
/// crates and for two toolchain crates: the crate is in the key. CONTROL: one item asked twice
/// gives one key.
#[test]
fn equal_items_in_two_crates_get_different_keys() {
    let ids = ids(ToolchainId([7; 32]), &table([1; 32]));
    let key = |byte: u8| ids.body_check(item(byte, 9)).expect("an item of the table").key;
    assert_ne!(key(DEP_A), key(DEP_B), "two locked crates");
    assert_ne!(key(CORE), key(STD), "two toolchain crates");
    assert_eq!(key(DEP_A), key(DEP_A), "control: one item, one key");
    assert_eq!(ids.grain(item(DEP_A, 9)), Some(Grain::Dependency));
}

/// A locked crate's one read covers its closure: `b` depends on `a`, so a change of `a`'s pack
/// files alone moves `b`'s read digest (and `a`'s), and leaves `b`'s key (from identity) equal.
#[test]
fn a_locked_crate_read_moves_with_a_dependency_pack() {
    let before = ids(ToolchainId([7; 32]), &table([1; 32]));
    let after = ids(ToolchainId([7; 32]), &table([2; 32]));
    let b = item(DEP_B, 5);
    let (x, y) = (before.body_check(b).expect("b's item"), after.body_check(b).expect("b's item"));
    assert_eq!(x.key, y.key, "the key is identity");
    assert_eq!(x.reads[0].fact, y.reads[0].fact, "one fact, the crate's");
    assert_ne!(x.reads[0].digest, y.reads[0].digest, "a dependency's pack moved the closure digest");
    assert_eq!(after.today(&y.reads[0].fact), Some(y.reads[0].digest), "today is the digest it carries");
}

/// A locked crate that depends on a checkout crate is refused by name: no coarse fact covers it.
#[test]
fn a_locked_crate_on_the_checkout_is_refused_by_name() {
    let mut t = table([1; 32]);
    t.0[3].deps = &[4];
    let io = Io::simulated(SimWorld::new(3, Limits::USUAL));
    let cx = io.cx(bstd::test::BUDGET, bstd::mem::TASK_FRAME).expect("a root context");
    let tools = tools();
    match RunIds::new(&cx, ToolchainId([7; 32]), &t, &tools, &[], &[], &SortedToday::EMPTY, None, &mut Empty, blaze_own::testing::heap().alloc()) {
        Err(RunIdsError::LockedReadsWorkspace { krate: "b", dep: "app" }) => {}
        Err(other) => panic!("refused otherwise: {other}"),
        Ok(_) => panic!("a locked crate on the checkout was keyed"),
    }
}

/// A body whose file's `Owners` out (or the `UnitGraph`'s) the command does not know today is
/// not keyed with a guessed `ABSENT`: it is `NotToday` naming the missing name, and not `Clean`.
/// CONTROL: with both known it is `NoRecord` (the empty store), not `NotToday`.
#[test]
fn a_body_whose_owners_are_not_known_today_names_them() {
    let main = main_body();
    let no_owners = ids_with(ToolchainId([7; 32]), &table([1; 32]), &outs(false));
    assert_eq!(no_owners.checked(main.item), Some(Checked::Absent(Missing::NotToday { name: blaze_drive::key::owners_of(main.file) })));
    let nothing = ids_with(ToolchainId([7; 32]), &table([1; 32]), &[]);
    assert_eq!(nothing.checked(main.item), Some(Checked::Absent(Missing::NotToday { name: Name::unit_graph() })));
    let both = ids(ToolchainId([7; 32]), &table([1; 32]));
    assert_eq!(both.checked(main.item), Some(Checked::Absent(Missing::NoRecord)), "control");
}

/// Today's list must be sorted by name, one each: a repeated name is refused at its row.
#[test]
fn an_unsorted_today_is_refused() {
    let name = Name::unit_graph();
    match SortedToday::new(&[(name, GRAPH_OUT), (name, OWNERS_OUT)]) {
        Err(RunIdsError::TodayUnsorted { at: 1 }) => {}
        Err(other) => panic!("refused otherwise: {other}"),
        Ok(_) => panic!("a repeated name was taken"),
    }
}

/// The items every query is asked of: a body of each crate, an unnamed workspace body and an
/// item of no crate.
fn probes() -> [ItemKey; 7] {
    [item(CORE, 3), item(STD, 42), item(DEP_A, 9), item(DEP_B, 5), main_body().item, item(APP, 2), item(0x99, 1)]
}

/// Every query's answer of `ids` over `probes`, and `today` of every fact they read plus one
/// no body reads, as one comparable list.
fn answers(ids: &RunIds<'_>) -> std::vec::Vec<std::string::String> {
    let mut out = std::vec::Vec::new();
    for probe in probes() {
        let body = ids.body_check(probe).map(|b| (b.key, b.reads.to_vec()));
        out.push(std::format!("{probe:?}: {body:?} {:?} {:?} {:?}", ids.crate_fact(probe), ids.grain(probe), ids.checked(probe)));
        if let Some((_, reads)) = body {
            for read in reads {
                out.push(std::format!("today {:?}: {:?}", read.fact, ids.today(&read.fact)));
            }
        }
    }
    out.push(std::format!("today toolchain: {:?}", ids.today(&toolchain_fact())));
    out.push(std::format!("today other: {:?}", ids.today(&[0xee; 32])));
    out
}

/// Tables of three kinds of run: every row known, one with a `NotToday` row (its name kept in
/// the row), one with nothing known today.
fn tables() -> [RunIds<'static>; 3] {
    [
        ids(ToolchainId([7; 32]), &table([1; 32])),
        ids_with(ToolchainId([7; 32]), &table([1; 32]), &outs(false)),
        ids_with(ToolchainId([8; 32]), &table([2; 32]), &[]),
    ]
}

/// Written then read, a table answers every query (`body_check`, `today`, `crate_fact`,
/// `grain`, `checked`) as the built one does, and writes equal bytes again (rule 25). CONTROL:
/// two different tables write different bytes, so the equality is not vacuous.
#[test]
fn a_written_table_reads_back_equal() {
    let heap = blaze_own::testing::heap();
    let built = tables();
    for ids in built.iter() {
        let bytes = ids.write(heap.alloc());
        let back = RunIds::read(&bytes, heap.alloc()).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(answers(&back), answers(ids), "the record answers as the built table does");
        assert_eq!(&back.write(heap.alloc())[..], &bytes[..], "a rewrite is the same bytes");
    }
    assert_ne!(&built[0].write(heap.alloc())[..], &built[1].write(heap.alloc())[..], "control: two tables, two byte strings");
}

/// `put` files the table at `table_key(cert, ..)`, the bytes `write` gives; a store with no
/// table at a certificate is `Ok(None)` from `from_record` (the caller answers in full), never a
/// table guessed.
#[test]
fn a_table_is_put_at_its_certificate_and_absent_is_none() {
    struct Puts(std::vec::Vec<(Key, std::vec::Vec<u8>)>);
    impl<'m> Store<'m> for Puts {
        fn get(&mut self, _cx: &Cx, _key: Key) -> Result<Option<crate::store::Held<'m>>, CliError> {
            Ok(None)
        }
        fn put(&mut self, _cx: &Cx, key: Key, bytes: &[u8]) -> Result<(), CliError> {
            self.0.push((key, bytes.to_vec()));
            Ok(())
        }
        fn map_all(&mut self, _cx: &Cx) -> Result<(), CliError> {
            Ok(())
        }
        fn after_answer(&mut self, _cx: &Cx, _profile: &mut Profile) -> Result<(), CliError> {
            Ok(())
        }
        fn public_crates(&mut self, _subjects: &[ContentId]) {}
    }
    let io = Io::simulated(SimWorld::new(3, Limits::USUAL));
    let cx = io.cx(bstd::test::BUDGET, bstd::mem::TASK_FRAME).expect("a root context");
    let heap = blaze_own::testing::heap();
    let tools = tools();
    let ids = ids(ToolchainId([7; 32]), &table([1; 32]));
    let cert = Key(ContentId([0x77; 32]));
    let mut store = Puts(std::vec::Vec::new());
    ids.put(&cx, &mut store, cert, &tools, heap.alloc()).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(store.0.len(), 1, "one put");
    assert_eq!(store.0[0].0, table_key(cert, ToolchainId([7; 32]), &tools));
    assert_eq!(&store.0[0].1[..], &ids.write(heap.alloc())[..]);
    assert_ne!(table_key(cert, ToolchainId([8; 32]), &tools), store.0[0].0, "control: the toolchain is in the key");
    match RunIds::from_record(&cx, &mut Empty, cert, ToolchainId([7; 32]), &tools, heap.alloc()) {
        Ok(None) => {}
        Ok(Some(_)) => panic!("a table was read from an empty store"),
        Err(e) => panic!("refused: {e}"),
    }
}

/// Truncated, oversized, another version and another magic are typed errors, each its own.
#[test]
fn a_broken_table_is_a_typed_error() {
    let heap = blaze_own::testing::heap();
    let bytes = ids(ToolchainId([7; 32]), &table([1; 32])).write(heap.alloc()).to_vec();
    let read = |b: &[u8]| RunIds::read(b, heap.alloc()).err();
    assert!(matches!(read(&bytes[..HEADER - 1]), Some(TableError::Short { .. })), "shorter than the header");
    assert!(matches!(read(&bytes[..bytes.len() - 1]), Some(TableError::Size { .. })), "truncated rows");
    let mut longer = bytes.clone();
    longer.push(0);
    assert!(matches!(read(&longer), Some(TableError::Size { .. })), "oversized");
    let mut version = bytes.clone();
    version[4..8].copy_from_slice(&(TABLE_VERSION + 1).to_le_bytes());
    assert!(matches!(read(&version), Some(TableError::Version { found }) if found == TABLE_VERSION + 1), "another version");
    let mut magic = bytes.clone();
    magic[0] ^= 1;
    assert!(matches!(read(&magic), Some(TableError::Magic)), "another magic");
    let mut many = bytes.clone();
    many[8..12].copy_from_slice(&(MOST_ROWS + 1).to_le_bytes());
    assert!(matches!(read(&many), Some(TableError::TooMany { what: "crate rows", .. })), "past the cap");
    assert!(read(&bytes).is_none(), "control: the table itself reads");
}

/// A forward walk that cannot key `main` today (its file's `Owners` not reached: `NotToday`)
/// takes `main`'s row from the last answer's table as it stands: its key and verdict. CONTROL:
/// with no `previous` it stays `NotToday`.
#[test]
fn a_body_the_walk_did_not_reach_is_carried_from_the_last_table() {
    let main = main_body().item;
    let last = ids(ToolchainId([7; 32]), &table([1; 32]));
    let walked = ids_after(ToolchainId([7; 32]), &table([1; 32]), &outs(false), Some(&last));
    assert_eq!(walked.checked(main), last.checked(main), "the last answer's verdict stands");
    assert_eq!(walked.body_check(main).map(|b| b.key), last.body_check(main).map(|b| b.key), "and its key");
    let alone = ids_with(ToolchainId([7; 32]), &table([1; 32]), &outs(false));
    assert!(matches!(alone.checked(main), Some(Checked::Absent(Missing::NotToday { .. }))), "control");
}

/// An answer held in memory: each known name's parts and payload.
struct Held(std::vec::Vec<(Name, std::vec::Vec<blaze_drive::Part>, std::vec::Vec<u8>)>);

impl Answer for Held {
    fn open(&mut self, _cx: &Cx, name: Name, seen: &mut dyn FnMut(Seen<'_>) -> Result<(), RunIdsError<'static>>) -> Result<bool, RunIdsError<'static>> {
        let Some((_, parts, payload)) = self.0.iter().find(|(n, _, _)| *n == name) else { return Ok(false) };
        for part in parts {
            seen(Seen::Part(*part))?;
        }
        seen(Seen::Payload(payload))?;
        Ok(true)
    }
}

fn part(name: Name, locate: blaze_drive::Locate) -> blaze_drive::Part {
    blaze_drive::Part { name, locate, place: 0 }
}

/// A `BodyCheck` payload of work.rs's form: `file`, `text` and the sorted `keys`.
fn payload(file: Name, text: ContentId, keys: &[ItemKey]) -> std::vec::Vec<u8> {
    let mut out = blaze_own::Vec::new_in(std::alloc::Global);
    crate::work::write_body_payload(file, text, keys, &mut out, keys.len() as u32);
    out.to_vec()
}

const OUTER: u64 = 10;
const INNER: u64 = 11;
const HELPER: u64 = 12;

/// The two-file fixture: the root units list `app`'s units, whose crate root `src/main.rs`
/// holds `fn outer() { fn inner() {} }` (one `BodyCheck`, covering both) and `mod util;`, and
/// `src/util.rs` holds `fn helper()` (its own check) beside an `include_str!` data file and a
/// body whose check filed a failure (an empty payload).
struct Fixture {
    answer: Held,
    main_rs: Name,
    util_rs: Name,
    outer_check: Name,
    helper_check: Name,
}

fn fixture() -> Fixture {
    let units = Name::units();
    let app = Name::manifest(b"app/Cargo.toml");
    let (main_rs, util_rs, data) = (Name::file(b"app/src/main.rs"), Name::file(b"app/src/util.rs"), Name::file(b"app/src/words.txt"));
    let outer_check = Name::new(Kind::BodyCheck, ContentId([0x71; 32]));
    let helper_check = Name::new(Kind::BodyCheck, ContentId([0x72; 32]));
    let failed_check = Name::new(Kind::BodyCheck, ContentId([0x73; 32]));
    let rust = blaze_drive::Locate::File { edition: 2 };
    let source = blaze_drive::Locate::Source(ContentId([0x70; 32]));
    let answer = Held(std::vec![
        (units, std::vec![part(app, source)], std::vec::Vec::new()),
        (app, std::vec![part(main_rs, rust)], std::vec::Vec::new()),
        (main_rs, std::vec![part(outer_check, source), part(util_rs, rust), part(Name::new(Kind::Surface, main_rs.subject), source)], std::vec::Vec::new()),
        (
            util_rs,
            std::vec![part(helper_check, source), part(failed_check, source), part(data, blaze_drive::Locate::File { edition: blaze_drive::record::DATA_FILE })],
            std::vec::Vec::new()
        ),
        (outer_check, std::vec::Vec::new(), payload(main_rs, ContentId([0x81; 32]), &[item(APP, OUTER), item(APP, INNER)])),
        (helper_check, std::vec::Vec::new(), payload(util_rs, ContentId([0x82; 32]), &[item(APP, HELPER)])),
        (failed_check, std::vec::Vec::new(), std::vec::Vec::new()),
    ]);
    Fixture { answer, main_rs, util_rs, outer_check, helper_check }
}

/// The rows a walk found, sorted by item.
fn rows_of(walk: &Walked<'_>) -> std::vec::Vec<WorkspaceBody> {
    let mut rows = walk.bodies.to_vec();
    rows.sort_by_key(|row| row.item);
    rows
}

/// A fn with a nested fn is checked: the table built from the answer has both checker keys
/// under the one `BodyCheck` (its text and file from its record), so both get its key; the
/// other file's body has its own. The data file is not walked, the failed check covers nothing.
/// CONTROL: a key no record names has no row.
#[test]
fn a_nested_fn_is_keyed_by_the_check_that_covered_it() {
    let io = Io::simulated(SimWorld::new(3, Limits::USUAL));
    let cx = io.cx(bstd::test::BUDGET, bstd::mem::TASK_FRAME).expect("a root context");
    let heap = blaze_own::testing::heap();
    let mut f = fixture();
    let walk = workspace_of(&cx, &mut f.answer, None, heap.alloc()).unwrap_or_else(|e| panic!("{e}"));
    let (outer_text, helper_text) = (ContentId([0x81; 32]), ContentId([0x82; 32]));
    assert_eq!(
        rows_of(&walk),
        std::vec![
            WorkspaceBody { item: item(APP, OUTER), check: f.outer_check, text: outer_text, file: f.main_rs },
            WorkspaceBody { item: item(APP, INNER), check: f.outer_check, text: outer_text, file: f.main_rs },
            WorkspaceBody { item: item(APP, HELPER), check: f.helper_check, text: helper_text, file: f.util_rs },
        ],
        "both of main.rs's fns under its one check, util.rs's under its own"
    );
    assert!(walk.edges.iter().all(|(_, child)| *child != Name::file(b"app/src/words.txt")), "the data file is not walked");

    let mut outs = std::vec![(Name::unit_graph(), GRAPH_OUT), (blaze_drive::key::owners_of(f.main_rs), OWNERS_OUT), (blaze_drive::key::owners_of(f.util_rs), OWNERS_OUT)];
    outs.sort_by_key(|(name, _)| *name);
    let today = SortedToday::new(&outs).unwrap_or_else(|e| panic!("{e}"));
    let tools = tools();
    let ids = RunIds::new(&cx, ToolchainId([7; 32]), &table([1; 32]), &tools, &walk.bodies, &walk.edges, &today, None, &mut Empty, heap.alloc()).unwrap_or_else(|e| panic!("{e}"));
    let key = |low: u64| ids.body_check(item(APP, low)).map(|b| b.key);
    let tool = tools[Kind::BodyCheck.index()];
    let outer_key = crate::body_types::body_check_key(tool, f.outer_check, outer_text, f.main_rs, GRAPH_OUT, OWNERS_OUT).0;
    assert_eq!((key(OUTER), key(INNER)), (Some(outer_key), Some(outer_key)), "the nested fn is keyed by the check that covered it");
    assert_ne!(key(HELPER), Some(outer_key), "another check, another key");
    assert!(key(HELPER).is_some());
    let back = RunIds::read(&ids.write(heap.alloc()), heap.alloc()).map(|t| t.workspace_rows(heap.alloc()).to_vec()).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(back, rows_of(&walk), "the table names each row's check, text and file");
    assert_eq!(key(13), None, "control: a fn no record covers has no row");
}

/// A forward walk that knows only `main.rs` and its check carries the rest from the last
/// answer's table (its edges and its rows of `helper`'s check) and finds the same bodies.
/// CONTROL: with no last table the first unknown record is refused by name.
#[test]
fn a_forward_walk_crosses_what_it_did_not_reach_by_the_last_table() {
    let io = Io::simulated(SimWorld::new(3, Limits::USUAL));
    let cx = io.cx(bstd::test::BUDGET, bstd::mem::TASK_FRAME).expect("a root context");
    let heap = blaze_own::testing::heap();
    let tools = tools();
    let mut f = fixture();
    let full = workspace_of(&cx, &mut f.answer, None, heap.alloc()).unwrap_or_else(|e| panic!("{e}"));
    let known = outs(false);
    let today = SortedToday::new(&known).unwrap_or_else(|e| panic!("{e}"));
    let last =RunIds::new(&cx, ToolchainId([7; 32]), &table([1; 32]), &tools, &full.bodies, &full.edges, &today, None, &mut Empty, heap.alloc()).unwrap_or_else(|e| panic!("{e}"));
    let mut forward = Held(f.answer.0.into_iter().filter(|(name, _, _)| *name == f.main_rs || *name == f.outer_check).collect());
    let walk = workspace_of(&cx, &mut forward, Some(&last), heap.alloc()).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(rows_of(&walk), rows_of(&full), "the same bodies");
    let mut edges = walk.edges.to_vec();
    edges.sort_unstable();
    assert_eq!(edges, full.edges.to_vec(), "the same tree");
    match workspace_of(&cx, &mut forward, None, heap.alloc()) {
        Err(RunIdsError::NotWalked { name }) => assert_eq!(name, Name::units()),
        Err(other) => panic!("refused otherwise: {other}"),
        Ok(_) => panic!("control: a walk with no last table took unknown records as empty"),
    }
}