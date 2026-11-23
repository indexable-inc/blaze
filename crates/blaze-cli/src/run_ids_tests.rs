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
    fn toolchain_crates(&mut self, _subjects: &[ContentId]) {}
    fn keep_with_toolchain(&mut self, _key: Key) -> Result<(), CliError> {
        Ok(())
    }
    fn index_beside(&mut self, _cx: &Cx, _owner: Key, _keys: &[Key]) -> Result<(), CliError> {
        Ok(())
    }
    fn put_parts(&mut self, _cx: &Cx, _key: Key, _parts: &[&[u8]]) -> Result<(), CliError> {
        Ok(())
    }
    fn put_record(&mut self, _cx: &Cx, _key: Key, _tag: blaze_keyed::kind::Tag, _owner: &[&[u8]], _bytes: &[u8]) -> Result<blaze_keyed::store::Put, CliError> {
        Ok(blaze_keyed::store::Put::Written)
    }
    fn kept(&self) -> crate::store::StoreKept {
        crate::store::StoreKept::default()
    }
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
    let tree = [Edge::Walked(Name::units()), Edge::Part { parent: Name::units(), child: main_body().file }];
    RunIds::new(&cx, toolchain, table, &tools, &[main_body()], &tree, &mut Looked(today), previous, Most::USUAL, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("{e}"))
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
/// parent refuses it; its key is the one the answer hands ([`Answer::body`]: here
/// [`looked_key`], the crate list and the file's owners folded in), never a guess. An item of
/// the checkout the caller did not name has no `BodyId` at all.
#[test]
fn a_workspace_item_with_no_record_has_empty_reads() {
    let ids = ids(ToolchainId([7; 32]), &table([1; 32]));
    let main = main_body();
    let body = ids.body_check(main.item).expect("app's main is named");
    assert!(body.reads.is_empty(), "no record: no reads");
    let tool = tools()[Kind::BodyCheck.index()];
    let key = looked_key(tool, main.check, main.text, main.file, GRAPH_OUT, OWNERS_OUT);
    assert_eq!(body.key, key, "the key is the answer's own BodyCheck key");
    let other = looked_key(tool, main.check, main.text, main.file, GRAPH_OUT, GRAPH_OUT);
    assert_ne!(body.key, other, "control: another owners out is another key, so the key is the answer's for today");
    assert_eq!(ids.missing(main.item), Some(Missing::NoRecord));
    assert_eq!(ids.checked(main.item), Some(Checked::Absent(Missing::NoRecord)));
    assert_eq!(ids.grain(main.item), Some(Grain::Workspace));
    assert!(ids.body_check(item(APP, 2)).is_none(), "an unnamed workspace body has no id");
    assert_eq!(ids.crate_fact(main.item), None, "no CrateScope known today: no crate fact");
}

/// What the table holds beside a fn it has no row for (`no_row`, the parent's refusal words,
/// rule 19) tells the two gaps apart. A fn of `app` with no row (`main`'s sibling, a local fn
/// no covers list): `app` is a workspace crate row, with one body row. A fn under a prefix the
/// table has no workspace row for (another unit's key, D152): that prefix is none of the rows.
/// CONTROL: `main` itself has a row, so the gate never asks.
#[test]
fn a_fn_with_no_row_says_which_gap_it_is() {
    let ids = ids(ToolchainId([7; 32]), &table([1; 32]));
    let main = main_body();
    assert!(ids.checked(main.item).is_some(), "control: main has a row");
    let sibling = ids.no_row(item(APP, 2));
    assert_eq!((sibling.workspace, sibling.named, sibling.crates[0]), (1, 1, [APP; 8]), "the one workspace crate row: app");
    assert_eq!((sibling.called_held, sibling.under_called), (true, 1));
    let shown = std::format!("{sibling}");
    assert!(shown.contains("with 1 body row(s), none this fn's"), "{shown}");
    let other_unit = ids.no_row(item(0x32, 1));
    assert_eq!((other_unit.called_held, other_unit.under_called), (false, 0));
    let shown = std::format!("{other_unit}");
    assert!(shown.contains("is none of them"), "{shown}");
}

/// THE GATE BY GRAIN (states.md item 13): a toolchain body and a locked body each get their
/// grain's proof, naming them; a workspace body with no record is refused with its verdict; a
/// workspace fn with no row is refused with what the table holds; a fn of no crate of the list
/// is `NoGrain` naming it. CAUSE REMOVED (rule 23): the same `NoRecord` verdict under the
/// planted test proof runs, so the refusal is the verdict's, not the item's.
#[test]
fn the_gate_gives_each_grain_its_own_proof() {
    let ids = ids(ToolchainId([7; 32]), &table([1; 32]));
    let symbol = blaze_launch::wire::run::Symbol([0; 32]);
    let std_fn = item(STD, 42);
    let toolchain = ids.runnable(std_fn).unwrap_or_else(|why| panic!("a toolchain body runs: {why}"));
    assert!(toolchain.covers(std_fn, symbol) && !toolchain.covers(item(STD, 43), symbol), "it names its body");
    let locked = ids.runnable(item(DEP_A, 9)).unwrap_or_else(|why| panic!("a locked body runs: {why}"));
    assert_ne!(locked, toolchain, "each grain's own maker");
    let main = main_body();
    assert_eq!(ids.runnable(main.item), Err(NotRunnable::Checked(Checked::Absent(Missing::NoRecord))));
    assert!(matches!(ids.runnable(item(APP, 2)), Err(NotRunnable::NoRow(held)) if held.called_held), "a fn of app with no row");
    let stray = item(0x32, 1);
    assert_eq!(ids.runnable(stray), Err(NotRunnable::NoGrain { function: stray }));
    assert!(Checked::Clean.runs(main.item).is_ok_and(|proof| proof.covers(main.item, symbol)), "control: a clean verdict runs");
}

/// An `Errors` verdict with no error is no verdict (finding 13): the table's read refuses that
/// row by name. CONTROL: the same row with one error reads as `Errors` of one.
#[test]
fn an_errors_row_with_no_error_is_refused() {
    let heap = blaze_own::testing::heap();
    let mut bytes = ids(ToolchainId([7; 32]), &table([1; 32])).write(heap.alloc()).to_vec();
    let crates = u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]) as usize;
    let row = HEADER + crates * CRATE_ROW;
    bytes[row + 56] = VerdictTag::Errors as u8;
    bytes[row + 60..row + 64].copy_from_slice(&0u32.to_le_bytes());
    assert!(matches!(RunIds::read(&bytes, blaze_own::testing::bump()), Err(TableError::NoErrors { at: 0 })), "no error: refused");
    bytes[row + 60..row + 64].copy_from_slice(&1u32.to_le_bytes());
    let read = RunIds::read(&bytes, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(read.checked(main_body().item), Some(Checked::Errors { errors: NonZeroU32::MIN, first: 0 }), "control: one error");
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
    match RunIds::new(&cx, ToolchainId([7; 32]), &t, &tools, &[], &[], &mut Looked(SortedToday::EMPTY), None, Most::USUAL, blaze_own::testing::bump()) {
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
        let back = RunIds::read(&bytes, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("{e}"));
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
        fn toolchain_crates(&mut self, _subjects: &[ContentId]) {}
        fn keep_with_toolchain(&mut self, _key: Key) -> Result<(), CliError> {
            Ok(())
        }
        fn index_beside(&mut self, _cx: &Cx, _owner: Key, _keys: &[Key]) -> Result<(), CliError> {
            Ok(())
        }
        fn put_parts(&mut self, _cx: &Cx, key: Key, parts: &[&[u8]]) -> Result<(), CliError> {
            self.0.push((key, parts.concat()));
            Ok(())
        }
        fn put_record(&mut self, _cx: &Cx, key: Key, _tag: blaze_keyed::kind::Tag, _owner: &[&[u8]], bytes: &[u8]) -> Result<blaze_keyed::store::Put, CliError> {
            self.0.push((key, bytes.to_vec()));
            Ok(blaze_keyed::store::Put::Written)
        }
        fn kept(&self) -> crate::store::StoreKept {
            let bytes = self.0.iter().map(|(_, bytes)| bytes.len() as u64).sum();
            crate::store::StoreKept { copies: self.0.len() as u64, copy_bytes: bytes, held_bytes: bytes }
        }
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
    match RunIds::from_record(&cx, &mut Empty, cert, ToolchainId([7; 32]), &tools, blaze_own::testing::bump()) {
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
    let read = |b: &[u8]| RunIds::read(b, blaze_own::testing::bump()).err();
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

/// What a `BodyCheck` task of a held answer covered: its file, its entry's text and the checker
/// keys, sorted (what engine_body_facts.rs files for the engine's answer).
struct Covered {
    file: Name,
    text: ContentId,
    keys: std::vec::Vec<ItemKey>,
}

/// An answer held in memory: each known name's parts, and a `BodyCheck`'s cover (`None`: the
/// check filed a failure, or the name is no check). It says no file is skipped ([`Skipping`]
/// says some are).
struct Held(std::vec::Vec<(Name, std::vec::Vec<blaze_drive::Part>, Option<Covered>)>);

impl Today for Held {
    fn out(&self, _name: Name) -> Option<ContentId> {
        None
    }
    fn cell(&self, _name: Name, _cell: Cell) -> Option<ContentId> {
        None
    }
}

impl Answer for Held {
    fn open(&mut self, _cx: &Cx, name: Name, seen: &mut dyn FnMut(Seen<'_>) -> Result<(), RunIdsError<'static>>) -> Result<bool, RunIdsError<'static>> {
        let Some((_, parts, _)) = self.0.iter().find(|(n, _, _)| *n == name) else { return Ok(false) };
        for part in parts {
            seen(Seen::Part(*part))?;
        }
        seen(Seen::Payload(&[]))?;
        Ok(true)
    }

    fn skipped(&self, _file: Name) -> bool {
        false
    }

    /// The join only: the check's cover names the file, text and covered keys; no key, no
    /// reads (this answer holds no checked record).
    fn body(
        &mut self,
        _cx: &Cx,
        check: Name,
        _tool: Tool,
        _known: Option<(ContentId, Name)>,
        out: &mut BodyScratch<'_>,
    ) -> Result<Option<BodyFacts>, RunIdsError<'static>> {
        out.clear();
        let Some((_, _, covered)) = self.0.iter().find(|(n, _, _)| *n == check) else { return Ok(None) };
        let none = BodyFacts { key: ABSENT, checked: Checked::Absent(Missing::NoRecord), text: ABSENT, file: check };
        let Some(covered) = covered else { return Ok(Some(none)) };
        out.covers.extend(covered.keys.iter().copied());
        Ok(Some(BodyFacts { text: covered.text, file: covered.file, ..none }))
    }
}

/// The key [`Looked`] hands for a body known today: the answer's own (the engine's task row
/// key), here a hash of the check, its text and file, and today's `UnitGraph` and `Owners` outs.
fn looked_key(tool: Tool, check: Name, text: ContentId, file: Name, graph: ContentId, owners: ContentId) -> ContentId {
    let mut h = Hasher::new("blaze-cli run ids tests looked key 1");
    h.id(&tool.0).update(&[check.kind as u8]).id(&check.subject).id(&text).update(&[file.kind as u8]).id(&file.subject).id(&graph).id(&owners);
    h.finish()
}

/// `RunIds::new`'s answer: today's outs, and no checked record. A body whose `UnitGraph` or
/// file's `Owners` out is not known today is `NotToday` naming it; one with both is keyed
/// ([`looked_key`]) and `NoRecord`.
struct Looked<'t>(SortedToday<'t>);

impl Today for Looked<'_> {
    fn out(&self, name: Name) -> Option<ContentId> {
        self.0.out(name)
    }
    fn cell(&self, name: Name, cell: Cell) -> Option<ContentId> {
        self.0.cell(name, cell)
    }
}

impl Answer for Looked<'_> {
    fn open(&mut self, _cx: &Cx, _name: Name, _seen: &mut dyn FnMut(Seen<'_>) -> Result<(), RunIdsError<'static>>) -> Result<bool, RunIdsError<'static>> {
        Ok(false)
    }

    fn skipped(&self, _file: Name) -> bool {
        false
    }

    fn body(
        &mut self,
        _cx: &Cx,
        check: Name,
        tool: Tool,
        known: Option<(ContentId, Name)>,
        out: &mut BodyScratch<'_>,
    ) -> Result<Option<BodyFacts>, RunIdsError<'static>> {
        out.clear();
        let Some((text, file)) = known else { return Ok(None) };
        let (graph, owners) = (Name::unit_graph(), blaze_drive::key::owners_of(file));
        let (key, checked) = match (self.0.out(graph), self.0.out(owners)) {
            (Some(g), Some(o)) => (looked_key(tool, check, text, file, g, o), Checked::Absent(Missing::NoRecord)),
            (None, _) => (ABSENT, Checked::Absent(Missing::NotToday { name: graph })),
            (Some(_), None) => (ABSENT, Checked::Absent(Missing::NotToday { name: owners })),
        };
        Ok(Some(BodyFacts { key, checked, text, file }))
    }
}

fn part(name: Name, locate: blaze_drive::Locate) -> blaze_drive::Part {
    blaze_drive::Part { name, locate, place: 0 }
}

/// A `BodyCheck` task's cover: `file`, `text` and the sorted `keys`.
fn payload(file: Name, text: ContentId, keys: &[ItemKey]) -> Option<Covered> {
    Some(Covered { file, text, keys: keys.to_vec() })
}

const OUTER: u64 = 10;
const INNER: u64 = 11;
const HELPER: u64 = 12;

/// The two-file fixture: the root units list `app`'s units, whose crate root `src/main.rs`
/// holds `fn outer() { fn inner() {} }` (one `BodyCheck`, covering both) and `mod util;`, and
/// `src/util.rs` holds `fn helper()` (its own check) beside an `include_str!` data file and a
/// body whose check filed a failure (no cover).
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
        (units, std::vec![part(app, source)], None),
        (app, std::vec![part(main_rs, rust)], None),
        (main_rs, std::vec![part(outer_check, source), part(util_rs, rust), part(Name::new(Kind::Surface, main_rs.subject), source)], None),
        (
            util_rs,
            std::vec![part(helper_check, source), part(failed_check, source), part(data, blaze_drive::Locate::File { edition: blaze_drive::record::DATA_FILE })],
            None
        ),
        (outer_check, std::vec::Vec::new(), payload(main_rs, ContentId([0x81; 32]), &[item(APP, OUTER), item(APP, INNER)])),
        (helper_check, std::vec::Vec::new(), payload(util_rs, ContentId([0x82; 32]), &[item(APP, HELPER)])),
        (failed_check, std::vec::Vec::new(), None),
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
    let walk = workspace_of(&cx, &mut f.answer, tools()[Kind::BodyCheck.index()], &[], None, heap.alloc()).unwrap_or_else(|e| panic!("{e}"));
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
    assert!(walk.edges.iter().all(|edge| edge.pair().1 != Name::file(b"app/src/words.txt")), "the data file is not walked");

    let mut outs = std::vec![(Name::unit_graph(), GRAPH_OUT), (blaze_drive::key::owners_of(f.main_rs), OWNERS_OUT), (blaze_drive::key::owners_of(f.util_rs), OWNERS_OUT)];
    outs.sort_by_key(|(name, _)| *name);
    let today = SortedToday::new(&outs).unwrap_or_else(|e| panic!("{e}"));
    let tools = tools();
    let ids = RunIds::new(&cx, ToolchainId([7; 32]), &table([1; 32]), &tools, &walk.bodies, &walk.edges, &mut Looked(today), None, Most::USUAL, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("{e}"));
    let key = |low: u64| ids.body_check(item(APP, low)).map(|b| b.key);
    let tool = tools[Kind::BodyCheck.index()];
    let outer_key = looked_key(tool, f.outer_check, outer_text, f.main_rs, GRAPH_OUT, OWNERS_OUT);
    assert_eq!((key(OUTER), key(INNER)), (Some(outer_key), Some(outer_key)), "the nested fn is keyed by the check that covered it");
    assert_ne!(key(HELPER), Some(outer_key), "another check, another key");
    assert!(key(HELPER).is_some());
    let back = RunIds::read(&ids.write(heap.alloc()), blaze_own::testing::bump()).map(|t| t.workspace_rows(heap.alloc()).to_vec()).unwrap_or_else(|e| panic!("{e}"));
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
    let tool = tools[Kind::BodyCheck.index()];
    let full = workspace_of(&cx, &mut f.answer, tool, &[], None, heap.alloc()).unwrap_or_else(|e| panic!("{e}"));
    let known = outs(false);
    let today = SortedToday::new(&known).unwrap_or_else(|e| panic!("{e}"));
    let last = RunIds::new(&cx, ToolchainId([7; 32]), &table([1; 32]), &tools, &full.bodies, &full.edges, &mut Looked(today), None, Most::USUAL, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("{e}"));
    let mut forward = Held(f.answer.0.into_iter().filter(|(name, _, _)| *name == f.main_rs || *name == f.outer_check).collect());
    let walk = workspace_of(&cx, &mut forward, tool, &[], Some(&last), heap.alloc()).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(rows_of(&walk), rows_of(&full), "the same bodies");
    let mut edges = walk.edges.to_vec();
    edges.sort_unstable();
    assert_eq!(edges, full.edges.to_vec(), "the same tree");
    match workspace_of(&cx, &mut forward, tool, &[], None, heap.alloc()) {
        Err(RunIdsError::NotWalked { name }) => assert_eq!(name, Name::units()),
        Err(other) => panic!("refused otherwise: {other}"),
        Ok(_) => panic!("control: a walk with no last table took unknown records as empty"),
    }
}

/// [`Held`] with the files the answer says the check planned no body task of
/// (`Answer::skipped`: engine_check.rs `PLACE_SKIPPED`, a features-skipped target's files).
struct Skipping {
    held: Held,
    skipped: std::vec::Vec<Name>,
}

impl Today for Skipping {
    fn out(&self, name: Name) -> Option<ContentId> {
        self.held.out(name)
    }
    fn cell(&self, name: Name, cell: Cell) -> Option<ContentId> {
        self.held.cell(name, cell)
    }
}

impl Answer for Skipping {
    fn open(&mut self, cx: &Cx, name: Name, seen: &mut dyn FnMut(Seen<'_>) -> Result<(), RunIdsError<'static>>) -> Result<bool, RunIdsError<'static>> {
        self.held.open(cx, name, seen)
    }
    fn skipped(&self, file: Name) -> bool {
        self.skipped.contains(&file)
    }
    fn body(&mut self, cx: &Cx, check: Name, tool: Tool, known: Option<(ContentId, Name)>, out: &mut BodyScratch<'_>) -> Result<Option<BodyFacts>, RunIdsError<'static>> {
        self.held.body(cx, check, tool, known, out)
    }
}

/// THE TEMPO CASE (2026-11-21, binary c0545wave91: `blaze test -p tempo-hardfork` after a whole
/// `blaze check`, exit 125, "the walk reached BodyCheck(00a930d79f39), which this request did not
/// settle and the last answer's body id table did not walk"): a member's units record lists
/// every target root, a bin whose `required-features` are off too (tempo's `localnet`); the check
/// outlines its files and plans no body task of them, so its `BodyCheck` parts have no row. The
/// answer says so (`Answer::skipped`), and the walk keeps the bin's root as a part of the units
/// and does not open it: the bodies are the fixture's own. PLANTED: the same answer not saying
/// it (a skipped file's place lost, or a standing body row dropped) is the typed refusal naming
/// the body part, never a quiet skip (rule 3).
#[test]
fn a_features_skipped_targets_bodies_are_not_walked() {
    let io = Io::simulated(SimWorld::new(3, Limits::USUAL));
    let cx = io.cx(bstd::test::BUDGET, bstd::mem::TASK_FRAME).expect("a root context");
    let heap = blaze_own::testing::heap();
    let tool = tools()[Kind::BodyCheck.index()];
    let mut plain = fixture();
    let full = workspace_of(&cx, &mut plain.answer, tool, &[], None, heap.alloc()).unwrap_or_else(|e| panic!("{e}"));
    // The fixture again, its units listing a skipped bin root whose one body has no row.
    let app = Name::manifest(b"app/Cargo.toml");
    let extra = Name::file(b"app/src/bin/extra.rs");
    let unchecked = Name::new(Kind::BodyCheck, ContentId([0x74; 32]));
    let source = blaze_drive::Locate::Source(ContentId([0x70; 32]));
    let with_bin = || {
        let mut f = fixture();
        for (name, parts, _) in f.answer.0.iter_mut() {
            if *name == app {
                parts.push(part(extra, blaze_drive::Locate::File { edition: 2 }));
            }
        }
        f.answer.0.push((extra, std::vec![part(unchecked, source)], None));
        f.answer
    };
    let mut skipping = Skipping { held: with_bin(), skipped: std::vec![extra] };
    let walk = workspace_of(&cx, &mut skipping, tool, &[], None, heap.alloc()).unwrap_or_else(|e| panic!("the skipped bin stopped the walk: {e}"));
    assert_eq!(rows_of(&walk), rows_of(&full), "the bodies are the checked targets' alone");
    assert!(walk.edges.contains(&Edge::Part { parent: app, child: extra }), "the skipped root stays a part of its units");
    assert!(!walk.edges.contains(&Edge::Walked(extra)), "the skipped root is not walked");
    assert!(walk.edges.iter().all(|edge| edge.pair().1 != unchecked), "no body part of it is listed");
    // PLANTED: the answer does not say the bin is skipped: its row-less body part is refused by name.
    let mut silent = Skipping { held: with_bin(), skipped: std::vec::Vec::new() };
    match workspace_of(&cx, &mut silent, tool, &[], None, heap.alloc()) {
        Err(RunIdsError::NotWalked { name }) => assert_eq!(name, unchecked, "the refusal names the body part with no row"),
        Err(other) => panic!("refused otherwise: {other}"),
        Ok(_) => panic!("planted: a body part with no row was walked past"),
    }
}

// --- Each unit's rows at its view (D191) ---

const LIB: u8 = 0x41;
const LIB_TEST: u8 = 0x42;
const BIN: u8 = 0x43;

fn unit(byte: u8, root: Name) -> UnitRoot {
    let key = CrateItemKey(ContentId([byte; 32]));
    UnitRoot { root, view: unit_view(byte), prefix: key.prefix() }
}

/// The view of the unit `byte` (blaze-drive view.rs `ViewKey::of_member`: its own root, the
/// package's name, the test flag of `LIB_TEST`).
fn unit_view(byte: u8) -> ViewKey {
    ViewKey::of_member(&[byte], b"pkg", byte == LIB_TEST)
}

/// `check`'s name at the view of the unit `byte`.
fn view_name(check: Name, byte: u8) -> Name {
    at_view(check, UnitView::At(unit_view(byte)))
}

/// The package of three units D191 plans rows for: the library `pkg` (root `src/lib.rs`, which
/// holds `f` and `mod shared;`), its test build (the same root), and a bin (root `src/main.rs`,
/// which holds `main` and `mod shared;` too); `src/shared.rs` holds `g`. The rows `Full` plans
/// (rank: the lib, the bin, then the test build): `f` plain under the lib and at the test
/// build's view; `g` plain under the lib, at the bin's view and at the test build's; `main`
/// plain under the bin. `g`'s test-build row covers its own text (`cfg!(test)` read in it).
/// `planned`: false is the rule before D191 (the first holder checks alone): plain rows only.
struct Views {
    answer: Held,
    units: std::vec::Vec<UnitRoot>,
    f: Name,
    g: Name,
    main: Name,
}

fn views(planned: bool) -> Views {
    let rust = blaze_drive::Locate::File { edition: 2 };
    let source = blaze_drive::Locate::Source(ContentId([0x70; 32]));
    let pkg = Name::manifest(b"pkg/Cargo.toml");
    let (lib_rs, main_rs, shared_rs) = (Name::file(b"pkg/src/lib.rs"), Name::file(b"pkg/src/main.rs"), Name::file(b"pkg/src/shared.rs"));
    let (f, g, main) = (Name::new(Kind::BodyCheck, ContentId([0x91; 32])), Name::new(Kind::BodyCheck, ContentId([0x92; 32])), Name::new(Kind::BodyCheck, ContentId([0x93; 32])));
    let units = std::vec![unit(LIB, lib_rs), unit(LIB_TEST, lib_rs), unit(BIN, main_rs)];
    let at = view_name;
    let mut rows = std::vec![
        (Name::units(), std::vec![part(pkg, source)], None),
        (pkg, std::vec![part(lib_rs, rust), part(main_rs, rust)], None),
        (lib_rs, std::vec![part(f, source), part(shared_rs, rust)], None),
        (main_rs, std::vec![part(main, source), part(shared_rs, rust)], None),
        (shared_rs, std::vec![part(g, source)], None),
        (f, std::vec::Vec::new(), payload(lib_rs, ContentId([0xa1; 32]), &[item(LIB, 1)])),
        (g, std::vec::Vec::new(), payload(shared_rs, ContentId([0xa2; 32]), &[item(LIB, 2)])),
        (main, std::vec::Vec::new(), payload(main_rs, ContentId([0xa3; 32]), &[item(BIN, 3)])),
    ];
    if planned {
        rows.push((at(f, LIB_TEST), std::vec::Vec::new(), payload(lib_rs, ContentId([0xa1; 32]), &[item(LIB_TEST, 1)])));
        rows.push((at(g, BIN), std::vec::Vec::new(), payload(shared_rs, ContentId([0xa2; 32]), &[item(BIN, 2)])));
        rows.push((at(g, LIB_TEST), std::vec::Vec::new(), payload(shared_rs, ContentId([0xb2; 32]), &[item(LIB_TEST, 2)])));
    }
    Views { answer: Held(rows), units, f, g, main }
}

/// D191: every unit that holds a body has a row of its own, found at its view. The test build's
/// `f` and `g` (what a `#[cfg(test)] mod tests` test calls through the test build's keys) and
/// the bin's `g` (a file a lib and a bin both `mod`) each have a row under that unit's prefix,
/// named at its view; each base row stays under its own unit. PLANTED (the rule before D191, the
/// first holder checks alone): the test build's and the bin's keys have no row, and the base
/// rows (the library's keys) are not taken for them. CONTROL: without the units the walk reads
/// the plain rows alone, which this fixture's planned rows do not change.
#[test]
fn each_unit_that_holds_a_body_has_its_own_row_at_its_view() {
    let io = Io::simulated(SimWorld::new(3, Limits::USUAL));
    let cx = io.cx(bstd::test::BUDGET, bstd::mem::TASK_FRAME).expect("a root context");
    let heap = blaze_own::testing::heap();
    let tool = tools()[Kind::BodyCheck.index()];
    let mut v = views(true);
    let walk = workspace_of(&cx, &mut v.answer, tool, &v.units, None, heap.alloc()).unwrap_or_else(|e| panic!("{e}"));
    let rows = rows_of(&walk);
    let row = |key: ItemKey| rows.iter().find(|row| row.item == key).map(|row| row.check);
    let at = view_name;
    assert_eq!(row(item(LIB, 1)), Some(v.f), "the library's f: the base row, plain");
    assert_eq!(row(item(LIB_TEST, 1)), Some(at(v.f, LIB_TEST)), "the test build's f: at its view");
    assert_eq!(row(item(LIB, 2)), Some(v.g), "the library's g: the base row");
    assert_eq!(row(item(BIN, 2)), Some(at(v.g, BIN)), "the bin's g: at its view");
    assert_eq!(row(item(LIB_TEST, 2)), Some(at(v.g, LIB_TEST)), "the test build's g: at its view");
    assert_eq!(row(item(BIN, 3)), Some(v.main), "the bin's main, one holder: plain");
    assert_eq!(rows.len(), 6, "one row a (body, holder): {rows:?}");
    let g_test = rows.iter().find(|row| row.item == item(LIB_TEST, 2)).map(|row| row.text);
    assert_eq!(g_test, Some(ContentId([0xb2; 32])), "the test build's g is its own check's, not the base's");
    // Every library key is under a plain name (the base rows): none under a view's.
    for row in rows.iter().filter(|row| prefix_of(row.item) == [LIB; 8]) {
        assert!(row.check == v.f || row.check == v.g, "a library key under a view's name: {row:?}");
    }

    // PLANTED: the first holder checks alone. The test build's and the bin's keys have no row;
    // nothing of the library's is taken for them.
    let mut old = views(false);
    let walk = workspace_of(&cx, &mut old.answer, tool, &old.units, None, heap.alloc()).unwrap_or_else(|e| panic!("{e}"));
    let rows = rows_of(&walk);
    for key in [item(LIB_TEST, 1), item(LIB_TEST, 2), item(BIN, 2)] {
        assert!(rows.iter().all(|row| row.item != key), "planted: {key:?} has a row under the old rule");
    }
    assert_eq!(rows.len(), 3, "the base rows alone: {rows:?}");
    // The table built from those rows: a test-build call is refused with no row at its view.
    let mut held = table([1; 32]);
    held.0.push(Row { byte: LIB, name: "pkg", place: Place::Checkout, deps: &[] });
    held.0.push(Row { byte: LIB_TEST, name: "pkg (test)", place: Place::Checkout, deps: &[] });
    held.0.push(Row { byte: BIN, name: "pkg (bin)", place: Place::Checkout, deps: &[] });
    let today = SortedToday::new(&[]).unwrap_or_else(|e| panic!("{e}"));
    let tools = tools();
    let ids = RunIds::new(&cx, ToolchainId([7; 32]), &held, &tools, &walk.bodies, &walk.edges, &mut Looked(today), None, Most::USUAL, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("{e}"));
    match ids.runnable(item(LIB_TEST, 1)) {
        Err(NotRunnable::NoRow(no_row)) => assert!(std::format!("{no_row}").contains("no row at this unit's view"), "{no_row}"),
        other => panic!("planted: the test build's f was {other:?}"),
    }

    // CONTROL: without the units, the planned answer gives the plain rows alone.
    let mut plain = views(true);
    let walk = workspace_of(&cx, &mut plain.answer, tool, &[], None, heap.alloc()).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(rows_of(&walk).len(), 3, "the plain rows alone");
}

/// A row at a unit's view whose keys are another unit's (a planner that checked the view under
/// the base unit) is refused by name, never taken. CONTROL: the fixture as planned walks.
#[test]
fn a_view_row_of_another_units_keys_is_refused() {
    let io = Io::simulated(SimWorld::new(3, Limits::USUAL));
    let cx = io.cx(bstd::test::BUDGET, bstd::mem::TASK_FRAME).expect("a root context");
    let heap = blaze_own::testing::heap();
    let tool = tools()[Kind::BodyCheck.index()];
    let mut v = views(true);
    let wrong = view_name(v.f, LIB_TEST);
    for (name, _, covered) in v.answer.0.iter_mut() {
        if *name == wrong {
            *covered = payload(Name::file(b"pkg/src/lib.rs"), ContentId([0xa1; 32]), &[item(LIB, 1)]);
        }
    }
    match workspace_of(&cx, &mut v.answer, tool, &v.units, None, heap.alloc()) {
        Err(RunIdsError::ViewOfAnotherUnit { check, unit: at, covered }) => {
            assert_eq!((check, at, covered), (wrong, [LIB_TEST; 8], [LIB; 8]), "the refusal names the view, its unit and the keys' crate");
        }
        Err(other) => panic!("refused otherwise: {other}"),
        Ok(_) => panic!("a view row of the library's keys was taken for the test build"),
    }
    let mut ok = views(true);
    assert!(workspace_of(&cx, &mut ok.answer, tool, &ok.units, None, heap.alloc()).is_ok(), "control");
}

// --- A workspace body's reads, BY NEED (the module doc, case 3) ---

/// The byte of member `at`'s crate key (and of its one body's check, text and file).
fn member(at: usize) -> u8 {
    0x80 + at as u8
}

/// `n` workspace members, each a checkout crate with one body (`item(member(i), 1)`), beside
/// `core`; and those bodies.
fn members(n: usize) -> (Table, std::vec::Vec<WorkspaceBody>) {
    let mut rows = std::vec![Row { byte: CORE, name: "core", place: Place::Toolchain, deps: &[] }];
    let mut bodies = std::vec::Vec::new();
    for at in 0..n {
        let byte = member(at);
        let name: &'static str = std::boxed::Box::leak(std::format!("member{at}").into_boxed_str());
        rows.push(Row { byte, name, place: Place::Checkout, deps: &[0] });
        bodies.push(member_body(byte, 1));
    }
    (Table(rows), bodies)
}

/// A body of the member whose byte is `byte`, its check named by `byte` and `low`.
fn member_body(byte: u8, low: u64) -> WorkspaceBody {
    let mut subject = [byte; 32];
    subject[..8].copy_from_slice(&low.to_le_bytes());
    WorkspaceBody {
        item: item(byte, low),
        check: Name::new(Kind::BodyCheck, ContentId(subject)),
        text: ContentId([byte; 32]),
        file: Name::new(Kind::FileItems, ContentId([byte; 32])),
    }
}

/// An answer that checked every body clean: a body's key is its check's subject, its verdict
/// `Clean`. Its reads are not this seam's (`BodyReads`).
struct Cleanly;

impl Today for Cleanly {
    fn out(&self, _name: Name) -> Option<ContentId> {
        None
    }
    fn cell(&self, _name: Name, _cell: Cell) -> Option<ContentId> {
        None
    }
}

impl Answer for Cleanly {
    fn open(&mut self, _cx: &Cx, _name: Name, _seen: &mut dyn FnMut(Seen<'_>) -> Result<(), RunIdsError<'static>>) -> Result<bool, RunIdsError<'static>> {
        Ok(false)
    }
    fn skipped(&self, _file: Name) -> bool {
        false
    }
    fn body(&mut self, _cx: &Cx, check: Name, _tool: Tool, known: Option<(ContentId, Name)>, out: &mut BodyScratch<'_>) -> Result<Option<BodyFacts>, RunIdsError<'static>> {
        out.clear();
        let Some((text, file)) = known else { return Ok(None) };
        Ok(Some(BodyFacts { key: check.subject, checked: Checked::Clean, text, file }))
    }
}

/// A source of proofs that counts each check it is asked, and has no row of `dropped`. Each
/// row reads `each` cells, every one standing today.
struct Counted {
    asked: core::cell::RefCell<std::vec::Vec<Name>>,
    dropped: Option<Name>,
    each: u32,
}

impl Counted {
    fn new(dropped: Option<Name>) -> Counted {
        Counted { asked: core::cell::RefCell::new(std::vec::Vec::new()), dropped, each: 3 }
    }
}

impl BodyReads for Counted {
    fn proof(&self, check: Name, most: u32) -> Result<Proof, Unread> {
        self.asked.borrow_mut().push(check);
        if self.dropped == Some(check) {
            return Err(Unread::NoRow { check });
        }
        if self.each > most {
            return Err(Unread::TooManyReads { check, count: self.each as usize, most });
        }
        let mut fold = ProofFold::new(check);
        for k in 0..self.each {
            let digest = ContentId([k as u8 ^ check.subject.0[0]; 32]);
            fold.read(&[k as u8; 32], &digest, &digest);
        }
        Ok(fold.finish())
    }
}

/// The table of `n` members, built with the caps `most`.
fn members_ids(n: usize, most: Most) -> Result<RunIds<'static>, RunIdsError<'static>> {
    let io = Io::simulated(SimWorld::new(3, Limits::USUAL));
    let cx = io.cx(bstd::test::BUDGET, bstd::mem::TASK_FRAME).expect("a root context");
    let (table, bodies) = members(n);
    let table: &'static Table = std::boxed::Box::leak(std::boxed::Box::new(table));
    RunIds::new(&cx, ToolchainId([7; 32]), table, &tools(), &bodies, &[], &mut Cleanly, None, most, blaze_own::testing::bump())
}

/// What a run that reaches members 0 and 1 sees, by `R`: each one's gate, its id (key and
/// reads) and today's digest of its one read; and the checks the source was asked.
fn reach_two<R: Reading>(n: usize) -> (std::vec::Vec<(ContentId, std::vec::Vec<Read>)>, u32, usize) {
    let ids = members_ids(n, Most::USUAL).unwrap_or_else(|e| panic!("{e}"));
    let (source, asked) = (Counted::new(None), Asked::default());
    let ids = ids.reading::<R>(ReadsFrom::new(&source, &asked, Most::USUAL)).unwrap_or_else(|e| panic!("{e}"));
    let mut seen = std::vec::Vec::new();
    for at in 0..2 {
        let body = item(member(at), 1);
        let _ran = ids.runnable(body).unwrap_or_else(|why| panic!("member {at} runs: {why}"));
        let id = ids.body_check(body).expect("a member's body has an id");
        assert_eq!(id.reads.len(), 1, "one read: its check's proof");
        assert_eq!(ids.today(&id.reads[0].fact), Some(id.reads[0].digest), "every read stands, so the proof does");
        seen.push((id.key, id.reads.to_vec()));
    }
    let asked_of = source.asked.borrow().len();
    (seen, asked.bodies(), asked_of)
}

/// (a) A run that reaches 2 of the workspace's bodies reads exactly those 2 bodies' reads,
/// whether the workspace has 8 members or 64: the table holds no read, the gate reads by need.
/// CONTROL: the `Eager` strategy (the old table's work) reads every member's, and hands the 2
/// reached bodies the same keys and reads.
#[test]
fn a_run_reads_only_the_bodies_it_reaches() {
    let (small, small_read, small_asked) = reach_two::<ByNeed>(8);
    let (large, large_read, large_asked) = reach_two::<ByNeed>(64);
    assert_eq!((small_read, small_asked), (2, 2), "8 members: the 2 reached");
    assert_eq!((large_read, large_asked), (2, 2), "64 members: still the 2 reached");
    assert_eq!(small, large, "the 2 bodies' ids do not depend on the members around them");
    let (eager, eager_read, eager_asked) = reach_two::<Eager>(64);
    assert_eq!((eager_read, eager_asked), (64, 64), "control: the eager table reads every member's");
    assert_eq!(eager, large, "control: equal keys and reads");
}

/// (b) A body whose row the stored answer no longer has is refused at its call with a typed
/// `Unread::NoRow` naming its check, asked once (not retried); the other body runs. CONTROL:
/// with the row there it runs.
#[test]
fn a_dropped_row_refuses_its_call_only() {
    let dropped = member_body(member(1), 1).check;
    let ids = members_ids(8, Most::USUAL).unwrap_or_else(|e| panic!("{e}"));
    let (source, asked) = (Counted::new(Some(dropped)), Asked::default());
    let ids = ids.reading::<ByNeed>(ReadsFrom::new(&source, &asked, Most::USUAL)).unwrap_or_else(|e| panic!("{e}"));
    let gone = item(member(1), 1);
    assert_eq!(ids.runnable(gone).err(), Some(NotRunnable::Unread(Unread::NoRow { check: dropped })), "refused by name");
    assert_eq!(ids.runnable(gone).err(), Some(NotRunnable::Unread(Unread::NoRow { check: dropped })), "the same refusal again");
    assert!(ids.body_check(gone).is_none(), "no id for a body whose reads cannot be read");
    assert_eq!(source.asked.borrow().iter().filter(|check| **check == dropped).count(), 1, "asked once, never retried");
    assert!(ids.runnable(item(member(0), 1)).is_ok(), "the other body runs");
    let whole = members_ids(8, Most::USUAL).unwrap_or_else(|e| panic!("{e}"));
    let (source, asked) = (Counted::new(None), Asked::default());
    let whole = whole.reading::<ByNeed>(ReadsFrom::new(&source, &asked, Most::USUAL)).unwrap_or_else(|e| panic!("{e}"));
    assert!(whole.runnable(gone).is_ok(), "control: with its row it runs");
}

/// A table handed no source refuses a body whose check runs with `NoSource`, never a guess.
#[test]
fn a_table_with_no_source_reads_nothing() {
    let ids = members_ids(2, Most::USUAL).unwrap_or_else(|e| panic!("{e}"));
    let check = member_body(member(0), 1).check;
    assert_eq!(ids.runnable(item(member(0), 1)).err(), Some(NotRunnable::Unread(Unread::NoSource { check })));
}

/// (d) Past the body cap the table is a typed error naming the crate with the most rows and
/// the count, made before any row; past the reads cap a proof is a typed error at the call.
/// CONTROL: the usual caps build and run the same input.
#[test]
fn a_planted_cap_is_a_typed_error() {
    let io = Io::simulated(SimWorld::new(3, Limits::USUAL));
    let cx = io.cx(bstd::test::BUDGET, bstd::mem::TASK_FRAME).expect("a root context");
    let (table, mut bodies) = members(8);
    bodies.push(member_body(member(2), 2));
    bodies.push(member_body(member(2), 3));
    let tiny = Most { bodies: 4, reads: Most::USUAL.reads };
    match RunIds::new(&cx, ToolchainId([7; 32]), &table, &tools(), &bodies, &[], &mut Cleanly, None, tiny, blaze_own::testing::bump()) {
        Err(RunIdsError::TooManyBodies { krate, rows, count, most }) => {
            assert_eq!((krate, rows, count, most), ("member2", 3, 10, 4), "names the crate, its rows, the count and the cap");
        }
        Err(other) => panic!("refused otherwise: {other}"),
        Ok(_) => panic!("10 bodies past a cap of 4 were taken"),
    }
    let ids = RunIds::new(&cx, ToolchainId([7; 32]), &table, &tools(), &bodies, &[], &mut Cleanly, None, Most::USUAL, blaze_own::testing::bump())
        .unwrap_or_else(|e| panic!("control: {e}"));
    let (source, asked) = (Counted::new(None), Asked::default());
    let few = Most { bodies: Most::USUAL.bodies, reads: 2 };
    let capped = ids.reading::<ByNeed>(ReadsFrom::new(&source, &asked, few)).unwrap_or_else(|e| panic!("{e}"));
    let check = member_body(member(0), 1).check;
    assert_eq!(capped.runnable(item(member(0), 1)).err(), Some(NotRunnable::Unread(Unread::TooManyReads { check, count: 3, most: 2 })));
    let ids = members_ids(8, Most::USUAL).unwrap_or_else(|e| panic!("{e}"));
    let usual = ids.reading::<ByNeed>(ReadsFrom::new(&source, &asked, Most::USUAL)).unwrap_or_else(|e| panic!("{e}"));
    assert!(usual.runnable(item(member(0), 1)).is_ok(), "control: the usual cap folds 3 reads");
}

/// A table of the last form (a body row's read run at bytes 48..56, now zero) is a typed
/// error, never a row read with a run it no longer has. CONTROL: the table itself reads.
#[test]
fn a_body_row_of_the_last_form_is_a_typed_error() {
    let heap = blaze_own::testing::heap();
    let bytes = ids(ToolchainId([7; 32]), &table([1; 32])).write(heap.alloc()).to_vec();
    let crates = u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]) as usize;
    let mut old = bytes.clone();
    old[HEADER + crates * CRATE_ROW + 52] = 1;
    assert!(matches!(RunIds::read(&old, blaze_own::testing::bump()).err(), Some(TableError::BodyPadding { at: 0 })), "a body row's zero bytes");
    let mut header = bytes.clone();
    header[28] = 1;
    assert!(matches!(RunIds::read(&header, blaze_own::testing::bump()).err(), Some(TableError::Padding)), "the header's zero bytes");
    assert!(RunIds::read(&bytes, blaze_own::testing::bump()).is_ok(), "control");
}

// --- A constructor's row is its ADT's head task's (engine_heads.rs `ctor_facts`) ---

/// A tuple struct's constructor key in `app` (a struct's is a member key, front2 keys.rs
/// `member_key_of`; here any key of `app` no body covers).
const CTOR: u64 = 20;

/// An answer whose head task `head` (a `Signature`) files run facts of class `checked`; every
/// other check is clean ([`Cleanly`]'s rule). A check's key is its subject.
struct HeadFacts {
    head: Name,
    checked: Checked,
}

impl Today for HeadFacts {
    fn out(&self, _name: Name) -> Option<ContentId> {
        None
    }
    fn cell(&self, _name: Name, _cell: Cell) -> Option<ContentId> {
        None
    }
}

impl Answer for HeadFacts {
    fn open(&mut self, _cx: &Cx, _name: Name, _seen: &mut dyn FnMut(Seen<'_>) -> Result<(), RunIdsError<'static>>) -> Result<bool, RunIdsError<'static>> {
        Ok(false)
    }
    fn skipped(&self, _file: Name) -> bool {
        false
    }
    fn body(&mut self, _cx: &Cx, check: Name, _tool: Tool, known: Option<(ContentId, Name)>, out: &mut BodyScratch<'_>) -> Result<Option<BodyFacts>, RunIdsError<'static>> {
        out.clear();
        let Some((text, file)) = known else { return Ok(None) };
        let checked = if check == self.head { self.checked } else { Checked::Clean };
        Ok(Some(BodyFacts { key: check.subject, checked, text, file }))
    }
}

/// A CONSTRUCTOR is keyed and gated by its ADT's head task. The fixture's `main.rs` lists the
/// ADT's head task (a `Signature` part) whose run facts cover the constructor's key: the walk
/// gives the constructor a row under that task's name, its text and file; the gate lets it run
/// on the head row's proof (the source is asked that task's name); the table written and read
/// back keeps the check's kind (form 5). TWIN: the head task's facts `Untyped`
/// (`TYPES_NOT_CHECKED`: an ADT's head that did not freeze) refuse the call with that verdict.
/// CONTROL, the cause removed: the same answer with no `Signature` part listed gives the
/// constructor no row, and the call is `NoRow` (what a constructor met before: "the body ids
/// hold nothing for ... none this fn's"); the bodies' rows are as they were.
#[test]
fn a_constructor_is_keyed_and_gated_by_its_adts_head_task() {
    let io = Io::simulated(SimWorld::new(3, Limits::USUAL));
    let cx = io.cx(bstd::test::BUDGET, bstd::mem::TASK_FRAME).expect("a root context");
    let heap = blaze_own::testing::heap();
    let tool = tools()[Kind::BodyCheck.index()];
    let head = Name::new(Kind::Signature, ContentId([0x75; 32]));
    let head_text = ContentId([0x83; 32]);
    let ctor = item(APP, CTOR);
    let mut listed = fixture();
    let main_rs = listed.main_rs;
    for (name, parts, _) in listed.answer.0.iter_mut() {
        if *name == main_rs {
            parts.push(part(head, blaze_drive::Locate::Source(ContentId([0x70; 32]))));
        }
    }
    listed.answer.0.push((head, std::vec::Vec::new(), payload(main_rs, head_text, &[ctor])));
    let walk = workspace_of(&cx, &mut listed.answer, tool, &[], None, heap.alloc()).unwrap_or_else(|e| panic!("{e}"));
    let rows = rows_of(&walk);
    assert!(rows.contains(&WorkspaceBody { item: ctor, check: head, text: head_text, file: main_rs }), "the constructor's row is its ADT's head task's: {rows:?}");

    let mut plain = fixture();
    let without = workspace_of(&cx, &mut plain.answer, tool, &[], None, heap.alloc()).unwrap_or_else(|e| panic!("{e}"));
    let plain_rows = rows_of(&without);
    assert!(plain_rows.iter().all(|row| row.item != ctor), "control: with no `Signature` part the constructor has no row");
    assert_eq!(rows.iter().filter(|row| row.item != ctor).copied().collect::<std::vec::Vec<_>>(), plain_rows, "control: the bodies' rows are as they were");

    let tools = tools();
    let ids_of = |bodies: &[WorkspaceBody], edges: &[Edge], checked: Checked| {
        RunIds::new(&cx, ToolchainId([7; 32]), &table([1; 32]), &tools, bodies, edges, &mut HeadFacts { head, checked }, None, Most::USUAL, blaze_own::testing::bump()).unwrap_or_else(|e| panic!("{e}"))
    };
    let clean = ids_of(&walk.bodies, &walk.edges, Checked::Clean);
    let back = RunIds::read(&clean.write(heap.alloc()), blaze_own::testing::bump()).map(|t| t.workspace_rows(heap.alloc()).to_vec()).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(back.iter().find(|row| row.item == ctor).map(|row| row.check), Some(head), "the table read back names the head task, its kind with it");
    let (source, asked) = (Counted::new(None), Asked::default());
    let clean = clean.reading::<ByNeed>(ReadsFrom::new(&source, &asked, Most::USUAL)).unwrap_or_else(|e| panic!("{e}"));
    assert!(clean.runnable(ctor).is_ok(), "a constructor of a head that froze runs");
    assert!(source.asked.borrow().contains(&head), "on the head row's proof");

    let untyped = Checked::Incomplete { rule: crate::body_types::TYPES_NOT_CHECKED };
    let broken = ids_of(&walk.bodies, &walk.edges, untyped);
    assert_eq!(broken.runnable(ctor), Err(NotRunnable::Checked(untyped)), "twin: an ADT's head not frozen refuses its constructor");

    let bare = ids_of(&without.bodies, &without.edges, Checked::Clean);
    assert!(matches!(bare.runnable(ctor), Err(NotRunnable::NoRow(_))), "control: no `Signature` part, no row (`hold nothing`)");
}