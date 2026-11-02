//! Model tests (docs/v2/plan/lanes/safety/model.md) of the store's lock-free structures: a
//! table's slots and levels, the flag bits of a table and of an index, and a segment chunk's
//! published bytes, each from 2 or 3 threads under every interleaving to the preemption bound
//! and every value C11 lets a load read, against a sequential model. A table value holds a
//! tracked cell written before it is published and when it is freed, and chunk bytes are
//! tracked to the byte (`sync::track`), so a read not ordered after the write it depends on, or
//! a free not ordered after a read, is a data race the checker reports.

use std::{collections::BTreeMap, sync::Arc};

use blaze_check::{
    cell::UnsafeCell,
    linear::{History, Spec},
    model, thread,
};
use blaze_ids::ContentId;
use blaze_reclaim::enter;

use crate::{
    Key, Kind, Unit,
    disk::{Form, RECORD_HEADER, RecordHeader, content_of},
    flags::{Flag, FlagColumns},
    segment::{FRAME, Own, Segment},
    sync::atomic::{AtomicUsize, Ordering},
    table::{Held, Keyed, Table},
};

/// A table value. `n` is written before the value is published and when it is freed, and read
/// by every reader that finds it; `drops` counts frees.
struct Val {
    key: u64,
    n: UnsafeCell<u64>,
    drops: Arc<AtomicUsize>,
}

// SAFETY: `n` is reached only through `with`, `with_mut` and `get_mut`, each an access the model
// tracks. It runs every thread on one OS thread, so no two accesses overlap, and one its clocks
// do not order is reported as a race.
unsafe impl Sync for Val {}

impl Val {
    /// A value whose `n` is written once here: the cell's tracking moves with it into the table.
    fn new(key: u64, n: u64, drops: &Arc<AtomicUsize>) -> Val {
        let val = Val { key, n: UnsafeCell::new(0), drops: Arc::clone(drops) };
        // SAFETY: a `u64` of a value no other thread can reach yet.
        val.n.with_mut(|it| unsafe { *it = n });
        val
    }

    fn n(&self) -> u64 {
        // SAFETY: a `u64` of a live value: the caller found it under a `Held`.
        self.n.with(|it| unsafe { *it })
    }
}

impl Drop for Val {
    fn drop(&mut self) {
        *self.n.get_mut() = u64::MAX;
        self.drops.fetch_add(1, Ordering::Relaxed);
    }
}

impl Keyed for Val {
    type Key = u64;

    fn key(&self) -> u64 {
        self.key
    }

    /// The key itself, so a test puts each key in the slot it means.
    fn hash(key: &u64) -> u64 {
        *key
    }
}

/// A table's sequential model: a map from key to value.
#[derive(Clone, PartialEq, Eq, Hash, Default)] // clone: the checker's `Spec` copies its model (tests)
struct Map(BTreeMap<u64, u64>);

#[derive(Debug)]
enum MapOp {
    Insert(u64, u64),
    Get(u64),
    Replace(u64, u64),
}

#[derive(Clone, PartialEq, Debug)]
enum MapRet {
    /// `Err` with the value the key has already.
    Inserted(Result<(), u64>),
    Got(Option<u64>),
    /// The value replaced; `None` for a key not there.
    Replaced(Option<u64>),
}

impl Spec for Map {
    type Op = MapOp;
    type Ret = MapRet;

    fn apply(&mut self, op: &MapOp) -> MapRet {
        match *op {
            MapOp::Insert(key, n) => match self.0.get(&key) {
                Some(&kept) => MapRet::Inserted(Err(kept)),
                None => {
                    self.0.insert(key, n);
                    MapRet::Inserted(Ok(()))
                }
            },
            MapOp::Get(key) => MapRet::Got(self.0.get(&key).copied()),
            MapOp::Replace(key, n) => MapRet::Replaced(self.0.get_mut(&key).map(|it| std::mem::replace(it, n))),
        }
    }
}

fn insert(table: &Table<Val>, history: &History<Map>, key: u64, n: u64, drops: &Arc<AtomicUsize>) {
    let guard = enter();
    history.record(MapOp::Insert(key, n), |_| match table.insert(Held::task(&guard), Val::new(key, n, drops)) {
        Ok(_) => MapRet::Inserted(Ok(())),
        Err((_, (_, kept))) => MapRet::Inserted(Err(kept.n())),
    });
}

fn get(table: &Table<Val>, history: &History<Map>, key: u64) {
    let guard = enter();
    history.record(MapOp::Get(key), |_| MapRet::Got(table.get(Held::task(&guard), &key).map(|(_, it)| it.n())));
}

/// Inserts and lookups from three threads, two keys racing for one slot (1 and 65 both start
/// at slot 1 of 64): each key keeps the first value published, a refused insert gets that
/// value back, a lookup reads a value whole, and every value is freed once.
#[test]
fn model_table_inserts_and_gets_are_a_map() {
    model(|| {
        let drops = Arc::new(AtomicUsize::new(0));
        let table = Arc::new(Table::<Val>::new(64, 0));
        let history = Arc::new(History::<Map>::new());
        let other = {
            let (table, history, drops) = (Arc::clone(&table), Arc::clone(&history), Arc::clone(&drops));
            thread::spawn(move || {
                insert(&table, &history, 1, 10, &drops);
                get(&table, &history, 65);
            })
        };
        let third = {
            let (table, history, drops) = (Arc::clone(&table), Arc::clone(&history), Arc::clone(&drops));
            thread::spawn(move || {
                insert(&table, &history, 1, 11, &drops);
                insert(&table, &history, 65, 20, &drops);
            })
        };
        get(&table, &history, 1);
        other.join().unwrap();
        third.join().unwrap();
        history.check(Map::default());
        drop(table);
        assert_eq!(drops.load(Ordering::Relaxed), 3, "every value was freed once");
    });
}

/// The store replaces a value while a reader looks it up: the reader reads the old value or
/// the new one whole, and the old one is freed only once the reader's task has ended.
#[test]
fn model_table_replace_frees_the_old_value_after_its_readers() {
    model(|| {
        let drops = Arc::new(AtomicUsize::new(0));
        let table = Arc::new(Table::<Val>::new(64, 0));
        let history = Arc::new(History::<Map>::new());
        insert(&table, &history, 5, 1, &drops);
        let reader = {
            let (table, history) = (Arc::clone(&table), Arc::clone(&history));
            thread::spawn(move || {
                get(&table, &history, 5);
                get(&table, &history, 5);
            })
        };
        history.record(MapOp::Replace(5, 2), |_| match table.replace(Held::writer(&*table), Val::new(5, 2, &drops)) {
            Ok(old) => MapRet::Replaced(Some(old.n())),
            Err(_) => MapRet::Replaced(None),
        });
        table.collect();
        reader.join().unwrap();
        table.collect();
        assert_eq!(table.retired(), 0, "a grace outlived every task");
        assert_eq!(drops.load(Ordering::Relaxed), 1, "the replaced value was freed once");
        history.check(Map::default());
        drop(table);
        assert_eq!(drops.load(Ordering::Relaxed), 2);
    });
}

/// Two inserts of one key while level 0 reaches half full: one lands, the other is refused
/// with it.
#[test]
fn model_table_racing_inserts_of_a_key_across_a_new_level_keep_one() {
    model(|| {
        let drops = Arc::new(AtomicUsize::new(0));
        let table = Arc::new(Table::<Val>::new(64, 0));
        let history = Arc::new(History::<Map>::new());
        {
            // 31 values in slots 32 to 62: level 0 takes one more before it is half full.
            let guard = enter();
            for key in 32..63 {
                assert!(table.insert(Held::task(&guard), Val::new(key, key, &drops)).is_ok());
            }
        }
        let other = {
            let (table, history, drops) = (Arc::clone(&table), Arc::clone(&history), Arc::clone(&drops));
            thread::spawn(move || insert(&table, &history, 3, 1, &drops))
        };
        insert(&table, &history, 3, 2, &drops);
        other.join().unwrap();
        history.check(Map::default());
    });
}

/// Flag columns with a current period and the one before.
trait Periodic: Send + Sync + 'static {
    fn set_now(&self, row: u64);
    fn start_period(&self);
    /// Whether `row` is set now, and in the period before.
    fn periods(&self, row: u64) -> (bool, bool);
}

impl Periodic for FlagColumns {
    fn set_now(&self, row: u64) {
        self.set(row, Flag::Read);
    }

    fn start_period(&self) {
        self.rotate();
    }

    fn periods(&self, row: u64) -> (bool, bool) {
        (self.get(row, Flag::Read), self.get(row, Flag::ReadBefore))
    }
}

impl Periodic for Table<Val> {
    fn set_now(&self, row: u64) {
        self.set_flag(row, 0);
    }

    fn start_period(&self) {
        self.rotate(0, 1);
    }

    fn periods(&self, row: u64) -> (bool, bool) {
        (self.flag(row, 0), self.flag(row, 1))
    }
}

/// The flag columns' sequential model: the rows set this period, and in the one before.
#[derive(Clone, PartialEq, Eq, Hash, Default)]
struct Periods {
    now: u128,
    before: u128,
}

#[derive(Debug)]
enum FlagOp {
    Set(u64),
    Rotate,
    Get(u64),
}

impl Spec for Periods {
    type Op = FlagOp;
    type Ret = Option<(bool, bool)>;

    fn apply(&mut self, op: &FlagOp) -> Option<(bool, bool)> {
        match *op {
            FlagOp::Set(row) => {
                self.now |= 1 << row;
                None
            }
            FlagOp::Rotate => {
                self.before = std::mem::take(&mut self.now);
                None
            }
            FlagOp::Get(row) => Some((self.now >> row & 1 != 0, self.before >> row & 1 != 0)),
        }
    }
}

/// Two threads set flags (rows 1 and 2 in one word, `far` on its own) while the store starts a
/// period: no flag is lost, and each lands in the period before or the current one. A thread's
/// rows share a word: `rotate` moves a word at a time, so one thread's flags in two words may
/// land in the reverse order of a rotation, which a flag (a hint) allows.
fn flags_land_in_one_period_or_the_other<P: Periodic>(make: fn() -> P, far: u64) {
    model(move || {
        let flags = Arc::new(make());
        let history = Arc::new(History::<Periods>::new());
        let set = |rows: Vec<u64>| {
            let (flags, history) = (Arc::clone(&flags), Arc::clone(&history));
            thread::spawn(move || {
                for row in rows {
                    history.record(FlagOp::Set(row), |_| {
                        flags.set_now(row);
                        None
                    });
                }
            })
        };
        let (near, other) = (set(vec![1, 2]), set(vec![far]));
        history.record(FlagOp::Rotate, |_| {
            flags.start_period();
            None
        });
        near.join().unwrap();
        other.join().unwrap();
        for row in [1, 2, 3, far] {
            history.record(FlagOp::Get(row), |_| Some(flags.periods(row)));
        }
        history.check(Periods::default());
    });
}

#[test]
fn model_flag_bits_land_in_one_period_or_the_other() {
    flags_land_in_one_period_or_the_other(|| FlagColumns::new(128), 65);
}

#[test]
fn model_table_flags_land_in_one_period_or_the_other() {
    flags_land_in_one_period_or_the_other(|| Table::<Val>::new(64, 2), 40);
}

/// What a segment chunk holds: how many records it published.
#[derive(Clone, PartialEq, Eq, Hash, Default)]
struct Records(u64);

#[derive(Debug)]
enum ChunkOp {
    Put,
    Read,
}

impl Spec for Records {
    type Op = ChunkOp;
    type Ret = Option<u64>;

    fn apply(&mut self, op: &ChunkOp) -> Option<u64> {
        match op {
            ChunkOp::Put => {
                self.0 += 1;
                None
            }
            ChunkOp::Read => Some(self.0),
        }
    }
}

/// The `n`th record the writer puts: a header and 24 bytes of `n`.
fn record_of(n: u8) -> (RecordHeader, Vec<u8>) {
    let payload = vec![n; 24];
    let header = RecordHeader {
        kind: Kind::Code,
        form: Form::Raw,
        dictionary: 0,
        stored: payload.len() as u64,
        raw: payload.len() as u64,
        key: Key(ContentId([n; 32])),
        content: content_of(&payload),
    };
    (header, payload)
}

/// Reads every record below what the segment's chunk published, as a reader of a spot would.
fn read_chunk(segment: &Segment, history: &History<Records>) {
    history.record(ChunkOp::Read, |_| {
        // SAFETY: no store runs here to unlink a chunk, so the segment's first chunk lives as
        // long as the segment (`ChunkRef::get`'s contract), which the caller holds.
        let chunk = unsafe { segment.first().get() };
        let (published, _) = chunk.published();
        let (mut at, mut records) = (0, 0u8);
        while at < published {
            records += 1;
            let (want, payload) = record_of(records);
            // SAFETY: the header of a record below what the chunk published (`Chunk::bytes`'s
            // contract): the writer writes only past it.
            let header = unsafe { chunk.bytes(at + FRAME, RECORD_HEADER) }
                .map(RecordHeader::decode)
                .expect("a published record is in its chunk")
                .expect("a published record's header is whole");
            assert_eq!(header, want, "record {records} read back");
            // SAFETY: as above, for its payload.
            let read = unsafe { chunk.bytes(at + FRAME + RECORD_HEADER, header.stored) }
                .expect("a published record is in its chunk");
            assert_eq!(read, &payload[..], "record {records}'s payload read back");
            at += FRAME + header.span();
        }
        assert_eq!(at, published, "a chunk publishes whole records");
        Some(u64::from(records))
    });
}

/// A writer puts two records into its segment while two readers read its chunk: each reads
/// whole records below the published word, never bytes the writer is writing past it.
#[test]
fn model_a_segment_chunk_is_read_whole_below_what_it_published() {
    model(|| {
        let mut own = Own::new(1);
        let segment = Arc::clone(own.segment());
        let history = Arc::new(History::<Records>::new());
        let writer = {
            let history = Arc::clone(&history);
            thread::spawn(move || {
                for n in 1..=2 {
                    let (header, payload) = record_of(n);
                    history.record(ChunkOp::Put, |_| {
                        let (_, span) = own.write(Unit::SHARED, &header, &payload, 0);
                        own.keep(span);
                        None
                    });
                }
            })
        };
        let reader = {
            let (segment, history) = (Arc::clone(&segment), Arc::clone(&history));
            thread::spawn(move || read_chunk(&segment, &history))
        };
        read_chunk(&segment, &history);
        writer.join().unwrap();
        reader.join().unwrap();
        history.check(Records::default());
    });
}
