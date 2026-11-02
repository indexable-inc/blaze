use std::hash::{BuildHasher, Hash};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};

use blaze_mem::Tag;
use blaze_mem::blocks::Slice;
use blaze_mem::lock::SpinLock;
use derive_where::derive_where;
use rustc_hash::FxBuildHasher;

use super::{AvailableDepth, Cx, NestedGoals, RequiredDepth};
use crate::data_structures::HashMap;
use crate::search_graph::EvaluationResult;

/// One published result of a goal. Never changed once published: a later result of the same
/// goal is a new row the goal's column points at instead.
struct Row<X: Cx> {
    /// The result's index in `GlobalCache::results`.
    result: X::Tracked<u32>,
    /// In a success row, the depth its result needs; in an overflow row, the depth it was
    /// computed with.
    depth: u32,
    /// Its nested goals' index in `GlobalCache::nested_goals`, plus one; 0 for none, which is
    /// all but the goals that overflowed or were in a cycle.
    nested_goals: u32,
    /// In an overflow row, the goal's overflow row before it, plus one; 0 for none.
    next: u32,
}

#[derive_where(Debug; X: Cx)]
pub(super) struct CacheData<'a, X: Cx> {
    pub(super) result: X::Result,
    pub(super) required_depth: RequiredDepth,
    pub(super) encountered_overflow: bool,
    pub(super) nested_goals: &'a NestedGoals<X>,
}

/// The goals solved and their results, each goal and each distinct result interned as a `u32`,
/// read and written through `&self` by any number of threads.
///
/// A lookup takes no lock and writes no shared memory: it is acquire loads of slots published
/// once with a release store (`docs/v2/plan/design/concurrency.md`). Tables grow by a new
/// level and values never move, so a reader never waits on a writer. Writers to one goal take
/// one of `writers`, by goal, only to publish its row.
///
/// Its tables are blocks of `blaze_mem` ([`table`]), and its locks never allocate: a cache made
/// per interner costs the global allocator nothing, and the blocks of one dropped (its
/// interner's generation evicted) are the next one's.
///
/// `success` holds a goal's latest result whose computation never hit the recursion limit;
/// `with_overflow` the results which hit it, newest first, each with the depth it was computed
/// with.
///
/// Cost:
/// - A goal costs its input plus 8 bytes of hash and two 4-byte slots (tables at most half
///   full), plus 4 bytes in each of `success` and `with_overflow`.
/// - A distinct result costs the same, without the two columns.
/// - A row costs `Tracked<u32>` plus 12 bytes and the `OnceLock` state word. Nested goals are
///   a side column, filled only when a goal has some.
/// - A hit is one FxHash of the input, about 1.5 slot probes and three dependent acquire loads
///   (goal, row, result), and allocates nothing.
///
/// The benchmark is rustc's `new_solver_evaluation_cache`: one `HashMap` behind one lock for
/// the session, with an entry of about 120 bytes per goal and a copy of the result per goal.
/// Reads here take no lock, and goals that end in one result share it. Where rustc is ahead:
/// it hashes the input once for lookup and insert together. The search graph solves between
/// the two, so no entry can be held across, and this hashes the input again on insert.
pub struct GlobalCache<X: Cx> {
    goals: Interned<X::Input>,
    results: Interned<X::Result>,
    /// By goal: its latest success row, plus one; 0 for none.
    success: Chunked<AtomicU32>,
    /// By goal: its latest overflow row, plus one; 0 for none.
    with_overflow: Chunked<AtomicU32>,
    rows: Chunked<OnceLock<Row<X>>>,
    /// The first row index no writer has claimed.
    next_row: AtomicU64,
    /// The non-empty nested goals of rows, by `Row::nested_goals` minus one.
    nested_goals: Chunked<OnceLock<NestedGoals<X>>>,
    next_nested: AtomicU64,
    /// By goal: held while publishing one of its rows, with the row and nested-goal indices
    /// its holder hands out.
    writers: OnceLock<Slice<Writer>>,
    /// What a row without nested goals has.
    no_nested_goals: NestedGoals<X>,
}

impl<X: Cx> Default for GlobalCache<X> {
    fn default() -> Self {
        GlobalCache::new()
    }
}

impl<X: Cx> GlobalCache<X> {
    #[inline]
    pub const fn new() -> Self {
        GlobalCache {
            goals: Interned::new(),
            results: Interned::new(),
            success: Chunked::new(),
            with_overflow: Chunked::new(),
            rows: Chunked::new(),
            next_row: AtomicU64::new(0),
            nested_goals: Chunked::new(),
            next_nested: AtomicU64::new(0),
            writers: OnceLock::new(),
            no_nested_goals: NestedGoals { nested_goals: HashMap::with_hasher(FxBuildHasher) },
        }
    }

    /// Insert a final result into the global cache.
    pub(super) fn insert(
        &self,
        cx: X,
        input: X::Input,
        evaluation_result: EvaluationResult<X>,
        dep_node: X::DepNodeIndex,
    ) {
        let EvaluationResult { encountered_overflow, required_depth, heads, nested_goals, result } =
            evaluation_result;
        debug_assert!(heads.is_empty());
        let goal = self.goals.intern(input);
        let index = self.results.intern(result);
        let depth = u32::try_from(required_depth.0).expect("a solver depth past `u32::MAX`");
        let writers = self.writers.get_or_init(|| table(shard_count(), |_| Writer::new()));
        let mut claimed = writers[goal as usize & (writers.len() - 1)].0.lock();
        let column = if encountered_overflow { &self.with_overflow } else { &self.success };
        let head = column.get_or_init(goal, || AtomicU32::new(0));
        let latest = head.load(Ordering::Acquire);
        let prev = match encountered_overflow {
            true => self.overflow_row(latest, required_depth),
            false => self.row(latest),
        };
        if let Some(prev) = prev {
            cx.assert_evaluation_is_concurrent();
            // By value: two threads interning one result at once may get two indices.
            let prev = *self.results.get(cx.get_tracked(&prev.result));
            assert_eq!(prev, result, "{input:?}");
        }
        let nested_goals = match nested_goals.is_empty() {
            true => 0,
            false => {
                let raw = claimed.nested_goals.take(&self.next_nested);
                if self.nested_goals.get_or_init(raw, OnceLock::new).set(nested_goals).is_err() {
                    unreachable!("nested goals {raw} handed out twice");
                }
                raw + 1
            }
        };
        let row = Row {
            result: cx.mk_tracked(index, dep_node),
            depth,
            nested_goals,
            next: if encountered_overflow { latest } else { 0 },
        };
        let raw = claimed.rows.take(&self.next_row);
        if self.rows.get_or_init(raw, OnceLock::new).set(row).is_err() {
            unreachable!("row {raw} handed out twice");
        }
        head.store(raw + 1, Ordering::Release);
    }

    /// Try to fetch a cached result, checking the recursion limit
    /// and handling root goals of coinductive cycles.
    ///
    /// If this returns `Some` the cache result can be used.
    pub(super) fn get<'a>(
        &'a self,
        cx: X,
        input: X::Input,
        available_depth: AvailableDepth,
        mut candidate_is_applicable: impl FnMut(&NestedGoals<X>) -> bool,
    ) -> Option<CacheData<'a, X>> {
        let goal = self.goals.find(&input)?;
        if let Some(row) = self.success.get(goal).and_then(|head| self.row(head.load(Ordering::Acquire)))
            && available_depth.cache_entry_is_applicable(RequiredDepth(row.depth as usize))
        {
            let nested_goals = self.nested_goals_of(row);
            if candidate_is_applicable(nested_goals) {
                return Some(CacheData {
                    result: *self.results.get(cx.get_tracked(&row.result)),
                    required_depth: RequiredDepth(row.depth as usize),
                    encountered_overflow: false,
                    nested_goals,
                });
            }
        }

        let additional_depth = RequiredDepth(available_depth.0);
        if let Some(row) = self
            .with_overflow
            .get(goal)
            .and_then(|head| self.overflow_row(head.load(Ordering::Acquire), additional_depth))
        {
            let nested_goals = self.nested_goals_of(row);
            if candidate_is_applicable(nested_goals) {
                return Some(CacheData {
                    result: *self.results.get(cx.get_tracked(&row.result)),
                    required_depth: additional_depth,
                    encountered_overflow: true,
                    nested_goals,
                });
            }
        }

        None
    }

    /// The row `plus_one` names, `None` for 0.
    fn row(&self, plus_one: u32) -> Option<&Row<X>> {
        let raw = plus_one.checked_sub(1)?;
        Some(self.rows.get(raw).and_then(OnceLock::get).expect("a row named before it was published"))
    }

    fn nested_goals_of(&self, row: &Row<X>) -> &NestedGoals<X> {
        match row.nested_goals.checked_sub(1) {
            None => &self.no_nested_goals,
            Some(raw) => self
                .nested_goals
                .get(raw)
                .and_then(OnceLock::get)
                .expect("nested goals named before they were published"),
        }
    }

    /// The newest overflow row computed with `depth`, from the one `plus_one` names back.
    fn overflow_row(&self, mut plus_one: u32, depth: RequiredDepth) -> Option<&Row<X>> {
        while let Some(row) = self.row(plus_one) {
            if row.depth as usize == depth.0 {
                return Some(row);
            }
            plus_one = row.next;
        }
        None
    }
}

/// Chunk `k` holds `FIRST << k` entries, so an index's chunk and place in it are arithmetic,
/// and 27 chunks hold every `u32`. A chunk once made never moves.
const FIRST: usize = 64;
const CHUNKS: usize = 27;

struct Chunked<T> {
    chunks: [OnceLock<Slice<T>>; CHUNKS],
}

/// `len` values, the one at `at` made by `make(at)`, in a block of `blaze_mem` given back when
/// it drops. Memory the system will not give stops the solver with what it wanted.
fn table<T>(len: usize, make: impl FnMut(usize) -> T) -> Slice<T> {
    Slice::new_with(len, Tag::Infer, make)
        .unwrap_or_else(|error| panic!("the trait solver's cache: {error}"))
}

fn chunk_of(raw: u32) -> (usize, usize) {
    let biased = raw as usize / FIRST + 1;
    let chunk = (usize::BITS - 1 - biased.leading_zeros()) as usize;
    (chunk, raw as usize - FIRST * ((1 << chunk) - 1))
}

impl<T> Chunked<T> {
    const fn new() -> Self {
        Chunked { chunks: [const { OnceLock::new() }; CHUNKS] }
    }

    /// `None` when no entry of `raw`'s chunk was made yet.
    fn get(&self, raw: u32) -> Option<&T> {
        let (chunk, at) = chunk_of(raw);
        Some(&self.chunks[chunk].get()?[at])
    }

    /// Makes `raw`'s chunk, every entry `empty()`, if it has none.
    fn get_or_init(&self, raw: u32, empty: impl Fn() -> T) -> &T {
        let (chunk, at) = chunk_of(raw);
        &self.chunks[chunk].get_or_init(|| table(FIRST << chunk, |_| empty()))[at]
    }
}

/// How many indices a writer claims at a time, so writers under different locks share no
/// counter but once a block.
const BLOCK: u32 = 64;

/// The indices `next..end` a writer has claimed and not yet handed out.
struct Claimed {
    next: u32,
    end: u32,
}

impl Claimed {
    const EMPTY: Claimed = Claimed { next: 0, end: 0 };

    /// An index no one else is handed, claiming a block from `counter` when this one is used.
    fn take(&mut self, counter: &AtomicU64) -> u32 {
        if self.next == self.end {
            let start = counter.fetch_add(BLOCK as u64, Ordering::Relaxed);
            assert!(
                start + BLOCK as u64 <= u32::MAX as u64,
                "the solver's cache holds `u32::MAX` values of one kind"
            );
            *self = Claimed { next: start as u32, end: start as u32 + BLOCK };
        }
        let raw = self.next;
        self.next += 1;
        raw
    }
}

/// The indices a writer has claimed of each column it appends to.
struct Claims {
    rows: Claimed,
    nested_goals: Claimed,
}

/// One cache line pair each, so writers under neighbouring locks don't share a line.
#[repr(align(128))]
struct Writer(SpinLock<Claims>);

impl Writer {
    fn new() -> Writer {
        Writer(SpinLock::new(Claims { rows: Claimed::EMPTY, nested_goals: Claimed::EMPTY }))
    }
}

const _: () = assert!(size_of::<Writer>() == 128);

/// A power of two at least 4 times the cores, at least 4, so writers rarely meet on one lock.
fn shard_count() -> usize {
    static COUNT: OnceLock<usize> = OnceLock::new();
    *COUNT.get_or_init(|| {
        let cores = std::thread::available_parallelism().map_or(1, |it| it.get());
        (cores * 4).next_power_of_two().max(4)
    })
}

/// Table `k` of a shard has `TABLE << k` places: 28 hold more than a `u32` of entries.
const TABLE: usize = 16;
const TABLES: usize = 28;

/// Values interned as `u32` indices, as `blaze_ids::Sharded` interns by content ID: values in
/// chunks that never move, each with its hash, found by the hash in one of `shard_count`
/// open-addressed tables of `u32` slots (an index plus one, 0 for none), at most half full.
///
/// `find` and `get` take no lock. `intern` locks one shard, by hash, only to look again and
/// insert. A shard's table grows by copying into a new level twice its size, published once
/// full; the levels it outgrew are kept, as a reader may be probing one, so they cost at most
/// twice the last. A value two threads intern at once while its shard grows may get two
/// indices; callers compare values, not indices.
struct Interned<T> {
    values: Chunked<OnceLock<(u64, T)>>,
    /// The first index no shard has claimed.
    next: AtomicU64,
    shards: OnceLock<Slice<Shard>>,
}

#[repr(align(128))]
struct Shard {
    /// Held only to look again and insert.
    lock: SpinLock<Claimed>,
    /// How many values the shard has, written under `lock`.
    len: AtomicU32,
    /// Which of `tables` is the one in use.
    level: AtomicUsize,
    tables: [OnceLock<Slice<AtomicU32>>; TABLES],
}

impl Shard {
    fn new() -> Shard {
        Shard {
            lock: SpinLock::new(Claimed::EMPTY),
            len: AtomicU32::new(0),
            level: AtomicUsize::new(0),
            tables: [const { OnceLock::new() }; TABLES],
        }
    }

    fn table(&self, level: usize) -> &[AtomicU32] {
        self.tables[level].get_or_init(|| table(TABLE << level, |_| AtomicU32::new(0)))
    }
}

/// Where a hash's probe starts; bits the shard was not picked by.
fn home(hash: u64) -> usize {
    (hash >> 20) as usize
}

impl<T: Hash + Eq> Interned<T> {
    const fn new() -> Self {
        Interned { values: Chunked::new(), next: AtomicU64::new(0), shards: OnceLock::new() }
    }

    fn get(&self, index: u32) -> &T {
        &self.slot(index).expect("an index this table did not hand out").1
    }

    fn find(&self, value: &T) -> Option<u32> {
        let hash = hash_of(value);
        let shard = self.shard(hash);
        self.probe(shard.table(shard.level.load(Ordering::Acquire)), hash, value).ok()
    }

    fn intern(&self, value: T) -> u32 {
        let hash = hash_of(&value);
        let shard = self.shard(hash);
        if let Ok(found) = self.probe(shard.table(shard.level.load(Ordering::Acquire)), hash, &value)
        {
            return found;
        }
        let mut claimed = shard.lock.lock();
        let mut level = shard.level.load(Ordering::Acquire);
        if let Ok(found) = self.probe(shard.table(level), hash, &value) {
            return found;
        }
        let len = shard.len.load(Ordering::Relaxed);
        if (len as usize + 1) * 2 > TABLE << level {
            level = self.grow(shard, level);
        }
        let raw = claimed.take(&self.next);
        if self.values.get_or_init(raw, OnceLock::new).set((hash, value)).is_err() {
            unreachable!("index {raw} handed out twice");
        }
        let table = shard.table(level);
        table[empty_place(table, hash)].store(raw + 1, Ordering::Release);
        shard.len.store(len + 1, Ordering::Relaxed);
        raw
    }

    fn slot(&self, raw: u32) -> Option<&(u64, T)> {
        self.values.get(raw)?.get()
    }

    fn shard(&self, hash: u64) -> &Shard {
        let shards = self.shards.get_or_init(|| table(shard_count(), |_| Shard::new()));
        &shards[(hash >> 48) as usize & (shards.len() - 1)]
    }

    fn probe(&self, table: &[AtomicU32], hash: u64, value: &T) -> Result<u32, usize> {
        let mask = table.len() - 1;
        let mut at = home(hash) & mask;
        loop {
            match table[at].load(Ordering::Acquire) {
                0 => return Err(at),
                plus_one => {
                    let raw = plus_one - 1;
                    let (stored, found) = self.slot(raw).expect("a table entry before its value");
                    if *stored == hash && found == value {
                        return Ok(raw);
                    }
                }
            }
            at = (at + 1) & mask;
        }
    }

    /// Copies a full shard's entries into a level twice its size and makes it the one in use.
    /// Called with the shard locked.
    fn grow(&self, shard: &Shard, level: usize) -> usize {
        assert!(level + 1 < TABLES, "a shard of the solver's cache of more than 2^30 values");
        let old = shard.table(level);
        let new = shard.table(level + 1);
        for entry in old {
            let plus_one = entry.load(Ordering::Relaxed);
            if plus_one == 0 {
                continue;
            }
            let (hash, _) = self.slot(plus_one - 1).expect("a table entry before its value");
            new[empty_place(new, *hash)].store(plus_one, Ordering::Relaxed);
        }
        shard.level.store(level + 1, Ordering::Release);
        level + 1
    }
}

/// The first empty place of `hash`'s probe; only a shard's lock holder writes its tables.
fn empty_place(table: &[AtomicU32], hash: u64) -> usize {
    let mask = table.len() - 1;
    let mut at = home(hash) & mask;
    while table[at].load(Ordering::Relaxed) != 0 {
        at = (at + 1) & mask;
    }
    at
}

/// FxHash of `value`, multiplied so the high bits, which pick the shard, and the bits the probe
/// starts at both spread.
fn hash_of<T: Hash>(value: &T) -> u64 {
    FxBuildHasher.hash_one(value).wrapping_mul(0x9E37_79B9_7F4A_7C15)
}

#[cfg(test)]
mod tests {
    use std::sync::Barrier;

    use super::{Chunked, Interned, chunk_of};

    #[test]
    fn a_value_interned_twice_has_one_index() {
        let interned = Interned::<u64>::new();
        let indices: Vec<u32> = (0..5000u64).map(|it| interned.intern(it * 7919)).collect();
        for (it, &index) in indices.iter().enumerate() {
            assert_eq!(interned.intern(it as u64 * 7919), index);
            assert_eq!(interned.find(&(it as u64 * 7919)), Some(index));
            assert_eq!(*interned.get(index), it as u64 * 7919);
        }
        let mut sorted = indices.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), indices.len());
        assert_eq!(interned.find(&1), None);
    }

    /// A table made fresh, filled through growth and dropped, after one like it, allocates
    /// nothing: its chunks, shards and probe tables are recycled blocks and its locks never
    /// allocate.
    #[test]
    fn a_fresh_table_after_a_dropped_one_allocates_nothing() {
        let fill = || {
            let interned = Interned::<u64>::new();
            for it in 0..3000u64 {
                interned.intern(it * 31);
            }
            assert_eq!(interned.find(&(2999 * 31)).map(|index| *interned.get(index)), Some(2999 * 31));
        };
        fill();
        blaze_alloc_count::assert_at_most(0, "a fresh solver cache table", fill);
    }

    #[test]
    fn an_empty_table_finds_nothing() {
        assert_eq!(Interned::<u64>::new().find(&0), None);
        assert!(Chunked::<u32>::new().get(0).is_none());
    }

    #[test]
    fn chunks_cover_every_index_once() {
        assert_eq!(chunk_of(0), (0, 0));
        assert_eq!(chunk_of(63), (0, 63));
        assert_eq!(chunk_of(64), (1, 0));
        assert_eq!(chunk_of(191), (1, 127));
        assert_eq!(chunk_of(192), (2, 0));
        let (last, at) = chunk_of(u32::MAX);
        assert!(last < super::CHUNKS);
        assert!(at < super::FIRST << last);
    }

    /// Threads interning overlapping values at once, through growth, each find every value
    /// at an index whose value is it.
    #[test]
    fn threads_interning_at_once_agree_on_values() {
        let interned = Interned::<u64>::new();
        let threads = 8;
        let barrier = Barrier::new(threads);
        std::thread::scope(|scope| {
            for thread in 0..threads as u64 {
                let (interned, barrier) = (&interned, &barrier);
                scope.spawn(move || {
                    barrier.wait();
                    for it in 0..4000u64 {
                        let value = (it + thread * 500) % 6000;
                        let index = interned.intern(value);
                        assert_eq!(*interned.get(index), value);
                    }
                });
            }
        });
        for value in 0..6000u64 {
            let index = interned.find(&value).expect("interned by some thread");
            assert_eq!(*interned.get(index), value);
        }
    }
}
