//! The keys' records (docs/v2/plan/lanes/fastcompile/sched.md, "The table"): what each task
//! key's last runs took and held, in a table of fixed size that one writer changes and any
//! thread reads with no lock and no write to shared memory.
//!
//! **Layout.** A record is 64 bytes, one cache line, in buckets of [`WAYS`]. A key's bucket is
//! its own first eight bytes (it is a content hash already) mixed with its kind and masked, so
//! a lookup hashes nothing and reads one bucket: four lines, two of the pairs that arm64 and
//! x86 prefetch together, loaded at once. The writer compares every way with no branch on what
//! it holds, so where a key sits costs no misprediction; a reader stops at the first empty way.
//! A bucket fills from its first way and never empties one (bytes read back with a gap are
//! refused). A bucket that is full when a new key comes drops its oldest record by
//! the writer's clock (a count of observations, not time), so the table holds the keys seen
//! most lately, never more than its fixed size, and the same observations leave the same
//! records in the same slots.
//!
//! **Reads.** Each record is a seqlock over atomics: the writer makes the record's sequence
//! odd, stores the fields relaxed after a release fence, and makes the sequence even again with
//! a release store; a reader loads the sequence with acquire, the fields relaxed, and the
//! sequence again after an acquire fence, and keeps the fields only when both loads gave the
//! same even number. A read writes nothing shared, so readers on every core scale; one that met
//! a write under way reads again. A table has one [`Writer`], which is not `Sync`, so no two
//! writes race, and the writer reads its own records with plain relaxed loads.

use std::{
    cell::Cell,
    marker::PhantomData,
    sync::{
        Arc,
        atomic::{
            AtomicU64,
            Ordering::{Acquire, Relaxed, Release},
            fence,
        },
    },
};

use blaze_graph::{Key, admission::TaskKind};

/// Records a bucket holds.
pub const WAYS: usize = 4;
/// Runs a record keeps the time of.
pub const RUNS: usize = 4;

/// One key's record, as read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Record {
    pub kind: TaskKind,
    /// The key's first 16 bytes. Two keys of a kind that share them share a record: a wrong
    /// prediction costs only an order or an admission, never a result.
    pub key: [u64; 2],
    /// Its runs seen, saturating.
    pub seen: u32,
    /// The writer's clock at its last run: its age.
    pub stamp: u64,
    /// The peak its next run is predicted to hold, in bytes: a peak at or over it becomes it at
    /// once, and one under it brings it half the way down.
    pub peak: u64,
    /// Its last runs' times in microseconds (saturating at 71 minutes), a ring: run `n`
    /// (counting from 0) at `n % RUNS`; the slots no run reached yet are 0.
    pub runs: [u32; RUNS],
}

impl Record {
    /// The first run of `kind`'s `key`, at `stamp`.
    pub fn first(kind: TaskKind, key: [u64; 2], stamp: u64, ran_us: u32, peak: u64) -> Record {
        let mut runs = [0; RUNS];
        runs[0] = ran_us;
        Record { kind, key, seen: 1, stamp, peak, runs }
    }

    /// Another run of it, at `stamp`, that took `ran_us` and held `peak` bytes at most.
    pub fn observe(&mut self, stamp: u64, ran_us: u32, peak: u64) {
        self.runs[self.seen as usize % RUNS] = ran_us;
        self.seen = self.seen.saturating_add(1);
        self.stamp = stamp;
        self.peak = if peak >= self.peak { peak } else { self.peak - (self.peak - peak) / 2 };
    }

    /// The time its next run is predicted to take, in microseconds: the median of its last
    /// runs, the middle two's mean rounded up for an even count, so one slow or fast run
    /// moves it little. Min and max only, with no branch on the times: every prediction of a
    /// seen key pays it.
    pub fn ran_us(&self) -> u64 {
        let [a, b, c, d] = self.runs.map(u64::from);
        let (low, high) = (a.min(b), a.max(b));
        match self.seen {
            0 => 0,
            1 => a,
            2 => (a + b).div_ceil(2),
            3 => low.max(high.min(c)),
            // Of the pairs' lows the lesser is the least of all four and of their highs the
            // greater the most, so the other low and the other high are the middle two.
            _ => (low.max(c.min(d)) + high.min(c.max(d))).div_ceil(2),
        }
    }

    /// Whether observations make it: seen, stamped, and no run time where no run reached.
    pub(crate) fn is_sound(&self) -> bool {
        let filled = (self.seen as usize).min(RUNS);
        self.seen > 0 && self.stamp > 0 && self.runs[filled..].iter().all(|&it| it == 0)
    }
}

/// A record's fields as words: `kind << 32 | seen`, the stamp, the key's halves, the peak, and
/// the runs two to a word, the lower first.
type Words = [u64; 7];

fn pack(record: &Record) -> Words {
    let runs = record.runs.map(u64::from);
    [
        (u64::from(record.kind.0) << 32) | u64::from(record.seen),
        record.stamp,
        record.key[0],
        record.key[1],
        record.peak,
        runs[0] | (runs[1] << 32),
        runs[2] | (runs[3] << 32),
    ]
}

/// The record of `words`; `None` for an empty slot (`seen` 0).
fn unpack(words: Words) -> Option<Record> {
    let seen = words[0] as u32;
    (seen != 0).then(|| Record {
        kind: TaskKind((words[0] >> 32) as u32),
        seen,
        stamp: words[1],
        key: [words[2], words[3]],
        peak: words[4],
        runs: [words[5] as u32, (words[5] >> 32) as u32, words[6] as u32, (words[6] >> 32) as u32],
    })
}

/// One record's slot: its sequence and its seven words. 64 bytes.
#[repr(C, align(64))]
struct Slot {
    /// Odd while a write is under way.
    seq: AtomicU64,
    words: [AtomicU64; 7],
}

const _: () = assert!(size_of::<Slot>() == 64);

impl Slot {
    fn empty() -> Slot {
        Slot { seq: AtomicU64::new(0), words: std::array::from_fn(|_| AtomicU64::new(0)) }
    }

    fn load(&self) -> Words {
        std::array::from_fn(|at| self.words[at].load(Relaxed))
    }

    /// Its record as the writer reads it: no write races the loads.
    fn peek(&self) -> Option<Record> {
        unpack(self.load())
    }

    /// Its record as any thread reads it: a whole one, never one torn by a write under way.
    fn read(&self) -> Option<Record> {
        let mut read = None;
        // The one writer's write is a few stores.
        blaze_graph::spin::SpinWait::until("a history record's write under way", || {
            let before = self.seq.load(Acquire);
            if before & 1 == 0 {
                let words = self.load();
                fence(Acquire);
                if self.seq.load(Relaxed) == before {
                    read = Some(unpack(words));
                    return true;
                }
            }
            false
        });
        read.expect("a read that ended read a record")
    }

    /// Puts `record` in it. Only the table's one writer calls it.
    fn write(&self, record: &Record) {
        let seq = self.seq.load(Relaxed);
        self.seq.store(seq.wrapping_add(1), Relaxed);
        fence(Release);
        for (word, value) in self.words.iter().zip(pack(record)) {
            word.store(value, Relaxed);
        }
        self.seq.store(seq.wrapping_add(2), Release);
    }

    /// Whether it holds `kind`'s `key`, as the writer reads it: its kind, seen count and key
    /// compared with no branch between them.
    fn holds(&self, kind: TaskKind, key: [u64; 2]) -> bool {
        let head = self.words[0].load(Relaxed);
        (head as u32 != 0)
            & (head >> 32 == u64::from(kind.0))
            & (self.words[2].load(Relaxed) == key[0])
            & (self.words[3].load(Relaxed) == key[1])
    }

    /// Puts `record` in it with plain stores and no fence: no reader shares it.
    fn set(&mut self, record: &Record) {
        for (word, value) in self.words.iter_mut().zip(pack(record)) {
            *word.get_mut() = value;
        }
    }
}

/// The slots, `buckets * WAYS` of them: 64 bytes a key, a lookup one bucket.
struct Table {
    slots: Box<[Slot]>,
    /// The bucket count less one, a power of two less one.
    mask: u64,
}

/// The first slot of the bucket of `kind`'s `key` in a table of `mask + 1` buckets.
fn bucket(mask: u64, kind: TaskKind, key: [u64; 2]) -> usize {
    ((key[0] ^ u64::from(kind.0).wrapping_mul(0x9e37_79b9_7f4a_7c15)) & mask) as usize * WAYS
}

/// The first slot of the bucket of `kind`'s `key` in a table of `slots` slots.
pub(crate) fn bucket_in(slots: usize, kind: TaskKind, key: [u64; 2]) -> usize {
    bucket((slots / WAYS - 1) as u64, kind, key)
}

/// A task key's first 16 bytes, as two words.
pub fn halves(key: &Key) -> [u64; 2] {
    let [a, b, c, d, e, f, g, h, i, j, k, l, m, n, o, p, ..] = key.0;
    [u64::from_le_bytes([a, b, c, d, e, f, g, h]), u64::from_le_bytes([i, j, k, l, m, n, o, p])]
}

impl Table {
    fn new(slots: usize) -> Table {
        assert!(
            slots.is_power_of_two() && slots >= WAYS,
            "a history's table has a power of two slots, at least {WAYS}, not {slots}"
        );
        Table {
            slots: std::iter::repeat_with(Slot::empty).take(slots).collect(), // alloc: the table's one region, at daemon start
            mask: (slots / WAYS - 1) as u64,
        }
    }

    fn bucket(&self, kind: TaskKind, key: [u64; 2]) -> usize {
        bucket(self.mask, kind, key)
    }

    /// The slot of `kind`'s `key` and its record, each slot of its bucket read by `read` up to
    /// the first empty one: a bucket fills from its first way and never empties one, so no
    /// record lies past an empty way.
    fn find(
        &self,
        kind: TaskKind,
        key: [u64; 2],
        read: impl Fn(&Slot) -> Option<Record>,
    ) -> Option<(usize, Record)> {
        let first = self.bucket(kind, key);
        for at in first..first + WAYS {
            let record = read(&self.slots[at])?;
            if record.kind == kind && record.key == key {
                return Some((at, record));
            }
        }
        None
    }

    /// Where `kind`'s `key` is or goes, for the writer, whose loads no write races: every way
    /// of its bucket read and compared with no branch on what it holds, so the way a key sits in
    /// costs no misprediction. The way holding its record, else the first empty way, else the
    /// way seen longest ago.
    fn way(&self, kind: TaskKind, key: [u64; 2]) -> Way {
        let first = self.bucket(kind, key);
        let (mut hit, mut empty, mut oldest, mut oldest_at) = (WAYS, WAYS, u64::MAX, 0);
        for (at, slot) in self.slots[first..first + WAYS].iter().enumerate().rev() {
            let seen = slot.words[0].load(Relaxed) as u32 != 0;
            let stamp = slot.words[1].load(Relaxed);
            hit = if slot.holds(kind, key) { at } else { hit };
            empty = if seen { empty } else { at };
            (oldest, oldest_at) = if seen & (stamp < oldest) { (stamp, at) } else { (oldest, oldest_at) };
        }
        let held = hit < WAYS;
        let at = if held { hit } else if empty < WAYS { empty } else { oldest_at };
        Way { at: first + at, held, evicts: !held & (empty == WAYS) }
    }
}

/// Where [`Table::way`] puts a key.
struct Way {
    at: usize,
    /// Whether the slot holds the key's record.
    held: bool,
    /// Whether a new record there drops another key's.
    evicts: bool,
}

/// The one writer of a table: it observes runs, and gives [`Reader`]s to other threads.
pub struct Writer {
    table: Arc<Table>,
    clock: u64,
    evicted: u64,
    /// Not `Sync`: one thread writes at a time, so no two writes to a record race.
    _one: PhantomData<Cell<()>>,
}

impl Writer {
    /// An empty table of `slots` records (a power of two, at least [`WAYS`]).
    ///
    /// # Panics
    ///
    /// When `slots` is not such a number.
    pub fn new(slots: usize) -> Writer {
        Writer { table: Arc::new(Table::new(slots)), clock: 0, evicted: 0, _one: PhantomData } // alloc: one per history, shared by its readers
    }

    pub fn slots(&self) -> usize {
        self.table.slots.len()
    }

    /// The observations made: the stamp of the last.
    pub fn clock(&self) -> u64 {
        self.clock
    }

    /// The records dropped for a newer key of their bucket.
    pub fn evicted(&self) -> u64 {
        self.evicted
    }

    /// A reader of the same table, for any thread: one reference count, taken once.
    pub fn reader(&self) -> Reader {
        Reader { table: self.table.clone() } // clone: one reference count per reader, taken once
    }

    /// The record of `kind`'s `key`, if the table holds it.
    pub fn get(&self, kind: TaskKind, key: &Key) -> Option<Record> {
        let key = halves(key);
        let first = self.table.bucket(kind, key);
        let ways = &self.table.slots[first..first + WAYS];
        let mut hit = WAYS;
        for (at, slot) in ways.iter().enumerate() {
            hit = if slot.holds(kind, key) { at } else { hit };
        }
        ways.get(hit).and_then(Slot::peek)
    }

    /// A run of `kind`'s `key` took `ran_us` and held `peak` bytes at most: its record learns
    /// it, or a new one takes the bucket's first empty slot, else its oldest.
    pub fn observe(&mut self, kind: TaskKind, key: &Key, ran_us: u32, peak: u64) {
        self.clock += 1;
        let stamp = self.clock;
        let key = halves(key);
        let way = self.table.way(kind, key);
        let slot = &self.table.slots[way.at];
        let record = match slot.peek().filter(|_| way.held) {
            Some(mut record) => {
                record.observe(stamp, ran_us, peak);
                record
            }
            None => Record::first(kind, key, stamp, ran_us, peak),
        };
        self.evicted += u64::from(way.evicts);
        slot.write(&record);
    }

    /// Every record and its slot, in slot order.
    pub fn records(&self) -> impl Iterator<Item = (usize, Record)> + '_ {
        self.table.slots.iter().enumerate().filter_map(|(at, slot)| Some((at, slot.peek()?)))
    }

    /// The table of a writer no [`Reader`] shares yet, to fill with plain stores: a table read
    /// back, before [`Writer::reader`] is first called.
    fn unshared(table: &mut Arc<Table>) -> &mut Table {
        Arc::get_mut(table).expect("a history's table is read back before any reader shares it")
    }

    /// Puts each of `records`, stamp and all, in its slot `at` of its bucket: a table read
    /// back into the size it had.
    pub(crate) fn put_all(&mut self, records: &[(usize, Record)]) {
        let table = Writer::unshared(&mut self.table);
        for (at, record) in records {
            debug_assert_eq!(at - at % WAYS, table.bucket(record.kind, record.key));
            table.slots[*at].set(record);
        }
    }

    /// Puts each of `records`, stamp and all, where a new key of it would go, in the order
    /// given: a table read back into another size, its records given oldest first.
    pub(crate) fn place_all(&mut self, records: impl Iterator<Item = Record>) {
        let table = Writer::unshared(&mut self.table);
        for record in records {
            let first = table.bucket(record.kind, record.key);
            let mut free = None;
            let (mut oldest, mut oldest_at) = (u64::MAX, first);
            for at in first..first + WAYS {
                match table.slots[at].peek() {
                    Some(it) if it.stamp < oldest => (oldest, oldest_at) = (it.stamp, at),
                    Some(_) => {}
                    None => {
                        free = Some(at);
                        break;
                    }
                }
            }
            let at = free.unwrap_or_else(|| {
                self.evicted += 1;
                oldest_at
            });
            table.slots[at].set(&record);
        }
    }

    /// Its clock and count of records dropped, as a table read back had them.
    pub(crate) fn set_counts(&mut self, clock: u64, evicted: u64) {
        self.clock = clock;
        self.evicted = evicted;
    }
}

/// Reads a table from any thread, with no lock and no write to shared memory.
#[derive(Clone)]
pub struct Reader {
    table: Arc<Table>,
}

impl Reader {
    /// The record of `kind`'s `key`, if the table holds it: a whole one, as some write left it.
    pub fn get(&self, kind: TaskKind, key: &Key) -> Option<Record> {
        self.table.find(kind, halves(key), Slot::read).map(|it| it.1)
    }
}
