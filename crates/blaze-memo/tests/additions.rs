//! The blaze-memo additions of the front-end port (spec front-end-on-memo, "L1 API, as
//! written"), each against an oracle with a planted bug or removed cause it catches: the typed
//! attach, `#[input(parts)]`, the declared closure and the index contract, the crate's own
//! image, sealed arenas (`Memo::changed`), the snapshot epoch, `owns` values, `keep_owned`
//! values and the downstream-first drop of a snapshot's arenas, kept values that borrow, fact boundaries and untracked reads, the run log and the entry count, a run as a task frame, and a bounded print of an id.

#![feature(allocator_api)]

mod common;

use std::{
    cell::Cell,
    panic::{AssertUnwindSafe, catch_unwind},
    ptr,
    sync::{
        Arc,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
};

use blaze_ids::ContentId;
use blaze_keyed::{
    kind::{Namespace, ToolId},
    proof,
};
use blaze_mem::Tag;
use blaze_memo::{
    Alone, Config, Database, Fact, Gid, Id, ImageOf, MAX_GENERATION, Memo, NextRefused, Plant,
    Read, Reads, Resident, SPILL, attach, completed, on_each_run, read_untracked, record,
    take_counts, testing::RunLog, unrecorded, with_attached_db,
};

use common::{CRATES, Crate, Db, LEAVES, Leaf, VALUE, World, crate_fact, leaf_fact, new_runs, Runs};

struct TestDb {
    memo: Memo,
    runs: Runs,
}

impl Database for TestDb {
    fn memo(&self) -> &Memo {
        &self.memo
    }
}

impl Db for TestDb {
    fn pause(&self) {}

    fn ran(&self, kind: u8, id: Gid, extra: u32) {
        *self.runs.lock().entry((kind, id, extra)).or_insert(0) += 1;
    }
}

/// Another database type: what `with_attached_db` must not hand out as a `TestDb`.
struct OtherDb {
    memo: Memo,
}

impl Database for OtherDb {
    fn memo(&self) -> &Memo {
        &self.memo
    }
}

fn config(plant: Plant) -> Config {
    Config { arenas: CRATES + 1, reads: Reads::Kept, flight: Arc::new(Alone::new()), cap_bytes: u64::MAX, plant }
}

fn values() -> Vec<Vec<u64>> {
    (0..=CRATES).map(|c| (0..LEAVES).map(|l| u64::from(c) * 10 + l as u64).collect()).collect()
}

fn db(memo: Memo) -> TestDb {
    TestDb { memo, runs: new_runs() }
}

fn runs(db: &TestDb) -> u32 {
    db.runs.lock().values().sum()
}

fn panic_text(payload: Box<dyn std::any::Any + Send>) -> String {
    payload
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| payload.downcast_ref::<&str>().map(|it| (*it).to_owned()))
        .unwrap_or_default()
}

// (a) The typed attach.

/// The invariant: `with_attached_db::<D>` is the attached database exactly when it is a `D`,
/// and nothing outside `attach`.
#[test]
fn attached_db_is_typed() {
    let mut world = World::new(values());
    let db = db(world.first(config(Plant::None)));
    let other = OtherDb { memo: world.first(config(Plant::None)) };
    assert!(with_attached_db(|it: Option<&TestDb>| it.is_none()), "nothing attached outside");
    let same = attach(&db, || with_attached_db(|it: Option<&TestDb>| it.is_some_and(|it| ptr::eq(it, &db))));
    assert!(same, "the attached database, as its own type");
    // The control: attached as another type, it is not handed out as a `TestDb`.
    let wrong = attach(&other, || with_attached_db(|it: Option<&TestDb>| it.is_some()));
    assert!(!wrong, "a database attached as `OtherDb` came back as a `TestDb`");
    let nested = attach(&db, || attach(&other, || with_attached_db(|it: Option<&OtherDb>| it.is_some())));
    assert!(nested, "the innermost attach is the one seen");
}

// (b) `#[input(parts)]`.

#[blaze_memo::input(parts)]
pub struct Bag {
    pub n: u64,
}

#[blaze_memo::tracked(returns(copy))]
fn bag_n(db: &dyn Db, bag: Bag) -> u64 {
    *bag.n(db)
}

/// The invariant: a `parts` input's getter lists no fact (its accessors list item facts);
/// a plain input's getter lists its one fact (the control: the cause, `parts`, removed).
#[test]
fn parts_getter_notes_no_fact() {
    let mut builder = Memo::builder(config(Plant::None));
    builder.arena(0, Tag::Query);
    let inputs = builder.arena(1, Tag::Query);
    let bag_fact = Fact { id: ContentId::of("additions bag", b"id"), digest: ContentId::of("additions bag", b"5") };
    let bag = Bag::new(&inputs, bag_fact, 5);
    let leaf = Leaf::new(&inputs, leaf_fact(1, 0, 7), 7);
    let db = db(builder.build());
    let (n, rows) = record(&db.memo, || bag_n(&db, bag)).expect("reads kept");
    let reads = common::facts_of(&rows);
    assert_eq!(n, 5);
    assert!(!reads.contains(&bag_fact), "a `parts` getter listed its input's fact: {reads:?}");
    let (n, rows) = record(&db.memo, || common::value(&db, leaf)).expect("reads kept");
    let reads = common::facts_of(&rows);
    assert_eq!(n, 7);
    assert!(reads.contains(&leaf_fact(1, 0, 7)), "a plain getter must list its fact (the control)");
}

// (c) The declared closure and the index contract.

#[blaze_memo::tracked(returns(copy))]
fn pair(db: &dyn Db, a: Leaf, b: Leaf) -> u64 {
    *a.n(db) + *b.n(db)
}

/// Arena 1 and arena 2 with a leaf each, arena 2 declaring no borrow of 1.
fn unrelated(plant: Plant) -> (TestDb, Leaf, Leaf) {
    let mut builder = Memo::builder(config(plant));
    builder.arena(0, Tag::Query);
    let a = Leaf::new(&builder.arena(1, Tag::Query), leaf_fact(1, 0, 3), 3);
    let b = Leaf::new(&builder.arena(2, Tag::Query), leaf_fact(2, 0, 4), 4);
    (db(builder.build()), a, b)
}

/// The invariant: a key homed at an arena that names an arena outside the home's declared
/// closure panics, naming the key; the planted bug (`MarkOutsideClosure`) lets it through, and
/// the oracle sees no panic.
#[test]
fn key_outside_closure_panics_naming_it() {
    let refused = |plant| {
        let (db, a, b) = unrelated(plant);
        catch_unwind(AssertUnwindSafe(|| pair(&db, a, b))).map_err(panic_text)
    };
    let why = refused(Plant::None).expect_err("a key naming an undeclared arena was accepted");
    assert!(why.contains("pair("), "the panic names the key: {why}");
    assert_eq!(refused(Plant::MarkOutsideClosure), Ok(7), "the plant must let the key through");
}

/// Arena 1 and arena 2 with a leaf each, neither declaring a borrow of the other, `open_1` and
/// `open_2` opening each (`Inputs::open`).
fn opened(open_1: bool, open_2: bool) -> (TestDb, Leaf, Leaf) {
    let mut builder = Memo::builder(config(Plant::None));
    builder.arena(0, Tag::Query);
    let first = builder.arena(1, Tag::Query);
    if open_1 {
        first.open();
    }
    let a = Leaf::new(&first, leaf_fact(1, 0, 3), 3);
    let second = builder.arena(2, Tag::Query);
    if open_2 {
        second.open();
    }
    let b = Leaf::new(&second, leaf_fact(2, 0, 4), 4);
    (db(builder.build()), a, b)
}

/// The invariant: a key homed at an arena may name an open arena outside its declared closure
/// (a source root's files, which a crate reaches through a `#[path]` module only once it
/// runs), and its entry borrows the open arena: a change of the open arena drops the home.
///
/// The control: the same key with arena 1 not open is refused
/// ([`key_outside_closure_panics_naming_it`]), as it is here.
#[test]
fn a_key_may_name_an_open_arena_and_its_home_drops_with_it() {
    let (db, a, b) = opened(true, false);
    assert_eq!(pair(&db, a, b), 7, "a key naming the open arena 1 from arena 2 is accepted");
    let next = db.memo.next(&[a.gid().arena()]).expect("a next snapshot");
    assert!(next.dropped().contains(&2), "the open arena changed, and its namer was carried: {:?}", next.dropped());

    let (db, a, b) = opened(false, false);
    let why = catch_unwind(AssertUnwindSafe(|| pair(&db, a, b))).map_err(panic_text);
    assert!(why.is_err_and(|why| why.contains("outside its home's declared")), "the control: refused");
}

/// The invariant: an entry homed at an open arena (its key names only inputs no crate owns)
/// that reads an arena neither open nor in its declared closure panics, naming the open arena:
/// its key is missing the crate.
///
/// The control: the same read of an open arena is accepted.
#[test]
fn an_open_arenas_entry_that_reads_a_crates_arena_panics() {
    // `pair(a, b)` is homed at arena 2, the greatest it names, and reads arena 1.
    let (db, a, b) = opened(false, true);
    let why = catch_unwind(AssertUnwindSafe(|| pair(&db, a, b))).map_err(panic_text);
    assert!(why.as_ref().is_err_and(|why| why.contains("open arena")), "{why:?}");

    let (db, a, b) = opened(true, true);
    assert_eq!(pair(&db, a, b), 7, "the control: an open arena reads another open one");
}

/// The invariant: a declared borrow's index is below its dependent's (index = 1 + topological
/// position); one above panics.
#[test]
fn borrow_of_a_higher_index_panics() {
    let mut builder = Memo::builder(config(Plant::None));
    builder.arena(0, Tag::Query);
    let two = builder.arena(2, Tag::Query).id();
    let one = builder.arena(1, Tag::Query);
    let refused = catch_unwind(AssertUnwindSafe(|| one.borrows(two))).map_err(panic_text);
    let why = refused.expect_err("a borrow of a higher index was accepted");
    assert!(why.contains("topological"), "the panic states the contract: {why}");
    // The control: the same borrow the right way round is accepted.
    let three = builder.arena(3, Tag::Query);
    three.borrows(two);
}

// (d) The crate's own image.

#[blaze_memo::tracked(image, returns(copy))]
fn doubled(db: &dyn Db, leaf: Leaf) -> u64 {
    db.ran(VALUE, leaf.gid(), 2);
    leaf.n(db).wrapping_mul(2)
}

fn facts_now(world: &World) -> Vec<Fact> {
    let mut out = Vec::new();
    for c in 1..=CRATES {
        out.push(crate_fact(c));
        for (l, n) in world.values[c as usize].iter().enumerate() {
            out.push(leaf_fact(c, l, *n));
        }
    }
    out
}

fn tool() -> ToolId {
    ToolId {
        tool: ContentId::of("blaze-memo additions tool", b"t"),
        target: ContentId::of("blaze-memo additions target", b"t"),
        namespace: Namespace::LOCAL,
    }
}

/// `doubled` of crate 1's first leaf asked under `serve`, the records filed after: the value,
/// the runs and the counts.
fn ask_imaged(world: &World, db: &TestDb, resident: &mut Resident) -> (u64, u32, blaze_memo::Counts) {
    let facts = facts_now(world);
    let now = |id: ContentId| facts.iter().find(|it| it.id == id).map(|it| it.digest);
    let leaf = world.krate(1).leaves(db)[0];
    take_counts();
    let before = runs(db);
    let value = {
        // No boundary is marked here, so no row needs translating.
        let query = |_: &Memo, row: Read| -> Option<Fact> { panic!("an unmarked boundary row {row:?}") };
        let image = ImageOf { tool: tool(), records: &*resident, now, query };
        db.memo.serve(&image, || doubled(db, leaf))
    };
    let counts = take_counts();
    proof::scope(|snap| db.memo.file_records(snap, resident));
    (value, runs(db) - before, counts)
}

fn image_check(plant: Plant) -> Result<(), String> {
    let mut world = World::new(values());
    let mut resident = Resident::new(u64::MAX);
    let first = db(world.first(config(plant)));
    let (got, ran, counts) = ask_imaged(&world, &first, &mut resident);
    if got != 20 || ran != 1 || counts.image_filed != 1 {
        return Err(format!("cold: {got}, {ran} runs, {counts:?}"));
    }
    // Crate 1's arena dropped and made again, nothing moved: served, not run.
    let (memo, dropped) = world.next(&first.memo, &[1]);
    assert!(dropped.contains(&1));
    let second = db(memo);
    let (got, ran, counts) = ask_imaged(&world, &second, &mut resident);
    if got != 20 || ran != 0 || counts.image_served != 1 || counts.image_checks != 1 {
        return Err(format!("after the drop: {got}, {ran} runs, {counts:?}"));
    }
    // The leaf moved: its record's read list no longer holds, and the query runs.
    world.values[1][0] = 99;
    let (memo, _) = world.next(&second.memo, &[1]);
    let third = db(memo);
    let (got, ran, counts) = ask_imaged(&world, &third, &mut resident);
    if got != 198 {
        return Err(format!("after the edit: {got}, not 198 ({ran} runs, {counts:?}): a stale record was served"));
    }
    if ran != 1 || counts.image_redone != 1 {
        return Err(format!("after the edit: {ran} runs, {counts:?}"));
    }
    Ok(())
}

/// The invariant: a record found under its key is served exactly when every digest its read
/// list names holds now (checked once), and then nothing runs; a moved digest runs the query.
/// The planted bug (`ImageSkipsVerify`) serves the stale record, which the oracle catches.
#[test]
fn image_serves_only_what_still_holds() {
    image_check(Plant::None).unwrap_or_else(|why| panic!("{why}"));
    let why = image_check(Plant::ImageSkipsVerify).expect_err("a stale record went unnoticed");
    assert!(why.contains("stale"), "{why}");
}

// (e) Sealed arenas.

/// The invariant: `changed` is false exactly for sealed arenas; a sealed arena with an
/// unsealed dependency is refused (the control: the same with the seal removed is accepted);
/// and a sealed arena named changed is dropped with every arena that borrows it, and nothing
/// else (2026-11-14: it was refused, and base-db started a new session instead).
#[test]
fn sealed_is_not_changed() {
    let made = |seal_one: bool, seal_two: bool| made_under(Plant::None, seal_one, seal_two);
    let memo = made(true, false).expect("a sealed dependency of an unsealed crate");
    let ids: Vec<_> = memo.arenas().collect();
    let one = ids.iter().copied().find(|it| it.index() == 1).expect("arena 1");
    let two = ids.iter().copied().find(|it| it.index() == 2).expect("arena 2");
    assert!(!memo.changed(one) && memo.changed(two));
    let Err(why) = made(false, true) else {
        panic!("a sealed crate with an unsealed dependency was built");
    };
    assert!(why.contains("sealed"), "{why}");
    let control = made(true, false).expect("as above");
    assert!(catch_unwind(AssertUnwindSafe(|| drop(control.next(&[two])))).is_ok(), "the control");
}

/// Arenas 0 (the session), 1 and 2, 2 declaring a borrow of 1, each sealed as asked, built
/// under `plant`; a refusal's text.
fn made_under(plant: Plant, seal_one: bool, seal_two: bool) -> Result<Memo, String> {
    catch_unwind(AssertUnwindSafe(|| {
        let mut builder = Memo::builder(config(plant));
        builder.arena(0, Tag::Query);
        let one = builder.arena(1, Tag::Query);
        let one_id = one.id();
        if seal_one {
            one.seal();
        }
        let two = builder.arena(2, Tag::Query);
        two.borrows(one_id);
        if seal_two {
            two.seal();
        }
        builder.build()
    }))
    .map_err(panic_text)
}

/// The indices `memo.next` drops when the arena at `index` is named changed.
fn dropped_naming(memo: &Memo, index: u32) -> Vec<u32> {
    let named: Vec<_> = memo.arenas().filter(|it| it.index() == index).collect();
    let next = memo.next(&named).unwrap_or_else(|refused| panic!("{refused}"));
    next.dropped().to_vec()
}

/// The invariant (2026-11-14): a sealed arena named changed is dropped, and so is every arena
/// that borrows it (declared or read: a sealed arena's borrows are tracked as any other's),
/// and nothing else: the session and an arena that borrows nothing of it are carried. So a
/// driver drops a sealed crate's closure for a write to it, never the whole session. The
/// planted bug: [`Plant::IgnoreBorrows`] carries the sealed arena's borrower, and the oracle
/// names it.
#[test]
fn a_sealed_arena_named_changed_drops_its_borrowers_alone() {
    let check = |plant: Plant| -> Result<(), String> {
        // Both sealed: a sealed crate over a sealed dependency, as std over core.
        let memo = made_under(plant, true, true)?;
        let dropped = dropped_naming(&memo, 1);
        match dropped.as_slice() {
            [1, 2] => Ok(()),
            other => Err(format!("naming the sealed arena 1 dropped {other:?}, not [1, 2]")),
        }
    };
    check(Plant::None).unwrap_or_else(|why| panic!("{why}"));
    let planted = check(Plant::IgnoreBorrows).expect_err("a carried borrower of a dropped sealed arena went unnoticed");
    assert!(planted.contains("dropped [1]"), "{planted}");
    // The control: the borrower alone named drops it alone, sealed or not.
    let memo = made_under(Plant::None, true, true).expect("both sealed");
    assert_eq!(dropped_naming(&memo, 2), vec![2]);
}

// (f) The snapshot epoch.

/// The invariant: each snapshot's epoch is new and greater than its predecessor's.
#[test]
fn epoch_moves_every_snapshot() {
    let mut world = World::new(values());
    let first = world.first(config(Plant::None));
    let (second, _) = world.next(&first, &[]);
    let (third, _) = world.next(&second, &[2]);
    assert!(first.snapshot_epoch() < second.snapshot_epoch());
    assert!(second.snapshot_epoch() < third.snapshot_epoch());
}

// `owns`.

static DROPS: AtomicUsize = AtomicUsize::new(0);

#[derive(PartialEq, Eq)]
pub struct Counted(Vec<u64>);

impl Drop for Counted {
    fn drop(&mut self) {
        DROPS.fetch_add(1, Ordering::SeqCst);
    }
}

#[blaze_memo::tracked(owns)]
fn owned(db: &dyn Db, leaf: Leaf) -> Counted {
    Counted(vec![*leaf.n(db)])
}

/// The invariant: an `owns` value is dropped once, when its arena is, and not while a
/// snapshot carries the arena (the control: the cause, the drop of the arena, removed).
#[test]
fn owns_value_drops_with_its_arena() {
    let mut world = World::new(values());
    let first = db(world.first(config(Plant::None)));
    let leaf = world.krate(1).leaves(&first)[0];
    let base = DROPS.load(Ordering::SeqCst);
    assert_eq!(owned(&first, leaf).0, vec![10]);
    let (carried, _) = world.next(&first.memo, &[]);
    drop(first);
    assert_eq!(DROPS.load(Ordering::SeqCst), base, "a carried arena dropped its value");
    let (next, dropped) = world.next(&carried, &[1]);
    assert!(dropped.contains(&1));
    drop(carried);
    assert_eq!(DROPS.load(Ordering::SeqCst), base + 1, "the arena's drop runs the value's once");
    drop(next);
    assert_eq!(DROPS.load(Ordering::SeqCst), base + 1, "dropped twice");
}

// `owns` with `cycle_initial`.

/// Every [`Grown`] made and dropped: this test's alone, so no other test's values count.
static GROWN_MADE: AtomicUsize = AtomicUsize::new(0);
static GROWN_DROPPED: AtomicUsize = AtomicUsize::new(0);
/// The address of the value `grow_b` read of `grow_a` in its first round: the initial value.
static FIRST_READ: AtomicUsize = AtomicUsize::new(0);

/// A value that owns memory, counting each one made and each drop.
#[derive(PartialEq, Eq, Debug)]
pub struct Grown(Vec<u64>);

impl Grown {
    fn new(items: Vec<u64>) -> Grown {
        GROWN_MADE.fetch_add(1, Ordering::SeqCst);
        Grown(items)
    }
}

impl Drop for Grown {
    fn drop(&mut self) {
        GROWN_DROPPED.fetch_add(1, Ordering::SeqCst);
    }
}

/// `grow_b`'s items and one more, up to three: with `grow_b` a cycle that settles at
/// `[0, 1, 2]` in a few rounds.
#[blaze_memo::tracked(returns(ref), owns, cycle_initial = |_db, _leaf| Grown::new(Vec::new()))]
fn grow_a(db: &dyn Db, leaf: Leaf) -> Grown {
    let mut items = grow_b(db, leaf).0.clone();
    if items.len() < 3 {
        items.push(items.len() as u64);
    }
    Grown::new(items)
}

/// `grow_a`'s items.
#[blaze_memo::tracked(returns(ref), owns, cycle_initial = |_db, _leaf| Grown::new(Vec::new()))]
fn grow_b(db: &dyn Db, leaf: Leaf) -> Grown {
    let a = grow_a(db, leaf);
    _ = FIRST_READ.compare_exchange(0, ptr::from_ref(a) as usize, Ordering::SeqCst, Ordering::SeqCst);
    Grown::new(a.0.clone())
}

/// The oracle: every value made was dropped exactly once (`made` of them, `dropped` drops).
fn each_dropped_once(made: usize, dropped: usize) -> Result<(), String> {
    match made.cmp(&dropped) {
        std::cmp::Ordering::Equal => Ok(()),
        std::cmp::Ordering::Less => Err(format!("{made} values made and {dropped} drops: one dropped twice")),
        std::cmp::Ordering::Greater => Err(format!("{made} values made and {dropped} drops: one never dropped")),
    }
}

/// The invariant: an `owns` fixpoint keeps every round's value where it was placed until its
/// arena drops: nothing is dropped while the snapshot lives (a reference handed out in round
/// one, the initial value, still reads), each value made is dropped exactly once after, and
/// the values settled are the last round's. The rounds kept are counted.
#[test]
fn owns_fixpoint_drops_each_round_once_with_its_arena() {
    let mut world = World::new(values());
    let first = db(world.first(config(Plant::None)));
    let leaf = world.krate(1).leaves(&first)[0];
    _ = take_counts();
    assert_eq!(grow_a(&first, leaf).0, vec![0, 1, 2], "the head's last round");
    assert_eq!(grow_b(&first, leaf).0, vec![0, 1, 2], "the member's last round, published");
    let kept = take_counts().owned_rounds;
    assert!(
        (3..=2 * u64::from(blaze_memo::MAX_ROUNDS)).contains(&kept),
        "every round's value kept, at most two a round: {kept}"
    );
    assert_eq!(GROWN_DROPPED.load(Ordering::SeqCst), 0, "a round's value dropped while its snapshot lives");
    let first_read = FIRST_READ.load(Ordering::SeqCst);
    assert_ne!(first_read, 0, "the member read the head in round one");
    // SAFETY: a value in an arena of `first`, which is alive: what the invariant promises.
    let initial = unsafe { &*(first_read as *const Grown) };
    assert_eq!(initial.0, Vec::<u64>::new(), "round one's reference reads the initial value");
    // Hits after the fixpoint make nothing.
    let made = GROWN_MADE.load(Ordering::SeqCst);
    assert_eq!(grow_a(&first, leaf).0, vec![0, 1, 2]);
    assert_eq!(GROWN_MADE.load(Ordering::SeqCst), made);
    drop(first);
    each_dropped_once(GROWN_MADE.load(Ordering::SeqCst), GROWN_DROPPED.load(Ordering::SeqCst))
        .unwrap_or_else(|why| panic!("{why}"));
}

/// The control: the oracle refuses a bitwise copy dropped beside its original (the bug
/// `owns` with `cycle_initial` was refused for), and a value never dropped.
#[test]
fn the_drop_oracle_catches_a_copy_and_a_leak() {
    /// Counts its drops and owns nothing else, so a bitwise copy of one is sound to drop.
    struct Tallied<'a>(&'a AtomicUsize);
    impl Drop for Tallied<'_> {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
    // Planted: one value made, copied bitwise and both dropped.
    let dropped = AtomicUsize::new(0);
    let original = Tallied(&dropped);
    // SAFETY: `Tallied` holds only a shared reference; dropping the copy frees nothing.
    let copy = unsafe { ptr::read(&original) };
    drop(copy);
    drop(original);
    let twice = each_dropped_once(1, dropped.load(Ordering::SeqCst));
    assert!(twice.is_err_and(|it| it.contains("twice")), "a planted double drop passed");
    // Planted: one value made and never dropped.
    let dropped = AtomicUsize::new(0);
    std::mem::forget(Tallied(&dropped));
    let never = each_dropped_once(1, dropped.load(Ordering::SeqCst));
    assert!(never.is_err_and(|it| it.contains("never")), "a planted leak passed");
    // Removed: the cause; one value made and dropped once passes.
    let dropped = AtomicUsize::new(0);
    drop(Tallied(&dropped));
    each_dropped_once(1, dropped.load(Ordering::SeqCst)).unwrap_or_else(|why| panic!("{why}"));
}

// A run is a task frame (port decision 7).

/// What the two tagged queries take from their frames: each more than the other's, so a tag's
/// peak names which query ran under it.
const FUZZ_BYTES: usize = 256 << 10;
const LINK_BYTES: usize = 64 << 10;

/// Takes `bytes` from the innermost task frame (which panicked, "frame::current outside any
/// frame::task", before a run was a frame), and gives the frame's tag's index.
fn take_from_frame(bytes: usize) -> usize {
    blaze_mem::frame::current(|frame| {
        let mut scratch: Vec<u8, &blaze_mem::Frame> = Vec::with_capacity_in(bytes, frame);
        scratch.resize(bytes, 1);
        frame.tag().index()
    })
}

#[blaze_memo::tracked(returns(copy))]
fn framed_default(db: &dyn Db, leaf: Leaf) -> usize {
    _ = leaf.n(db);
    take_from_frame(64)
}

#[blaze_memo::tracked(returns(copy), mem_tag = Fuzz)]
fn framed_fuzz(db: &dyn Db, leaf: Leaf) -> usize {
    _ = leaf.n(db);
    take_from_frame(FUZZ_BYTES)
}

#[blaze_memo::tracked(returns(copy), mem_tag = Link)]
fn framed_link(db: &dyn Db, leaf: Leaf) -> usize {
    _ = leaf.n(db);
    take_from_frame(LINK_BYTES)
}

/// The invariant: a query's body runs in a task frame of its tag (`Tag::Query` without
/// `mem_tag`), so it may allocate through `frame::current`, and the bytes it takes count under
/// its own tag. No other test of this binary runs under `Fuzz` or `Link`.
#[test]
fn a_run_is_a_frame_of_its_querys_tag() {
    let mut world = World::new(values());
    let first = db(world.first(config(Plant::None)));
    let leaf = world.krate(1).leaves(&first)[0];
    // The control: outside a run there is no frame, and asking for one stops.
    let outside = catch_unwind(|| blaze_mem::frame::current(|_| ()));
    assert!(
        outside.is_err_and(|it| panic_text(it).contains("outside any frame::task")),
        "a thread in no task had a frame"
    );
    let peak = |tag: Tag| blaze_mem::tag::snapshot().get(tag).task_peak;
    let (fuzz_before, link_before) = (peak(Tag::Fuzz), peak(Tag::Link));
    assert!(fuzz_before < FUZZ_BYTES as u64 && link_before < LINK_BYTES as u64, "another test ran under the tags");
    assert_eq!(framed_default(&first, leaf), Tag::Query.index(), "a query with no `mem_tag`");
    assert_eq!(framed_fuzz(&first, leaf), Tag::Fuzz.index());
    assert_eq!(framed_link(&first, leaf), Tag::Link.index());
    assert!(peak(Tag::Fuzz) >= FUZZ_BYTES as u64, "the `Fuzz` run's bytes are not under `Fuzz`");
    let link = peak(Tag::Link);
    assert!(
        (LINK_BYTES as u64..FUZZ_BYTES as u64).contains(&link),
        "the `Link` run's bytes are not under `Link`, or the `Fuzz` run's are: {link}"
    );
}

/// Runs of `round_a` and `round_b`, and how many of them began in an empty frame.
static ROUND_RUNS: AtomicUsize = AtomicUsize::new(0);
static ROUND_FRESH: AtomicUsize = AtomicUsize::new(0);

/// Notes whether this run's frame starts empty, then takes from it and never gives it back: a
/// round that shared the frame of the round before would find it used.
fn round_frame() {
    blaze_mem::frame::current(|frame| {
        ROUND_RUNS.fetch_add(1, Ordering::SeqCst);
        if frame.used() == 0 {
            ROUND_FRESH.fetch_add(1, Ordering::SeqCst);
        }
        let scratch: Vec<u64, &blaze_mem::Frame> = Vec::with_capacity_in(32, frame);
        std::mem::forget(scratch);
    });
}

/// With `round_b` a cycle that settles at 3 in a few rounds.
#[blaze_memo::tracked(returns(copy), cycle_initial = |_db, _leaf| 0)]
fn round_a(db: &dyn Db, leaf: Leaf) -> u64 {
    round_frame();
    (round_b(db, leaf) + 1).min(3)
}

#[blaze_memo::tracked(returns(copy), cycle_initial = |_db, _leaf| 0)]
fn round_b(db: &dyn Db, leaf: Leaf) -> u64 {
    round_frame();
    round_a(db, leaf)
}

/// The invariant: each round of a fixpoint is its own frame, begun empty, so a round's scratch
/// is freed before the next and never read by it.
#[test]
fn each_fixpoint_round_is_its_own_frame() {
    let mut world = World::new(values());
    let first = db(world.first(config(Plant::None)));
    let leaf = world.krate(1).leaves(&first)[0];
    assert_eq!(round_a(&first, leaf), 3);
    let runs = ROUND_RUNS.load(Ordering::SeqCst);
    assert!(runs >= 4, "a cycle that settles at 3 ran {runs} times");
    assert_eq!(ROUND_FRESH.load(Ordering::SeqCst), runs, "a round began in a frame another had used");
}

// `keep_owned`.

/// Drops of the values `kept_sum` kept (with the one drop list) and of those `kept_unlisted_sum`
/// kept (run only under `Plant::KeepOwnedUnlisted`): each test's own, so no other test counts.
static KEPT_DROPS: AtomicUsize = AtomicUsize::new(0);
static UNLISTED_DROPS: AtomicUsize = AtomicUsize::new(0);

/// A value that owns memory (a `Vec`), counting its drops in the counter it names.
pub struct KeptTree {
    items: Vec<u64>,
    drops: &'static AtomicUsize,
}

impl Drop for KeptTree {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}

/// Keeps one `KeptTree` of `leaf`'s value three times with `keep_owned`, and sums it.
fn kept_sum_in(db: &dyn Db, leaf: Leaf, drops: &'static AtomicUsize) -> u64 {
    let n = *leaf.n(db);
    let tree = blaze_memo::keep_owned(db.memo(), KeptTree { items: vec![n; 3], drops });
    tree.items.iter().sum()
}

#[blaze_memo::tracked(returns(copy))]
fn kept_sum(db: &dyn Db, leaf: Leaf) -> u64 {
    kept_sum_in(db, leaf, &KEPT_DROPS)
}

#[blaze_memo::tracked(returns(copy))]
fn kept_unlisted_sum(db: &dyn Db, leaf: Leaf) -> u64 {
    kept_sum_in(db, leaf, &UNLISTED_DROPS)
}

/// The invariant: a value kept with `keep_owned` is dropped exactly once, when its arena is,
/// and not while a snapshot holds or carries the arena.
#[test]
fn keep_owned_drops_with_its_arena() {
    let mut world = World::new(values());
    let first = db(world.first(config(Plant::None)));
    let leaf = world.krate(1).leaves(&first)[0];
    assert_eq!(kept_sum(&first, leaf), 30);
    assert_eq!(KEPT_DROPS.load(Ordering::SeqCst), 0, "a kept value dropped while its snapshot lives");
    let (carried, _) = world.next(&first.memo, &[]);
    drop(first);
    assert_eq!(KEPT_DROPS.load(Ordering::SeqCst), 0, "a carried arena dropped its kept value");
    let (next, dropped) = world.next(&carried, &[1]);
    assert!(dropped.contains(&1));
    drop(carried);
    each_dropped_once(1, KEPT_DROPS.load(Ordering::SeqCst)).unwrap_or_else(|why| panic!("{why}"));
    drop(next);
    each_dropped_once(1, KEPT_DROPS.load(Ordering::SeqCst)).unwrap_or_else(|why| panic!("{why}"));
}

/// The control: the same value through a form that places it but does not list it for drop
/// (`Plant::KeepOwnedUnlisted`: what `keep` would do with it, were its no-drop rule not
/// there) leaks. The test tells by the drop count: after every snapshot that held the arena is
/// gone, the one value made has 0 drops, which the oracle names "never dropped".
#[test]
fn keep_owned_unlisted_leaks() {
    let mut world = World::new(values());
    let first = db(world.first(config(Plant::KeepOwnedUnlisted)));
    let leaf = world.krate(1).leaves(&first)[0];
    assert_eq!(kept_unlisted_sum(&first, leaf), 30);
    let (next, dropped) = world.next(&first.memo, &[1]);
    assert!(dropped.contains(&1));
    drop(first);
    drop(next);
    let leaked = each_dropped_once(1, UNLISTED_DROPS.load(Ordering::SeqCst));
    assert!(leaked.is_err_and(|it| it.contains("never")), "an unlisted kept value was dropped");
}

/// The order `marker_of` values were dropped in, as decimal digits (crate 3, then 2, then 1
/// reads 321).
static DROP_ORDER: AtomicU64 = AtomicU64::new(0);

/// A kept value that writes its crate's number into `DROP_ORDER` when dropped.
pub struct Marker(u64);

impl Drop for Marker {
    fn drop(&mut self) {
        _ = DROP_ORDER.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |it| Some(it * 10 + self.0));
    }
}

/// Keeps a `Marker` of `leaf`'s crate (its value's tens digit) in the leaf's arena.
#[blaze_memo::tracked(returns(copy))]
fn marker_of(db: &dyn Db, leaf: Leaf) -> u64 {
    blaze_memo::keep_owned(db.memo(), Marker(*leaf.n(db) / 10)).0
}

/// The invariant: a snapshot releases its arenas downstream first (`memo::Slots`), so a drop
/// list runs while every arena of a lower index (what its values may borrow from: the
/// session, the dependencies) is alive. A drop in index order reads 123.
#[test]
fn arenas_drop_downstream_first() {
    let mut world = World::new(values());
    let first = db(world.first(config(Plant::None)));
    for c in 1..=3 {
        let leaf = world.krate(c).leaves(&first)[0];
        assert_eq!(marker_of(&first, leaf), u64::from(c));
    }
    drop(first);
    assert_eq!(DROP_ORDER.load(Ordering::SeqCst), 321, "the arenas' drop lists ran out of order");
}

// Kept values that borrow.

#[blaze_memo::tracked(returns(copy))]
fn pointed<'db>(db: &'db dyn Db, krate: Crate) -> &'db u64 {
    let dep = krate.deps(db)[0];
    let leaf = dep.leaves(db)[0];
    *blaze_memo::keep(db.memo(), leaf.n(db))
}

fn pointed_check(plant: Plant) -> Result<(), String> {
    let mut world = World::new(values());
    let first = db(world.first(config(plant)));
    let got = *pointed(&first, world.krate(2));
    if got != 10 {
        return Err(format!("{got}"));
    }
    let (_, dropped) = world.next(&first.memo, &[1]);
    if !dropped.contains(&2) {
        return Err(format!("crate 2 holds a reference into crate 1 but was carried: dropped {dropped:?}"));
    }
    Ok(())
}

/// The invariant: a kept value holding a reference into a borrowed arena is dropped with that
/// arena (its home is). The planted bug (`IgnoreBorrows`) carries the home, which the oracle
/// catches.
#[test]
fn kept_reference_drops_with_the_borrowed_arena() {
    pointed_check(Plant::None).unwrap_or_else(|why| panic!("{why}"));
    assert!(pointed_check(Plant::IgnoreBorrows).is_err(), "the plant must carry crate 2");
}

// Fact boundaries (port decision 2).

#[blaze_memo::tracked(returns(copy))]
fn item_hash(db: &dyn Db, leaf: Leaf) -> u64 {
    leaf.n(db).wrapping_mul(31)
}

#[blaze_memo::tracked(returns(copy))]
fn hashes(db: &dyn Db, krate: Crate) -> u64 {
    let leaves = krate.leaves(db);
    item_hash(db, leaves[0]).wrapping_add(item_hash(db, leaves[1]))
}

fn boundary_check(plant: Plant) -> Result<(), String> {
    let mut world = World::new(values());
    let mut builder = Memo::builder(config(plant));
    builder.mark_boundary(item_hash::KIND_NAME);
    builder.arena(0, Tag::Query);
    for c in 1..=CRATES {
        world.make(&mut builder, c);
    }
    let db = db(builder.build());
    let krate = world.krate(1);
    let leaves = krate.leaves(&db).clone();
    // One boundary found (a hit), the other run under the asker: both must list alike.
    item_hash(&db, leaves[0]);
    let (_, rows) = record(&db.memo, || hashes(&db, krate)).map_err(|it| it.to_string())?;
    let named: Vec<Gid> =
        rows.iter().filter_map(|row| item_hash::key_of(&db.memo, *row)).map(|(leaf,)| leaf.gid()).collect();
    let facts: Vec<Fact> = rows
        .iter()
        .filter_map(|row| match row {
            Read::Input { fact, .. } => Some(*fact),
            Read::Query { .. } => None,
        })
        .collect();
    if facts.contains(&leaf_fact(1, 0, 10)) || facts.contains(&leaf_fact(1, 1, 11)) {
        return Err(format!("a boundary's own reads leaked into its asker: {rows:?}"));
    }
    if named.len() != 2 || !named.contains(&leaves[0].gid()) || !named.contains(&leaves[1].gid()) {
        return Err(format!("the asker does not name both boundaries, one row each: {rows:?}"));
    }
    if facts != [crate_fact(1)] {
        return Err(format!("the asker's own input read is not listed alone: {rows:?}"));
    }
    Ok(())
}

/// The invariant: a boundary query asked under a recorder, hit or run, adds exactly one
/// `Read::Query` row naming its entry to the asker's list and none of its own rows (the
/// firewall); the asker's own input reads are listed as before. The planted bug
/// (`BoundaryLeaks`) flattens the boundary, which the oracle catches.
#[test]
fn boundary_lists_one_row_and_keeps_its_own() {
    boundary_check(Plant::None).unwrap_or_else(|why| panic!("{why}"));
    let why = boundary_check(Plant::BoundaryLeaks).expect_err("a leaking boundary went unnoticed");
    assert!(why.contains("leaked"), "{why}");
}

#[blaze_memo::tracked(image, returns(copy))]
fn from_env(db: &dyn Db, leaf: Leaf) -> u64 {
    read_untracked(db.memo(), "the test's environment");
    *leaf.n(db)
}

/// The invariant: an untracked read is listed as such, and an imaged query whose list holds
/// one is never filed (the control: `doubled`, the same without the read, is filed).
#[test]
fn untracked_read_is_never_filed() {
    let mut world = World::new(values());
    let mut resident = Resident::new(u64::MAX);
    let first = db(world.first(config(Plant::None)));
    let leaf = world.krate(1).leaves(&first)[0];
    let (_, rows) = record(&first.memo, || from_env(&first, leaf)).expect("reads kept");
    assert!(rows.iter().any(|row| row.is_untracked()), "the untracked read is listed: {rows:?}");
    let second = db(world.next(&first.memo, &[1]).0);
    let leaf = world.krate(1).leaves(&second)[0];
    let facts = facts_now(&world);
    let now = |id: ContentId| facts.iter().find(|it| it.id == id).map(|it| it.digest);
    let query = |_: &Memo, row: Read| -> Option<Fact> { panic!("an unmarked boundary row {row:?}") };
    let image = ImageOf { tool: tool(), records: &resident, now, query };
    take_counts();
    second.memo.serve(&image, || from_env(&second, leaf));
    let counts = take_counts();
    assert_eq!((counts.image_filed, counts.image_untracked), (0, 1), "{counts:?}");
    second.memo.serve(&image, || doubled(&second, leaf));
    assert_eq!(take_counts().image_filed, 1, "the control is filed");
    drop(image);
    proof::scope(|snap| second.memo.file_records(snap, &mut resident));
}

// The run log and the entry count.

/// The invariant: the log lists each run once, in order, and a hit adds none; `entries`
/// counts an arena's entries of a query, and none once the arena is dropped and made again.
#[test]
fn run_log_and_entries() {
    let mut world = World::new(values());
    let log = Arc::new(RunLog::new());
    let mut builder = Memo::builder(config(Plant::None));
    builder.log_runs(Arc::clone(&log));
    builder.arena(0, Tag::Query);
    for c in 1..=CRATES {
        world.make(&mut builder, c);
    }
    let first = db(builder.build());
    let leaves = world.krate(1).leaves(&first);
    let name = "additions::common::value";
    common::value(&first, leaves[0]);
    common::value(&first, leaves[1]);
    common::value(&first, leaves[0]);
    let ran = log.take();
    assert_eq!(ran.len(), 2, "{ran:?}");
    assert!(ran.iter().all(|it| it.query == name), "{ran:?}");
    let one = world.krate(1).gid().arena();
    assert_eq!(first.memo.entries(one, name), 2);
    // The control: crate 4 has no dependents, so a change of it carries crate 1 and its entries.
    let (carried, dropped) = world.next(&first.memo, &[4]);
    assert!(!dropped.contains(&1), "{dropped:?}");
    assert_eq!(carried.entries(one, name), 2, "a carried arena keeps its entries");
    let (remade, _) = world.next(&carried, &[1]);
    let one = world.krate(1).gid().arena();
    assert_eq!(remade.entries(one, name), 0, "a remade arena holds no entry");
    assert_eq!(log.count(name), 0, "nothing ran since the log was taken");
}

/// The invariant: a logged run names its query and key typed (`Ran::is`) and its home arena
/// (the controls: another key and another query are not it; a value of another type than
/// the query's argument tuple is refused by name, not answered `false`).
#[test]
fn a_run_names_its_query_key_and_arena() {
    let mut world = World::new(values());
    let log = Arc::new(RunLog::new());
    let mut builder = Memo::builder(config(Plant::None));
    builder.log_runs(Arc::clone(&log));
    builder.arena(0, Tag::Query);
    for c in 1..=CRATES {
        world.make(&mut builder, c);
    }
    let first = db(builder.build());
    let leaves = world.krate(2).leaves(&first).clone();
    common::value(&first, leaves[0]);
    let ran = log.take();
    assert_eq!(ran.len(), 1, "{ran:?}");
    assert!(ran[0].is::<common::value>(&(leaves[0],)), "{ran:?}");
    assert_eq!(ran[0].arena(), world.krate(2).gid().arena());
    assert!(!ran[0].is::<common::value>(&(leaves[1],)), "another key");
    assert!(!ran[0].is::<common::total>(&(world.krate(2), 0_u32)), "another query");
    let wrong = catch_unwind(AssertUnwindSafe(|| ran[0].is::<common::value>(&leaves[0]))).map_err(panic_text);
    let why = wrong.expect_err("a bare leaf for the argument tuple was answered");
    assert!(why.contains("additions::common::value"), "{why}");
}

// The API the ported crates call (2026-11-13).

/// The invariant: `Gid::in_arena` is the inverse of `arena` and `local`, the spill bit kept,
/// and the id it gives reads what the original reads; `Gid::for_test` makes an id of arena 1
/// without a memo (the controls: another local is another id).
#[test]
fn in_arena_inverts_arena_and_local() {
    let mut world = World::new(values());
    let first = db(world.first(config(Plant::None)));
    let gid = world.krate(2).leaves(&first)[1].gid();
    let again = Gid::in_arena(gid.arena(), gid.local());
    assert_eq!(again, gid);
    assert_eq!(*Leaf::from_gid(again).n(&first), 21);
    let spilled = Gid::in_arena(gid.arena(), 7 | SPILL);
    assert_eq!((spilled.arena(), spilled.local()), (gid.arena(), 7 | SPILL));
    assert_ne!(Gid::in_arena(gid.arena(), gid.local() + 1), gid);
    let test = Gid::for_test(5);
    assert_eq!((test.arena().index(), test.arena().generation(), test.local()), (1, 0, 5));
    assert_ne!(Gid::for_test(5), Gid::for_test(6));
}

/// The invariant: nothing read under `unrecorded`, run or hit, reaches the list of a recorder
/// around it, and a recorder inside it still gives its own rows (the control: the same read
/// without `unrecorded` is listed).
#[test]
fn unrecorded_reads_reach_no_list() {
    let mut world = World::new(values());
    let first = db(world.first(config(Plant::None)));
    let leaves = world.krate(1).leaves(&first).clone();
    let memo = &first.memo;
    for asked in ["run", "hit"] {
        let (n, rows) = record(memo, || unrecorded(memo, || common::value(&first, leaves[0]))).expect("reads kept");
        assert_eq!(n, 10);
        assert!(rows.is_empty(), "an unrecorded {asked} reached the list: {rows:?}");
    }
    let (inner, outer) = record(memo, || {
        unrecorded(memo, || record(memo, || common::value(&first, leaves[1])).expect("reads kept").1)
    })
    .expect("reads kept");
    assert!(common::facts_of(&inner).contains(&leaf_fact(1, 1, 11)), "{inner:?}");
    assert!(outer.is_empty(), "an inner recorder's rows went past `unrecorded`: {outer:?}");
    let (_, rows) = record(memo, || common::value(&first, leaves[0])).expect("reads kept");
    assert!(common::facts_of(&rows).contains(&leaf_fact(1, 0, 10)), "the control: {rows:?}");
}

thread_local! {
    static HOOKED: Cell<u32> = const { Cell::new(0) };
}

fn hooked() {
    HOOKED.with(|it| it.set(it.get() + 1));
}

/// The invariant: each run that returns grows `completed` and is told to the one hook, on its
/// thread; a second hook is refused (the control: a hit is no run, and is told to no one).
#[test]
fn each_run_is_completed_and_told() {
    assert!(on_each_run(hooked).is_ok(), "the one hook of this test binary");
    assert!(on_each_run(hooked).is_err(), "a second hook was accepted");
    let mut world = World::new(values());
    let first = db(world.first(config(Plant::None)));
    let leaf = world.krate(3).leaves(&first)[2];
    let (completed_before, told_before) = (completed(), HOOKED.with(Cell::get));
    assert_eq!(common::value(&first, leaf), 32);
    let told = HOOKED.with(Cell::get);
    assert!(completed() > completed_before, "a run returned and `completed` did not grow");
    assert_eq!(told, told_before + 1, "the hook is told of the run, once");
    assert_eq!(common::value(&first, leaf), 32);
    assert_eq!(HOOKED.with(Cell::get), told, "a hit was told as a run");
}

/// The invariant: an arena index is made at most `MAX_GENERATION + 1` times in an id space;
/// a drop that would make it once more is refused by `Memo::next`, naming the arena, never a
/// wrapped generation (the control: every remake up to the last generation succeeds).
#[test]
fn a_spent_generation_is_refused_naming_the_arena() {
    let mut builder = Memo::builder(config(Plant::None));
    builder.arena(0, Tag::Query);
    builder.arena(1, Tag::Query);
    let mut memo = builder.build();
    let one = |memo: &Memo| memo.arenas().find(|it| it.index() == 1).expect("arena 1");
    while one(&memo).generation() < MAX_GENERATION {
        let was = one(&memo).generation();
        let next = memo
            .next(&[one(&memo)])
            .unwrap_or_else(|why| panic!("the remake after generation {was} was refused: {why}"));
        assert_eq!(next.dropped(), &[1]);
        let mut builder = next.builder;
        builder.arena(1, Tag::Query);
        memo = builder.build();
        assert_eq!(one(&memo).generation(), was + 1, "a remake takes the next generation");
    }
    let last = one(&memo);
    let refused = memo.next(&[last]).map(drop).expect_err("a remake past the last generation wrapped");
    assert_eq!(refused, NextRefused::GenerationsSpent { arena: last });
    let why = refused.to_string();
    assert!(why.contains(&format!("{last:?}")), "the refusal names the arena: {why}");
}

// One id space per database: a new session never hands out an earlier id (2026-11-14).

/// Arenas `0..=CRATES` of a session's first snapshot, index 1 sealed (a crate not being built),
/// each id it took noted in `seen`.
fn whole_session(mut builder: blaze_memo::Builder, seen: &mut Seen) -> Result<Memo, String> {
    for index in 0..=CRATES {
        let inputs = builder.arena(index, Tag::Query);
        if index == 1 {
            inputs.seal();
        }
        seen.note(inputs.id())?;
    }
    Ok(builder.build())
}

/// Every arena id handed out so far, and the oracle: an id handed out twice.
#[derive(Default)]
struct Seen(std::collections::HashSet<blaze_memo::ArenaId>);

impl Seen {
    fn note(&mut self, id: blaze_memo::ArenaId) -> Result<(), String> {
        // An id equal to an earlier making's: its first `Gid` equals that making's first too.
        match self.0.insert(id) {
            true => Ok(()),
            false => Err(format!("{id:?} was handed out twice (local 0 of each: {:?})", Gid::in_arena(id, 0))),
        }
    }
}

/// The driver's life in miniature: three sessions after the first, each started as base-db
/// starts one when it cannot carry (`Memo::next_session`: a failed commit, the arenas
/// outgrown), and between them
/// commits that each drop an unsealed crate's arena, and every fifth the session arena. How
/// many ids were handed out, or the first handed out twice.
fn ids_across_sessions(plant: Plant) -> Result<usize, String> {
    let mut seen = Seen::default();
    let mut memo = whole_session(Memo::builder(config(Plant::None)), &mut seen)?;
    for session in 0..4 {
        for commit in 0..24_u32 {
            let index = match commit % 5 {
                4 => blaze_memo::ArenaId::SESSION_INDEX,
                n => 2 + n % (CRATES - 1),
            };
            let changed = memo.arenas().find(|it| it.index() == index).expect("every index is held");
            let next = memo.next(&[changed]).map_err(|why| why.to_string())?;
            assert_eq!(next.dropped(), &[index], "no borrows: only the named arena is dropped");
            let mut builder = next.builder;
            seen.note(builder.arena(index, Tag::Query).id())?;
            memo = builder.build();
        }
        if session < 3 {
            let builder = memo.next_session(config(plant)).map_err(|why| why.to_string())?;
            memo = whole_session(builder, &mut seen)?;
        }
    }
    Ok(seen.0.len())
}

/// The invariant: within one database's id space an `ArenaId` handed out names one making for
/// ever: over 96 commits and three new sessions (each what base-db starts when it cannot carry), no
/// arena, carried, remade by a commit or made by a new session, takes an id an earlier arena
/// had, so no `Gid` of a later making equals one of an earlier making, and a cache outside the
/// memo keyed by them is never answered by another arena's memory. The control (the cause put
/// back): with `Plant::SessionRestartsIds` the first new session makes the session arena at
/// generation 0 again, and the oracle names the id handed out twice.
#[test]
fn a_new_session_after_a_sealed_arenas_write_never_hands_out_an_earlier_id() {
    let handed = ids_across_sessions(Plant::None).unwrap_or_else(|why| panic!("{why}"));
    // Five arenas in each of four sessions, and one remade per commit.
    assert_eq!(handed, 4 * (CRATES as usize + 1) + 4 * 24);
    let planted = ids_across_sessions(Plant::SessionRestartsIds).expect_err("a restarted id space was not caught");
    assert!(planted.contains("arena 0.0 was handed out twice"), "{planted}");
}

/// The invariant: an index's last generation in an id space is its last in every session of
/// the database: once an arena is made at `MAX_GENERATION`, a new session of the database
/// (`Memo::next_session`) is refused naming that making, as `Memo::next` refuses its drop,
/// and never makes the index again at generation 0 in the same space; only a new id space
/// (`Memo::builder`) makes it again. The control: one generation before the last, the new
/// session is made and its arena takes the last generation.
#[test]
fn a_new_session_of_a_spent_index_is_refused_naming_its_last_making() {
    let mut builder = Memo::builder(config(Plant::None));
    builder.arena(0, Tag::Query);
    builder.arena(1, Tag::Query);
    let mut memo = builder.build();
    let one = |memo: &Memo| memo.arenas().find(|it| it.index() == 1).expect("arena 1");
    while one(&memo).generation() < MAX_GENERATION - 1 {
        let next = memo.next(&[one(&memo)]).unwrap_or_else(|why| panic!("{why}"));
        let mut builder = next.builder;
        builder.arena(1, Tag::Query);
        memo = builder.build();
    }
    // The control: the index has one generation left, which the new session takes.
    let mut builder = memo.next_session(config(Plant::None)).unwrap_or_else(|why| panic!("{why}"));
    assert_eq!(builder.arena(0, Tag::Query).id().generation(), 1, "the session index goes on");
    let last = builder.arena(1, Tag::Query).id();
    assert_eq!(last.generation(), MAX_GENERATION, "a new session goes on from the last making");
    let memo = builder.build();

    let refused = memo.next_session(config(Plant::None)).map(drop).expect_err("a new session restarted a spent index");
    assert_eq!(refused, NextRefused::GenerationsSpent { arena: last });
    let why = refused.to_string();
    assert!(why.contains(&format!("{last:?}")) && why.contains("id space"), "the refusal names the arena: {why}");
    let dropped = memo.next(&[last]).map(drop).expect_err("a drop of the spent index was accepted");
    assert_eq!(dropped, NextRefused::GenerationsSpent { arena: last });
    // Only a new id space makes the index again from generation 0.
    let mut fresh = Memo::builder(config(Plant::None));
    assert_eq!(fresh.arena(1, Tag::Query).id().generation(), 0);
}

// `try_from_gid`: an id from outside checked, not trusted (2026-11-14).

/// An interned struct homed in a crate's arena (its leaf's).
#[blaze_memo::interned(debug)]
pub struct Tagged<'db> {
    pub leaf: Leaf,
    pub n: u64,
}

/// Another interned kind with the same fields: an id of it is not a `Tagged`.
#[blaze_memo::interned(debug)]
pub struct Marked<'db> {
    pub leaf: Leaf,
    pub n: u64,
}

/// The invariant: `try_from_gid` is `Some` exactly for an id naming a live entry of its own
/// kind in the snapshot, and `None` (never a panic) for an id of another interned kind, an
/// input's id, a local past the arena's entries, a spill index naming nothing, and an id of an
/// arena dropped and made again (the old generation's). The control: the unchecked path
/// (`from_gid` and a getter) panics on the dropped arena's id, so the check is what keeps the
/// reader whole, and the same fields interned again in the new snapshot are found.
#[test]
fn try_from_gid_checks_every_word() {
    let mut world = World::new(values());
    let first = db(world.first(config(Plant::None)));
    let leaf = world.krate(1).leaves(&first)[0];
    let tagged = Tagged::new(&first, leaf, 7);
    let marked = Marked::new(&first, leaf, 7);
    assert_eq!(Tagged::try_from_gid(&first, tagged.gid()), Some(tagged), "a live id of its kind");
    assert_eq!(Marked::try_from_gid(&first, marked.gid()), Some(marked), "a live id of its kind");
    assert_eq!(Tagged::try_from_gid(&first, marked.gid()), None, "an id of another interned kind");
    assert_eq!(Marked::try_from_gid(&first, tagged.gid()), None, "an id of another interned kind");
    assert_eq!(Tagged::try_from_gid(&first, leaf.gid()), None, "an input's id");
    let past = Gid::in_arena(tagged.gid().arena(), tagged.gid().local() + 1000);
    assert_eq!(Tagged::try_from_gid(&first, past), None, "a local past the arena's entries");
    let far = Gid::in_arena(tagged.gid().arena(), (1 << 30) + 5);
    assert_eq!(Tagged::try_from_gid(&first, far), None, "a local in no segment of the column");
    let spill = Gid::in_arena(tagged.gid().arena(), 3 | SPILL);
    assert_eq!(Tagged::try_from_gid(&first, spill), None, "a spill index naming nothing");

    // Arena 1 dropped and made again: the old generation's id names nothing.
    let (next, dropped) = world.next(&first.memo, &[1]);
    assert!(dropped.contains(&1), "crate 1's arena is made again");
    let second = db(next);
    assert_eq!(Tagged::try_from_gid(&second, tagged.gid()), None, "an id of a dropped arena");
    assert_eq!(Marked::try_from_gid(&second, marked.gid()), None, "an id of a dropped arena");
    let unchecked = catch_unwind(AssertUnwindSafe(|| *Tagged::from_gid(tagged.gid()).n(&second)));
    assert!(unchecked.is_err(), "the control: the unchecked read of a dead id panics");
    let again = Tagged::new(&second, world.krate(1).leaves(&second)[0], 7);
    assert_ne!(again.gid(), tagged.gid(), "made again at the next generation");
    assert_eq!(Tagged::try_from_gid(&second, again.gid()), Some(again), "the new id is live");
}

// A print of an id is bounded (2026-11-14): one level deep, each field cut.

/// A node of a graph of interned keys: its children are in a `Kids`, which holds nodes again.
#[blaze_memo::interned(debug)]
pub struct Node<'db> {
    pub depth: u32,
    pub kids: Option<Kids<'db>>,
}

/// Four nodes: the list a `Node` holds.
#[blaze_memo::interned(debug)]
pub struct Kids<'db> {
    pub items: [Node<'db>; 4],
}

/// An input with a text field.
#[blaze_memo::input(debug)]
pub struct Doc {
    pub text: String,
}

/// A chain of `depth` levels, each node's four children the one node below: a graph of
/// `depth` nodes whose full print has 4^depth leaves.
fn chain<'db>(db: &'db TestDb, depth: u32) -> Node<'db> {
    let mut node = Node::new(db, 0, None);
    for level in 1..=depth {
        let kids = Kids::new(db, [node; 4]);
        node = Node::new(db, level, Some(kids));
    }
    node
}

/// The print the generated `Debug` made before the bound: every field, every id in it
/// printed with its own fields, to the leaves.
fn full_print<'db>(db: &'db TestDb, node: Node<'db>) -> String {
    let kids = match *node.kids(db) {
        None => "None".to_owned(),
        Some(kids) => {
            let items: Vec<String> = kids.items(db).iter().map(|it| full_print(db, *it)).collect();
            format!("Some(Kids {{ items: [{}] }})", items.join(", "))
        }
    };
    format!("Node {{ depth: {}, kids: {kids} }}", node.depth(db))
}

/// The invariant: an interned id's `Debug` names its kind and id and prints its fields one
/// level deep (an id inside a field, through any container, as its kind and id alone), each
/// field cut at `FIELD_CAP` bytes, and an input's `Debug` names its kind and id and none of its
/// fields (they are its value, not what it is): so a key 12 levels deep with fan-out 4 (a full
/// print of 4^12 leaves) prints in a few hundred bytes and an input with a megabyte of text in
/// a few tens. The control: the recursive print, the old `Debug`, grows by the fan-out each
/// level, so at depth 12 it would be millions of bytes.
#[test]
fn a_print_of_an_id_is_bounded() {
    let mut world = World::new(values());
    let db = db(world.first(config(Plant::None)));

    let deep = chain(&db, 12);
    let printed = attach(&db, || format!("{deep:?}"));
    assert!(printed.len() < 400, "a key 12 levels deep printed {} bytes: {printed}", printed.len());
    assert!(printed.starts_with("Node {"), "names its kind: {printed}");
    assert!(printed.contains(&format!("{:?}", deep.gid())), "names its id: {printed}");
    assert!(printed.contains("depth: 12"), "prints its own fields: {printed}");
    let kids = (*deep.kids(&db)).expect("a node above the leaves has kids");
    assert!(
        printed.contains(&format!("Kids({:?})", kids.gid())),
        "an id inside a field prints as its kind and id alone: {printed}"
    );
    // The bound does not grow with depth: depth 3 prints as long, give or take a digit.
    let shallow = attach(&db, || format!("{:?}", chain(&db, 3)));
    assert!(printed.len() <= shallow.len() + 4, "depth 12: {printed}; depth 3: {shallow}");
    // The bound holds through a container: a list of ids prints each as its kind and id.
    let list = attach(&db, || format!("{kids:?}"));
    assert!(list.len() < 400 && list.matches("Node(").count() == 4, "a list of ids: {list}");

    // The control: the recursive print grows by the fan-out per level.
    let lens: Vec<usize> = (1..=4).map(|depth| full_print(&db, chain(&db, depth)).len()).collect();
    for pair in lens.windows(2) {
        assert!(pair[1] >= 4 * pair[0], "the recursive print grows by 4 a level: {lens:?}");
    }
    assert!(lens[2] > printed.len(), "at depth 3 the recursive print is already the longer: {lens:?}");

    // An input with a megabyte of text.
    let mut builder = Memo::builder(config(Plant::None));
    builder.arena(0, Tag::Query);
    let inputs = builder.arena(1, Tag::Query);
    let fact = Fact { id: ContentId::of("additions doc", b"id"), digest: ContentId::of("additions doc", b"1") };
    let doc = Doc::new(&inputs, fact, "a".repeat(1 << 20));
    let docs = TestDb { memo: builder.build(), runs: new_runs() };
    let printed = attach(&docs, || format!("{doc:?}"));
    assert!(printed.len() < 100, "a megabyte of text printed {} bytes", printed.len());
    assert!(printed.starts_with("Doc {") && printed.contains(&format!("{:?}", doc.gid())), "{printed}");
    assert!(!printed.contains("aaaa"), "an input's print holds none of its value: {printed}");
}

// The session arena named by no constant (2026-11-14).

/// A key of session-level data that says it names no arena with `None`: the form hir-ty's
/// homes of plain data take (`Option<ArenaId>`), which carries no generation.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, blaze_memo::Home)]
pub struct Plain {
    pub n: u32,
    pub names: Option<blaze_memo::ArenaId>,
}

#[blaze_memo::tracked(returns(copy))]
fn session_square(_db: &dyn Db, n: u32) -> u64 {
    u64::from(n) * u64::from(n)
}

#[blaze_memo::tracked(returns(copy))]
fn session_plain(_db: &dyn Db, key: Plain) -> u64 {
    u64::from(key.n) + 1
}

/// The next snapshot of `memo` with its session arena named changed, so dropped and made again
/// at the next generation: what a commit of a session-level input does.
fn remake_session(memo: &Memo) -> Memo {
    let next = memo.next(&[memo.session()]).unwrap_or_else(|why| panic!("{why}"));
    assert_eq!(next.dropped(), &[blaze_memo::ArenaId::SESSION_INDEX], "only the session is dropped");
    let mut builder = next.builder;
    builder.arena(blaze_memo::ArenaId::SESSION_INDEX, Tag::Query);
    builder.build()
}

/// The invariant: a query keyed by plain data (a `u32`, a key whose `Home` names nothing) is
/// homed at the asking snapshot's own session arena, at whatever generation it holds, so it
/// answers the same before and after two commits that drop and remake the session. What
/// stopped before: hir-ty homed such keys at `ArenaId::SESSION`, a constant of generation 0,
/// and after the first commit every such ask panicked `arena 0.0 is dead`; no code can write
/// that now (`ArenaId` has no public constant or constructor). The control (the cause put
/// back): a key naming the first snapshot's session arena, an id of generation 0 held across
/// the commits, is refused as dead by name.
#[test]
fn a_plain_key_answers_at_the_live_session_across_commits() {
    let log = Arc::new(RunLog::new());
    let mut builder = Memo::builder(config(Plant::None));
    builder.log_runs(Arc::clone(&log));
    builder.arena(blaze_memo::ArenaId::SESSION_INDEX, Tag::Query);
    let first = db(builder.build());
    let plain = Plain { n: 3, names: None };
    let ask = |db: &TestDb| (session_square(db, 7), session_plain(db, plain));
    assert_eq!(ask(&first), (49, 4));
    let generation_0 = first.memo.session();
    assert_eq!(generation_0.generation(), 0);
    assert_eq!(first.memo.entries(generation_0, "additions::session_square"), 1);
    assert_eq!(log.take().len(), 2, "each asked once, each ran once");

    let second = db(remake_session(&first.memo));
    let third = db(remake_session(&second.memo));
    let live = third.memo.session();
    assert_eq!((live.index(), live.generation()), (blaze_memo::ArenaId::SESSION_INDEX, 2));
    assert_eq!(ask(&third), (49, 4), "the same answers at the session's third making");
    assert_eq!(third.memo.entries(live, "additions::session_square"), 1, "homed at the live session");
    assert_eq!(third.memo.entries(live, "additions::session_plain"), 1, "homed at the live session");
    let ran = log.take();
    assert_eq!(ran.len(), 2, "the remade session holds no entry, so each ran again: {ran:?}");
    assert!(ran.iter().all(|it| it.arena() == live), "each ran homed at the live session: {ran:?}");
    assert_eq!(ask(&third), (49, 4));
    assert!(log.take().is_empty(), "a second ask is a hit");

    // The control: the cause put back, a key naming generation 0 of the session.
    let dead = Plain { n: 3, names: Some(generation_0) };
    let refused = catch_unwind(AssertUnwindSafe(|| session_plain(&third, dead))).map_err(panic_text);
    let why = refused.expect_err("a key naming a dropped making of the session was answered");
    assert!(why.contains("is dead") && why.contains(&format!("{generation_0:?}")), "{why}");
}

// A session-homed run that reads a crate's arena names who asked it (2026-11-15).

thread_local! {
    /// The input in a crate's arena [`session_reader`] reads: planted out of its key, so the
    /// key names no arena and the run is homed at the session.
    static PLANTED: Cell<Option<Doc>> = const { Cell::new(None) };
}

/// Homed at the session (its key is a `u32`) and reads the planted input of a crate's arena:
/// the mistake the engine refuses.
#[blaze_memo::tracked(returns(copy))]
fn session_reader(db: &dyn Db, n: u32) -> u64 {
    let doc = PLANTED.with(Cell::get).expect("a planted input");
    doc.text(db).len() as u64 + u64::from(n)
}

/// The query that asks [`session_reader`]: the refusal must name it.
#[blaze_memo::tracked(returns(copy))]
fn session_asker(db: &dyn Db, n: u32) -> u64 {
    session_reader(db, n + 1)
}

/// The refusal of a session-homed run reading a crate's arena names the runs that asked it,
/// root first, by kind and key: the outer query's kind is in the message, after "asked
/// through". The invariant and the panic are unchanged; this is their message.
#[test]
fn a_session_run_reading_a_crate_arena_names_who_asked() {
    let mut builder = Memo::builder(config(Plant::None));
    builder.arena(0, Tag::Query);
    let inputs = builder.arena(1, Tag::Query);
    let fact = Fact { id: ContentId::of("additions planted", b"id"), digest: ContentId::of("additions planted", b"1") };
    let doc = Doc::new(&inputs, fact, "planted".to_owned());
    let db = db(builder.build());
    PLANTED.with(|it| it.set(Some(doc)));
    let refused = catch_unwind(AssertUnwindSafe(|| session_asker(&db, 1))).map_err(panic_text);
    PLANTED.with(|it| it.set(None));
    let why = refused.expect_err("a session-homed run read a crate's arena unrefused");
    assert!(why.contains("is homed at the session arena"), "{why}");
    let (reader, path) = why.split_once("asked through ").unwrap_or_else(|| panic!("no path: {why}"));
    assert!(reader.contains("session_reader"), "{why}");
    assert!(path.contains("session_asker"), "the outer query is not named: {why}");
    assert!(!path.contains("session_reader"), "the path names the reader as its own asker: {why}");
}

// The run log renders a key only when read (2026-11-14).

/// A key whose `Debug` prints only with a database attached, as hir-ty's solver ids do
/// (`SolverDefId` refuses to print detached).
#[derive(Clone, Copy, PartialEq, Eq, Hash, blaze_memo::Home)]
pub struct Shy {
    pub n: u32,
}

impl std::fmt::Debug for Shy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let attached = blaze_memo::with_attached(|memo| memo.is_some());
        assert!(attached, "Shy({}) printed with no database attached", self.n);
        write!(f, "Shy({})", self.n)
    }
}

#[blaze_memo::tracked(returns(copy))]
fn shy_twice(_db: &dyn Db, key: Shy) -> u64 {
    u64::from(key.n) * 2
}

/// A snapshot with a session arena only, logging its runs into `log`.
fn logged_session(plant: Plant, log: &Arc<RunLog>) -> TestDb {
    let mut builder = Memo::builder(config(plant));
    builder.log_runs(Arc::clone(log));
    builder.arena(blaze_memo::ArenaId::SESSION_INDEX, Tag::Query);
    db(builder.build())
}

/// The invariant: a logged run formats nothing when it starts (a run starts with no database
/// attached); its row keeps the query and the key typed, is found by `Ran::is` without text,
/// and is written as text only when read, inside `attach`. The controls: read outside
/// `attach`, the key refuses to print (so the row did hold the key unformatted, and the
/// asserts above are what made it print); and the cause put back, a log that formats the key
/// at the run's start (`Plant::LogFormatsEagerly`), panics in the ask itself.
#[test]
fn a_run_log_row_renders_its_key_only_when_read() {
    let log = Arc::new(RunLog::new());
    let db = logged_session(Plant::None, &log);
    let key = Shy { n: 5 };
    assert_eq!(shy_twice(&db, key), 10, "the run logged without printing its key");
    let ran = log.take();
    let row = ran.iter().find(|it| it.is::<shy_twice>(&(key,))).expect("the run is logged");
    assert!(!row.is::<shy_twice>(&(Shy { n: 6 },)), "another key");
    assert_eq!(log.count("additions::shy_twice"), 0, "taken");
    let printed = attach(&db, || row.key());
    assert!(printed.starts_with("additions::shy_twice(") && printed.contains("Shy(5)"), "{printed}");
    let listed = attach(&db, || format!("{ran:?}"));
    assert!(listed.contains("Shy(5)"), "{listed}");
    assert_eq!(ran[0].clone(), ran[0], "rows compare by typed key, with no text");

    let detached = catch_unwind(AssertUnwindSafe(|| row.key())).map_err(panic_text);
    let why = detached.expect_err("the key printed with no database attached");
    assert!(why.contains("Shy(5) printed with no database attached"), "{why}");

    let eager_log = Arc::new(RunLog::new());
    let eager = logged_session(Plant::LogFormatsEagerly, &eager_log);
    let planted = catch_unwind(AssertUnwindSafe(|| shy_twice(&eager, key))).map_err(panic_text);
    let why = planted.expect_err("a log formatting at the run's start printed a detached key");
    assert!(why.contains("Shy(5) printed with no database attached"), "{why}");
}
