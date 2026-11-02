//! The database's tables (docs/v2/plan/design/concurrency.md): columns of rows by `u32` index
//! that never move, and maps on [`QueryKey`]s over them that a lookup reads with acquire loads
//! alone.
//!
//! - A [`Rows`] is storage by row index in one reserved region ([`blaze_mem::Region`]): row
//!   `i` is at `base + i × size`, committed as it is written, so a row's address never changes. [`Ids`] hands out row indices from blocks a
//!   [`Free`] claims, and a [`Free`] keeps the rows its owner retired until their grace has
//!   passed, then readies them for reuse.
//! - A [`Map`] is [`blaze_ids::shard_count`] shards, each one open-addressed table of `u64`
//!   places: a 32-bit tag of the key and its entry's row + 1. A writer writes the row, then
//!   publishes the place with one release store; a reader probes by tag and reads the row only
//!   on a tag match, writing nothing.
//! - A changed value is a new row swapped into the place; a removed one leaves a tombstone. The
//!   row let go of is retired to the shard's [`Free`] and reused only after its grace. A table
//!   that fills is copied into a new one from the tags alone, published with one release store,
//!   never rehashed in place.
//! - A reader proves it is in a task with a [`Guard`] borrowed for as long as it holds what it
//!   read.

use std::{collections::VecDeque, marker::PhantomData, ptr};

use blaze_ids::{ContentId, Fingerprint};
use blaze_mem::{Region, Tag};
use blaze_reclaim::{Grace, Guard};

#[allow(clippy::disallowed_types)]
use crate::sync::{Mutex, MutexGuard};
use crate::{
    Edge, QueryKey, QueryKind,
    sync::{
        OnceLock,
        atomic::{AtomicPtr, AtomicU32, AtomicU64, Ordering},
    },
};

/// A row's live flag and its generation are `std`'s even under `blaze_check`: they are laid
/// out at one byte and four bytes a row in their regions, where a model atomic does not fit,
/// and each is written by its row's one claimer and read only after the row was published
/// to the reader by the atomics the model sees.
type PlainFlag = std::sync::atomic::AtomicBool;
type PlainWord = std::sync::atomic::AtomicU32;

pub(crate) type FxMap<K, V> = std::collections::HashMap<K, V, blaze_hash::FixedState>; // alloc: blaze_hash's hasher, not SipHash
pub(crate) type FxSet<K> = std::collections::HashSet<K, blaze_hash::FixedState>; // alloc: blaze_hash's hasher, not SipHash

/// A lock held for a few loads and stores: claiming or swapping a slot, copying a table to
/// grow it, claiming rows, holding what was unlinked. Nothing that holds it calls a query,
/// the tree or another lock, so it is never held across a call into the database.
pub(crate) struct ShortLock<T> {
    #[allow(clippy::disallowed_types)]
    inner: Mutex<T>,
}

#[allow(clippy::disallowed_types)]
impl<T> ShortLock<T> {
    pub(crate) const fn new(value: T) -> ShortLock<T> {
        ShortLock { inner: Mutex::new(value) }
    }

    pub(crate) fn lock(&self) -> MutexGuard<'_, T> {
        self.inner.lock()
    }

    pub(crate) fn get_mut(&mut self) -> &mut T {
        self.inner.get_mut()
    }
}

impl<T: Default> Default for ShortLock<T> {
    fn default() -> ShortLock<T> {
        let lock = ShortLock::new(T::default());
        // alloc: `std`'s mutex is a pthread one on macOS, boxed at its first lock: taken here,
        // where the owner is made, so a new view's first insert in a shard allocates nothing.
        #[cfg(not(blaze_check))]
        drop(lock.lock());
        lock
    }
}

/// The address space one [`Rows`] reserves: 2^30 rows of 64 bytes. Reserving takes no
/// memory; pages are committed as rows are written.
const ROWS_RESERVED: usize = 64 << 30;

/// How many rows a [`Free`] claims at a time: a line of live flags, so writers claiming in
/// different shards rarely write one line.
const BLOCK: u32 = 64;

/// Rows are below this: a place keeps `row + 1` in 32 bits, beside [`TOMB`].
const ROW_END: u32 = u32::MAX - 2 * BLOCK;

/// Asks the core to start loading the line at `at` into L1. A hint only: it reads nothing a
/// load can see, never faults, and on other targets does nothing.
#[inline(always)]
pub(crate) fn prefetch<T>(at: *const T) {
    #[cfg(target_arch = "x86_64")]
    // SAFETY: `prefetcht0` touches no register but its address and no memory, and never
    // faults, whatever the address.
    unsafe {
        std::arch::asm!("prefetcht0 [{at}]", at = in(reg) at, options(nostack, preserves_flags, readonly));
    }
    #[cfg(target_arch = "aarch64")]
    // SAFETY: as above, for `prfm`.
    unsafe {
        std::arch::asm!("prfm pldl1keep, [{at}]", at = in(reg) at, options(nostack, preserves_flags, readonly));
    }
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    let _ = at;
}

/// The query database's reservation of `bytes`, on huge pages if `huge`; stops naming what it
/// was for when the kernel refuses (a database without its tables can do nothing).
fn reserve(bytes: usize, huge: bool, what: &str) -> Region {
    let region = if huge { Region::reserve_huge(bytes, Tag::Query) } else { Region::reserve(bytes, Tag::Query) };
    region.unwrap_or_else(|error| panic!("the query database could not reserve its {what}: {error}"))
}

/// Commits `region` through `len` bytes; stops naming what it was for when it cannot.
#[inline]
fn commit(region: &Region, len: usize, what: &str) {
    if let Err(error) = region.ensure(len) {
        panic!("the query database could not grow its {what}: {error}")
    }
}

/// Rows of `T` by `u32` index in one reserved region: row `i` is at `base + i × size` for the
/// region's life, so finding it is a multiply and an add, with no table of chunks to load
/// first. Pages are committed as rows are written, and committed memory is zeroed, so a row
/// never written reads as not live. Which rows hold a value is the owner's to know ([`Ids`],
/// [`Free`]); a row is written once by the one thread that claimed it, read by any thread it
/// was published to, and dropped once no thread can reach it.
///
/// Per row: `size_of::<T>()` bytes (line-aligned when a multiple of 64, as the region's base
/// is a page) and one byte of live flag. A read: the row itself. Writing one: a plain store of
/// the row and of its flag, no read-modify-write, and a load of the committed length.
pub(crate) struct Rows<T> {
    rows: Region,
    /// One flag a row, set while it holds a value: what [`Rows`]' drop drops. Written only by
    /// the row's owner, with a plain store.
    live: Region,
    /// Rows below this fit the reservation.
    max: usize,
    _owns: PhantomData<T>,
}

// SAFETY: a row is written by the one thread that claimed it before it is published, read only
// after an acquire load of what published it, and dropped only once no thread can reach it
// (the callers of `put`, `get` and `drop_row` uphold this); so `Rows` hands `&T` across
// threads as a `Sync` `T` allows, and moves `T`s between threads as a `Send` `T` allows.
unsafe impl<T: Send> Send for Rows<T> {}
// SAFETY: as above.
unsafe impl<T: Send + Sync> Sync for Rows<T> {}

impl<T> Rows<T> {
    const SIZE: usize = {
        assert!(size_of::<T>() > 0, "rows of a zero-sized type");
        assert!(align_of::<T>() <= 4096, "a row aligned past a page");
        size_of::<T>()
    };

    /// As many rows as fit [`ROWS_RESERVED`], and below [`ROW_END`].
    const FIT: usize = {
        let fit = ROWS_RESERVED / Self::SIZE;
        if fit < ROW_END as usize { fit } else { ROW_END as usize }
    };

    /// Rows on base pages: for a view's maps and the database's smaller tables.
    pub(crate) fn new() -> Rows<T> {
        Rows::with(false)
    }

    /// Rows on huge pages: for the tables every hit reads (memo lines, slots, results).
    pub(crate) fn hot() -> Rows<T> {
        Rows::with(true)
    }

    /// Rows `0..max` on base pages: for a table indexed by a small number.
    pub(crate) fn sized(max: usize) -> Rows<T> {
        Rows::reserved(max.min(Self::FIT), false)
    }

    fn with(huge: bool) -> Rows<T> {
        Rows::reserved(Self::FIT, huge)
    }

    fn reserved(max: usize, huge: bool) -> Rows<T> {
        Rows {
            rows: reserve(max * Self::SIZE, huge, "rows"),
            live: reserve(max, false, "rows' live flags"),
            max,
            _owns: PhantomData,
        }
    }

    /// Where `row` is, without reading it: for [`prefetch`], which never faults.
    fn address(&self, row: u32) -> *const T {
        self.rows.at(row as usize * Self::SIZE).as_ptr().cast_const().cast()
    }

    fn row(&self, row: u32) -> *mut T {
        debug_assert!((row as usize + 1) * Self::SIZE <= self.rows.committed(), "a row never written");
        self.rows.at(row as usize * Self::SIZE).as_ptr().cast()
    }

    fn live(&self, row: u32) -> &PlainFlag {
        debug_assert!((row as usize) < self.live.committed(), "a flag never written");
        // SAFETY: the flag's byte is committed (written by `put` before any reader or dropper
        // reaches the row, or checked by the drop's bound), and committed memory is zeroed, a
        // valid `AtomicBool`; it lives as long as `self`.
        unsafe { self.live.at(row as usize).cast::<PlainFlag>().as_ref() }
    }

    /// Writes `value` into `row`.
    ///
    /// # Safety
    ///
    /// The caller owns `row`: it claimed it from [`Ids::claim`] and has not published it, so
    /// no other thread reads or writes it, and it holds no value.
    pub(crate) unsafe fn put(&self, row: u32, value: T) {
        let at = row as usize;
        assert!(at < self.max, "row {at} past the {} a table reserves", self.max);
        commit(&self.rows, (at + 1) * Self::SIZE, "rows");
        commit(&self.live, at + 1, "rows' live flags");
        // SAFETY: committed just now; the caller owns the row, which holds no value.
        unsafe { self.row(row).write(value) };
        self.live(row).store(true, Ordering::Relaxed);
    }

    /// The value at `row`.
    ///
    /// # Safety
    ///
    /// `row` holds a value published to this thread (an acquire load of what a release store
    /// published after [`Rows::put`]), which is not dropped while the borrow lives: the
    /// caller is in a task that began before any grace that would let it go, or it owns
    /// the row.
    pub(crate) unsafe fn get(&self, row: u32) -> &T {
        // SAFETY: the caller's contract: the row holds a published value that outlives this.
        unsafe { &*self.row(row) }
    }

    /// The value at `row` if one was written there: for a table whose rows are written once
    /// and dropped only with it, read at numbers nobody claimed.
    ///
    /// # Safety
    ///
    /// A value written at `row` was published to this thread and is not dropped while the
    /// borrow lives.
    #[inline]
    pub(crate) unsafe fn written(&self, row: u32) -> Option<&T> {
        if row as usize >= self.live.committed() || !self.live(row).load(Ordering::Relaxed) {
            return None;
        }
        // SAFETY: live, so `put` committed and wrote the row; the caller's contract keeps it.
        Some(unsafe { &*self.row(row) })
    }

    /// The value at `row`, to change.
    ///
    /// # Safety
    ///
    /// `row` holds a value.
    pub(crate) unsafe fn get_mut(&mut self, row: u32) -> &mut T {
        // SAFETY: the caller's contract; `&mut self` excludes every reader.
        unsafe { &mut *self.row(row) }
    }

    /// Drops the value at `row`.
    ///
    /// # Safety
    ///
    /// `row` holds a value that no thread can reach any more.
    pub(crate) unsafe fn drop_row(&self, row: u32) {
        self.live(row).store(false, Ordering::Relaxed);
        // SAFETY: the caller's contract: nothing reads the value any more.
        unsafe { ptr::drop_in_place(self.row(row)) };
    }
}

impl<T> Drop for Rows<T> {
    fn drop(&mut self) {
        if !std::mem::needs_drop::<T>() {
            return;
        }
        let written = (self.rows.committed() / Self::SIZE).min(self.live.committed());
        for row in 0..written as u32 {
            if self.live(row).load(Ordering::Relaxed) {
                // SAFETY: a live flag is set by `put` and cleared by `drop_row`, so the row
                // holds a value; `&mut` excludes every reader.
                unsafe { ptr::drop_in_place(self.row(row)) };
            }
        }
    }
}

/// A `u32` a row that outlives what the row holds: its generation, bumped each time the row is
/// claimed, so a row and its generation name one value however often the row is reused (until
/// one row is reused 2^32 times while a word naming it is kept). 4 bytes a row, in one
/// reserved region (zeroed as committed: generation 0).
pub(crate) struct Generations {
    words: Region,
}

impl Generations {
    pub(crate) fn new() -> Generations {
        Generations { words: reserve(ROW_END as usize * size_of::<PlainWord>(), false, "generations") }
    }

    /// The row's next generation: called once each time the row is claimed, by its claimer,
    /// which alone writes it (so a load and a store, no read-modify-write).
    pub(crate) fn bump(&self, row: u32) -> u32 {
        let at = row as usize * size_of::<PlainWord>();
        commit(&self.words, at + size_of::<PlainWord>(), "generations");
        // SAFETY: committed just now, aligned (the base is a page and `at` a multiple of 4),
        // and zeroed memory is a valid `AtomicU32`; it lives as long as `self`.
        let word = unsafe { self.words.at(at).cast::<PlainWord>().as_ref() };
        let next = word.load(Ordering::Relaxed).wrapping_add(1);
        word.store(next, Ordering::Relaxed);
        next
    }
}

/// The row indices of one column: handed out in blocks of [`BLOCK`] to each [`Free`], so
/// writers in different shards share no counter.
pub(crate) struct Ids {
    next: AtomicU32,
}

impl Ids {
    pub(crate) fn new() -> Ids {
        Ids { next: AtomicU32::new(0) }
    }

    /// A row for the caller alone: one its [`Free`] readied, or the next of its block.
    pub(crate) fn claim(&self, free: &mut Free) -> u32 {
        if let Some(row) = free.ready.pop() {
            return row;
        }
        if free.next == free.end {
            let start = self.next.fetch_add(BLOCK, Ordering::Relaxed);
            assert!(start < ROW_END, "a column of 2^32 rows");
            free.next = start;
            free.end = start + BLOCK;
        }
        let row = free.next;
        free.next += 1;
        row
    }

    /// Above every row handed out so far.
    pub(crate) fn end(&self) -> u32 {
        self.next.load(Ordering::Acquire)
    }
}

/// A writer's rows: the rest of its claimed block, rows ready for reuse, and rows it retired
/// that a reader may still hold, each with the grace begun after it was unlinked.
#[derive(Default)]
pub(crate) struct Free {
    next: u32,
    end: u32,
    ready: Vec<u32>,
    retired: VecDeque<(Grace, u32)>,
}

impl Free {
    /// Holds `row`, unlinked before `grace` started, until `grace` has passed.
    pub(crate) fn retire(&mut self, grace: Grace, row: u32) {
        self.retired.push_back((grace, row));
    }

    /// Readies a row no thread can reach.
    pub(crate) fn ready(&mut self, row: u32) {
        self.ready.push(row);
    }

    /// Hands each retired row whose grace has passed to `drop` (which drops what it holds),
    /// then readies it. Graces are started by whoever holds this (a shard's lock holder, or a
    /// lane's one thread), so they come in order: when the newest has passed every one has,
    /// and otherwise the oldest are checked
    /// until one has not.
    pub(crate) fn reclaim(&mut self, mut drop: impl FnMut(u32)) -> usize {
        let Some(&(newest, _)) = self.retired.back() else { return 0 };
        let before = self.retired.len();
        if newest.passed() {
            for (_, row) in self.retired.drain(..) {
                drop(row);
                self.ready.push(row);
            }
            return before;
        }
        while let Some(&(grace, row)) = self.retired.front() {
            if !grace.passed() {
                break;
            }
            self.retired.pop_front();
            drop(row);
            self.ready.push(row);
        }
        before - self.retired.len()
    }

    /// [`Free::reclaim`] of every retired row, passed or not: for an owner borrowed `&mut`,
    /// which no reader can be in.
    pub(crate) fn reclaim_all(&mut self, mut drop: impl FnMut(u32)) {
        for (_, row) in self.retired.drain(..) {
            drop(row);
            self.ready.push(row);
        }
    }

    pub(crate) fn retired(&self) -> usize {
        self.retired.len()
    }
}

/// How many rows a shard has retired before a write reclaims those past their grace.
pub(crate) const COLLECT_AT: usize = 16;
const FIRST_TABLE: usize = 16;

/// One key and its value, never changed once published.
pub(crate) struct Entry<V> {
    pub(crate) key: QueryKey,
    pub(crate) value: V,
}

/// An empty place.
const EMPTY: u64 = 0;
/// The low half of a removed entry's place.
const TOMB: u32 = u32::MAX;

fn place(tag: u32, row: u32) -> u64 {
    u64::from(tag) << 32 | u64::from(row + 1)
}

fn row_of(place: u64) -> Option<u32> {
    match place as u32 {
        0 | TOMB => None,
        low => Some(low - 1),
    }
}

/// Bits 64..96 of the key's input, mixed with its kind: where its probe starts (masked to
/// the table) and the tag its place keeps. Kinds of one input share a shard, which is picked
/// by bits 0..32, so the two are independent. The input is a content hash already: nothing is
/// hashed again.
fn tag(key: &QueryKey) -> u32 {
    let input = &key.input.0;
    u32::from_le_bytes([input[8], input[9], input[10], input[11]]) ^ u32::from(key.kind.0).wrapping_mul(0x9e37_79b9)
}

/// A shard's table is its places alone, 8 bytes each, at most half full with live entries and
/// tombs; the top bits of the pointer published in the shard are log2 of its length, so a find
/// goes from that one load straight to the place (no header to load first): one acquire load
/// a place probed (about 1.5 at that load) and one row read on a tag match.
const LEN_SHIFT: u32 = usize::BITS - 6;

const _: () = assert!(usize::BITS == 64, "a table's length is kept in its pointer's top bits");

fn new_table(len: usize) -> *mut AtomicU64 {
    debug_assert!(len.is_power_of_two());
    // alloc: once a table: on a shard's first insert, and each time it doubles.
    let places: Box<[AtomicU64]> = (0..len).map(|_| AtomicU64::new(EMPTY)).collect();
    let raw = Box::into_raw(places).cast::<AtomicU64>();
    assert!(raw.addr() >> LEN_SHIFT == 0, "a table mapped above 2^58");
    raw.map_addr(|addr| addr | (len.trailing_zeros() as usize) << LEN_SHIFT)
}

fn untag(table: *mut AtomicU64) -> (*mut AtomicU64, usize) {
    (table.map_addr(|addr| addr & ((1 << LEN_SHIFT) - 1)), 1 << (table.addr() >> LEN_SHIFT))
}

/// The places of a table [`new_table`] made.
///
/// # Safety
///
/// `table` is not freed while `'a` lives.
unsafe fn places<'a>(table: *mut AtomicU64) -> &'a [AtomicU64] {
    let (at, len) = untag(table);
    // SAFETY: `new_table` allocated `len` places at `at`; alive by the caller's contract.
    unsafe { std::slice::from_raw_parts(at, len) }
}

/// The table back as the box [`new_table`] made it from.
///
/// # Safety
///
/// `table` came from [`new_table`], is unlinked, and is taken back once.
unsafe fn table_box(table: *mut AtomicU64) -> Box<[AtomicU64]> {
    let (at, len) = untag(table);
    // SAFETY: the box `new_table` leaked, of `len` places at `at`.
    unsafe { Box::from_raw(ptr::slice_from_raw_parts_mut(at, len)) }
}

/// A table [`new_table`] made, owned: freed when dropped.
struct Table(*mut AtomicU64);

// SAFETY: a table is its places alone, atomics any thread may touch; the owner frees it once.
unsafe impl Send for Table {}

impl Drop for Table {
    fn drop(&mut self) {
        // SAFETY: from `new_table`, unlinked (an outgrown table) or never published, and owned
        // here alone.
        drop(unsafe { table_box(self.0) });
    }
}

/// Tables a shard outgrew, each held until no reader can be probing it, then kept as the
/// shard's next table of its length: a shard whose entries are removed and put back rehashes
/// into it and allocates nothing.
struct Outgrown {
    /// Oldest first: their graces start under the shard's lock, in order.
    held: VecDeque<(Grace, Table)>,
    /// One past its grace for each length, at log2 of it.
    ready: [Option<Table>; 64],
}

impl Default for Outgrown {
    fn default() -> Outgrown {
        Outgrown { held: VecDeque::new(), ready: [const { None }; 64] }
    }
}

impl Outgrown {
    fn hold(&mut self, grace: Grace, table: Table) {
        self.held.push_back((grace, table));
    }

    /// Keeps each table whose grace has passed as the one of its length (freeing the one it
    /// displaces).
    fn sweep(&mut self) {
        while self.held.front().is_some_and(|(grace, _)| grace.passed()) {
            let Some((_, table)) = self.held.pop_front() else { break };
            let (_, len) = untag(table.0);
            self.ready[len.trailing_zeros() as usize] = Some(table);
        }
    }

    /// Keeps every held table as the one of its length, graces or not: for an owner borrowed
    /// `&mut`, which no reader can be in.
    fn settle(&mut self) {
        while let Some((_, table)) = self.held.pop_front() {
            let (_, len) = untag(table.0);
            self.ready[len.trailing_zeros() as usize] = Some(table);
        }
    }

    /// An empty table of `len` places, if one is kept.
    fn take(&mut self, len: usize) -> Option<*mut AtomicU64> {
        self.sweep();
        let table = self.ready[len.trailing_zeros() as usize].take()?;
        let raw = std::mem::ManuallyDrop::new(table).0;
        // SAFETY: kept past its grace: no reader probes it, and this shard's lock holder owns it.
        for place in unsafe { places(raw) } {
            place.store(EMPTY, Ordering::Relaxed);
        }
        Some(raw)
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.held.len()
    }
}

/// A shard's writer state, under its lock.
#[derive(Default)]
struct Writer {
    live: usize,
    /// Live entries and tombs: what fills the table.
    used: usize,
    free: Free,
    /// Tables outgrown: held until no reader can be probing them, then kept by length.
    outgrown: Outgrown,
}

/// One cache line pair per shard, so writers to neighbouring shards don't share a line.
#[repr(align(128))]
struct Shard {
    writer: ShortLock<Writer>,
    /// Null until the first insert; see [`new_table`].
    table: AtomicPtr<AtomicU64>,
}

/// What [`Map::edit_all`] does to one entry.
pub(crate) enum Edit<V> {
    Keep,
    Remove,
    Replace(V),
}

/// A map on query keys whose lookups take no lock and write nothing shared.
///
/// Per entry: its row (`size_of::<Entry<V>>()`, the 34-byte key and the value) plus at most
/// 16 bytes of places and a live bit. A lookup: the shard's table pointer, about 1.5 place
/// loads, and one row read, with no hash computed (the key is one).
pub(crate) struct Map<V> {
    shards: Box<[Shard]>,
    ids: Ids,
    rows: Rows<Entry<V>>,
    mask: usize,
}

impl<V: Send + Sync + 'static> Map<V> {
    /// A map whose entries are on base pages.
    pub(crate) fn new() -> Map<V> {
        Map::with(Rows::new())
    }

    /// A map whose entries are on huge pages: for the maps every hit reads.
    pub(crate) fn hot() -> Map<V> {
        Map::with(Rows::hot())
    }

    fn with(rows: Rows<Entry<V>>) -> Map<V> {
        let count = blaze_ids::shard_count();
        Map {
            shards: (0..count)
                .map(|_| Shard { writer: ShortLock::default(), table: AtomicPtr::new(ptr::null_mut()) })
                .collect(), // alloc: once a map
            ids: Ids::new(),
            rows,
            mask: count - 1,
        }
    }

    fn shard(&self, key: &QueryKey) -> &Shard {
        let input = &key.input.0;
        &self.shards[u32::from_le_bytes([input[0], input[1], input[2], input[3]]) as usize & self.mask]
    }

    fn table<'g>(shard: &'g Shard, _task: &'g Guard) -> Option<&'g [AtomicU64]> {
        let table = shard.table.load(Ordering::Acquire);
        // SAFETY: a table published in a shard is freed only after a newer one replaced it and
        // a grace passed, or by `&mut`: `_task` keeps this thread in the task that loaded it.
        (!table.is_null()).then(|| unsafe { places(table) })
    }

    /// The entry a place names.
    ///
    /// # Safety
    ///
    /// `place` was loaded from a table of this map in the caller's task, or under the shard's
    /// lock, and names a row: the row is retired only after the place is overwritten and
    /// dropped only after a grace begun then, which the caller's task (or the lock) holds back.
    unsafe fn entry_at(&self, place: u64) -> Option<&Entry<V>> {
        // SAFETY: the caller's contract.
        row_of(place).map(|row| unsafe { self.rows.get(row) })
    }

    /// Where `key` is: its place's index and word, and its entry.
    fn find<'g>(&'g self, table: &'g [AtomicU64], key: &QueryKey) -> Option<(usize, u64, &'g Entry<V>)> {
        let tag = tag(key);
        let mask = table.len() - 1;
        let mut at = tag as usize & mask;
        loop {
            let place = table[at].load(Ordering::Acquire);
            if place == EMPTY {
                return None;
            }
            if (place >> 32) as u32 == tag {
                // SAFETY: loaded from a table of this map in the caller's task or under the lock.
                if let Some(entry) = unsafe { self.entry_at(place) }
                    && entry.key == *key
                {
                    return Some((at, place, entry));
                }
            }
            at = (at + 1) & mask;
        }
    }

    /// Where `key` is, or the first place it may be put: a tomb or an empty one.
    fn place_for(&self, table: &[AtomicU64], key: &QueryKey) -> Result<(usize, u64), usize> {
        let tag = tag(key);
        let mask = table.len() - 1;
        let mut at = tag as usize & mask;
        let mut free = None;
        loop {
            let place = table[at].load(Ordering::Relaxed);
            if place == EMPTY {
                return Err(free.unwrap_or(at));
            }
            if place as u32 == TOMB {
                free.get_or_insert(at);
            } else if (place >> 32) as u32 == tag {
                // SAFETY: loaded under the shard's lock, which only its holder changes.
                if unsafe { self.entry_at(place) }.is_some_and(|entry| entry.key == *key) {
                    return Ok((at, place));
                }
            }
            at = (at + 1) & mask;
        }
    }

    fn get_entry<'g>(&'g self, task: &'g Guard, key: &QueryKey) -> Option<(u64, &'g Entry<V>)> {
        let (_, place, entry) = self.find(Self::table(self.shard(key), task)?, key)?;
        Some((place, entry))
    }

    pub(crate) fn get<'g>(&'g self, task: &'g Guard, key: &QueryKey) -> Option<&'g V> {
        self.get_entry(task, key).map(|(_, entry)| &entry.value)
    }

    /// Starts loading the line of `key`'s first place, for a lookup of it soon: a hint.
    pub(crate) fn prefetch_place(&self, task: &Guard, key: &QueryKey) {
        if let Some(table) = Self::table(self.shard(key), task) {
            prefetch(&table[tag(key) as usize & (table.len() - 1)]);
        }
    }

    /// Starts loading the row `key`'s first place names: a hint, for once the place's line
    /// is in (see [`Map::prefetch_place`]).
    pub(crate) fn prefetch_entry(&self, task: &Guard, key: &QueryKey) {
        let Some(table) = Self::table(self.shard(key), task) else { return };
        let place = table[tag(key) as usize & (table.len() - 1)].load(Ordering::Relaxed);
        if let Some(at) = row_of(place).map(|row| self.rows.address(row)) {
            prefetch(at);
        }
    }

    /// The shard's table, with a place for one more entry.
    fn room<'s>(shard: &'s Shard, writer: &mut Writer) -> &'s [AtomicU64] {
        let current = shard.table.load(Ordering::Acquire);
        if current.is_null() {
            let table = writer.outgrown.take(FIRST_TABLE).unwrap_or_else(|| new_table(FIRST_TABLE));
            shard.table.store(table, Ordering::Release);
            // SAFETY: just published; freed only once outgrown and past its grace, or by `&mut`.
            return unsafe { places(table) };
        }
        // SAFETY: the current table, which only this lock's holder replaces.
        let old = unsafe { places(current) };
        if (writer.used + 1) * 2 <= old.len() {
            return old;
        }
        let len = ((writer.live + 1) * 4).next_power_of_two().max(FIRST_TABLE);
        let table = writer.outgrown.take(len).unwrap_or_else(|| new_table(len));
        // SAFETY: made just now and not yet published.
        let new = unsafe { places(table) };
        let mask = new.len() - 1;
        for place in old {
            let place = place.load(Ordering::Relaxed);
            if row_of(place).is_none() {
                continue;
            }
            let mut at = (place >> 32) as usize & mask;
            while new[at].load(Ordering::Relaxed) != EMPTY {
                at = (at + 1) & mask;
            }
            new[at].store(place, Ordering::Relaxed);
        }
        shard.table.store(table, Ordering::Release);
        writer.used = writer.live;
        // `current` is unlinked; its rows are named by the new table too, and a table owns only
        // its places.
        writer.outgrown.hold(Grace::start(), Table(current));
        new
    }

    /// Frees the shard's retired rows whose grace has passed, once enough are waiting.
    fn tidy(&self, writer: &mut Writer) {
        if writer.free.retired() >= COLLECT_AT {
            let rows = &self.rows;
            // SAFETY: a retired row's grace has passed: no task that could reach it is left.
            writer.free.reclaim(|row| unsafe { rows.drop_row(row) });
        }
    }

    /// Publishes `key`'s entry under the shard's lock, retiring the one it replaces.
    fn publish<'s>(&'s self, shard: &'s Shard, writer: &mut Writer, key: QueryKey, value: V) -> &'s Entry<V> {
        let table = Self::room(shard, writer);
        let row = self.ids.claim(&mut writer.free);
        // SAFETY: claimed just now, under the lock, and not yet published.
        unsafe { self.rows.put(row, Entry { key, value }) };
        let new = place(tag(&key), row);
        match self.place_for(table, &key) {
            Ok((at, old)) => {
                table[at].store(new, Ordering::Release);
                if let Some(old) = row_of(old) {
                    writer.free.retire(Grace::start(), old);
                }
            }
            Err(at) => {
                if table[at].load(Ordering::Relaxed) == EMPTY {
                    writer.used += 1;
                }
                writer.live += 1;
                table[at].store(new, Ordering::Release);
            }
        }
        self.tidy(writer);
        // SAFETY: published in the current table; dropped only after it is retired and a grace
        // has passed.
        unsafe { self.rows.get(row) }
    }

    /// Sets `key`'s value; a reader still holding the old one keeps it until its task ends.
    pub(crate) fn insert<'g>(&'g self, _task: &'g Guard, key: QueryKey, value: V) -> &'g V {
        let shard = self.shard(&key);
        let mut writer = shard.writer.lock();
        &self.publish(shard, &mut writer, key, value).value
    }

    /// `key`'s value, or `make()`'s, made before the shard is locked: a racing writer's wins
    /// and `make()`'s is dropped.
    pub(crate) fn get_or_insert_with<'g>(
        &'g self,
        task: &'g Guard,
        key: QueryKey,
        make: impl FnOnce() -> V,
    ) -> &'g V {
        if let Some(found) = self.get(task, &key) {
            return found;
        }
        let value = make();
        let shard = self.shard(&key);
        let mut writer = shard.writer.lock();
        if let Some(found) = self.get(task, &key) {
            return found;
        }
        &self.publish(shard, &mut writer, key, value).value
    }

    /// Replaces `key`'s value with what `change` makes of it (`None`: leave it), made without
    /// the lock and made again if another writer changed the entry meanwhile. A place names
    /// one row, which is not reused while this task lives, so an equal place is the same entry.
    pub(crate) fn update(&self, task: &Guard, key: QueryKey, mut change: impl FnMut(Option<&V>) -> Option<V>) {
        let shard = self.shard(&key);
        loop {
            let seen = self.get_entry(task, &key);
            let Some(value) = change(seen.map(|(_, entry)| &entry.value)) else { return };
            let mut writer = shard.writer.lock();
            let now = self.get_entry(task, &key).map(|(place, _)| place);
            if now == seen.map(|(place, _)| place) {
                self.publish(shard, &mut writer, key, value);
                return;
            }
        }
    }

    /// Every entry, as of when each shard's table is loaded; one published meanwhile may be
    /// missed.
    pub(crate) fn for_each<'g>(&'g self, task: &'g Guard, mut f: impl FnMut(&'g QueryKey, &'g V)) {
        for shard in 0..self.shards.len() {
            self.for_each_in(task, shard, &mut f);
        }
    }

    /// How many shards it has: [`Map::for_each_in`] takes one, to split a pass among workers.
    pub(crate) fn shard_count(&self) -> usize {
        self.shards.len()
    }

    /// [`Map::for_each`] of the entries of shard `shard` (none past the last).
    pub(crate) fn for_each_in<'g>(&'g self, task: &'g Guard, shard: usize, mut f: impl FnMut(&'g QueryKey, &'g V)) {
        let Some(table) = self.shards.get(shard).and_then(|shard| Self::table(shard, task)) else { return };
        for place in table.iter() {
            // SAFETY: loaded from a table of this map in the caller's task.
            if let Some(entry) = unsafe { self.entry_at(place.load(Ordering::Acquire)) } {
                f(&entry.key, &entry.value);
            }
        }
    }

    /// Keeps, removes or replaces each entry as `edit` says, decided without the lock; an entry
    /// another writer changed meanwhile is left as it is. `applied` sees, under the lock, each
    /// entry's old value and what replaced it (`None`: removed) for each edit that was made.
    pub(crate) fn edit_all_with(
        &self,
        task: &Guard,
        mut edit: impl FnMut(&QueryKey, &V) -> Edit<V>,
        mut applied: impl FnMut(&V, Option<&V>),
    ) {
        blaze_mem::frame::task(Tag::Query, |frame| {
            // Reused for every shard.
            let (mut edits, mut gone) = (Vec::new_in(frame), Vec::new_in(frame));
            for shard in self.shards.iter() {
                self.edit_shard(
                    shard,
                    task,
                    &mut edits,
                    &mut gone,
                    &mut (),
                    |(), key, value| edit(key, value),
                    |(), old, new| applied(old, new),
                );
            }
        })
    }

    /// [`Map::edit_all_with`] with the shards split among the plane's workers, each shard
    /// edited by one, in its own task. `edit` and `applied` run on many workers at once, each
    /// call with the state `start` made for the worker making it, which `end` gets back once
    /// no shard is left for that worker.
    pub(crate) fn edit_all_parallel<S>(
        &self,
        start: impl Fn() -> S + Sync,
        edit: impl Fn(&mut S, &Guard, &QueryKey, &V) -> Edit<V> + Sync,
        applied: impl Fn(&mut S, &V, Option<&V>) + Sync,
        end: impl Fn(S) + Sync,
    ) {
        let next = std::sync::atomic::AtomicUsize::new(0);
        crate::on_workers(self.shards.len(), || {
            let task = blaze_reclaim::enter();
            let mut state = start();
            blaze_mem::frame::task(Tag::Query, |frame| {
                let (mut edits, mut gone) = (Vec::new_in(frame), Vec::new_in(frame));
                while let Some(shard) = self.shards.get(next.fetch_add(1, std::sync::atomic::Ordering::Relaxed)) {
                    self.edit_shard(
                        shard,
                        &task,
                        &mut edits,
                        &mut gone,
                        &mut state,
                        |state, key, value| edit(state, &task, key, value),
                        &applied,
                    );
                }
            });
            end(state);
        });
    }

    /// One shard of [`Map::edit_all_with`]: `edit` decides without the lock, then each edit
    /// still current is made under it. `edits` and `gone` are scratch, left empty.
    #[allow(clippy::too_many_arguments)]
    fn edit_shard<S, A: std::alloc::Allocator + std::alloc::AllocatorNightly, B: std::alloc::Allocator + std::alloc::AllocatorNightly>(
        &self,
        shard: &Shard,
        task: &Guard,
        edits: &mut Vec<(u64, QueryKey, Edit<V>), A>,
        gone: &mut Vec<u32, B>,
        state: &mut S,
        mut edit: impl FnMut(&mut S, &QueryKey, &V) -> Edit<V>,
        mut applied: impl FnMut(&mut S, &V, Option<&V>),
    ) {
        let Some(table) = Self::table(shard, task) else { return };
        edits.clear();
        for place in table.iter() {
            let place = place.load(Ordering::Acquire);
            // SAFETY: loaded from a table of this map in the caller's task.
            let Some(seen) = (unsafe { self.entry_at(place) }) else { continue };
            match edit(state, &seen.key, &seen.value) {
                Edit::Keep => {}
                other => edits.push((place, seen.key, other)),
            }
        }
        if edits.is_empty() {
            return;
        }
        let mut writer = shard.writer.lock();
        gone.clear();
        for (seen, key, edit) in edits.drain(..) {
            let current = shard.table.load(Ordering::Acquire);
            // SAFETY: a table exists (`seen` was in one, and tables are only ever
            // replaced), and only this lock's holder unlinks it.
            let current = unsafe { places(current) };
            let Some((at, now, old)) = self.find(current, &key) else { continue };
            if now != seen {
                continue;
            }
            let Some(old_row) = row_of(now) else { continue };
            match edit {
                Edit::Keep => continue,
                Edit::Remove => {
                    current[at].store(u64::from(TOMB), Ordering::Release);
                    writer.live -= 1;
                    applied(state, &old.value, None);
                }
                Edit::Replace(value) => {
                    let row = self.ids.claim(&mut writer.free);
                    // SAFETY: claimed just now, under the lock, and not yet published.
                    unsafe { self.rows.put(row, Entry { key, value }) };
                    current[at].store(place(tag(&key), row), Ordering::Release);
                    // SAFETY: published just now under the lock; retired only by a later
                    // writer, after which this lock's holder would have to see it again.
                    applied(state, &old.value, Some(&unsafe { self.rows.get(row) }.value));
                }
            }
            gone.push(old_row);
        }
        if !gone.is_empty() {
            let grace = Grace::start();
            for row in gone.drain(..) {
                writer.free.retire(grace, row);
            }
            self.tidy(&mut writer);
        }
    }

    pub(crate) fn edit_all(&self, task: &Guard, edit: impl FnMut(&QueryKey, &V) -> Edit<V>) {
        self.edit_all_with(task, edit, |_, _| {});
    }

    pub(crate) fn retain(&self, task: &Guard, mut keep: impl FnMut(&QueryKey, &V) -> bool) {
        self.edit_all(task, |key, value| if keep(key, value) { Edit::Keep } else { Edit::Remove });
    }

    /// Frees what each shard retired whose grace has passed.
    pub(crate) fn sweep(&self) {
        for shard in self.shards.iter() {
            let mut writer = shard.writer.lock();
            let rows = &self.rows;
            // SAFETY: a retired row's grace has passed: no task that could reach it is left.
            writer.free.reclaim(|row| unsafe { rows.drop_row(row) });
            writer.outgrown.sweep();
        }
    }

    /// How many retired rows and outgrown tables wait for their grace.
    #[cfg(test)]
    pub(crate) fn held(&self) -> usize {
        self.shards
            .iter()
            .map(|shard| {
                let writer = shard.writer.lock();
                writer.free.retired() + writer.outgrown.len()
            })
            .sum()
    }

    /// The row `key`'s entry is in.
    #[cfg(test)]
    pub(crate) fn row(&self, task: &Guard, key: &QueryKey) -> Option<u32> {
        self.get_entry(task, key).and_then(|(place, _)| row_of(place))
    }

    /// Frees everything retired: no reader can hold it with the map borrowed `&mut`.
    pub(crate) fn reclaim(&mut self) {
        let rows = &self.rows;
        for shard in self.shards.iter_mut() {
            let writer = shard.writer.get_mut();
            // SAFETY: `&mut` excludes every reader.
            writer.free.reclaim_all(|row| unsafe { rows.drop_row(row) });
            writer.outgrown = Outgrown::default();
        }
    }

    /// Empties the map, keeping its tables (each at its length), rows and committed pages: for
    /// a map used again once no reader can be in it.
    pub(crate) fn reset(&mut self) {
        let rows = &self.rows;
        for shard in self.shards.iter_mut() {
            let table = *shard.table.get_mut();
            let writer = shard.writer.get_mut();
            // SAFETY: `&mut` excludes every reader.
            writer.free.reclaim_all(|row| unsafe { rows.drop_row(row) });
            writer.outgrown.settle();
            if !table.is_null() {
                // SAFETY: the current table, owned by the map; `&mut` excludes every reader.
                for place in unsafe { places(table) } {
                    if let Some(row) = row_of(place.load(Ordering::Relaxed)) {
                        // SAFETY: a place names a live row of this map, dropped once here.
                        unsafe { rows.drop_row(row) };
                        writer.free.ready(row);
                    }
                    place.store(EMPTY, Ordering::Relaxed);
                }
            }
            writer.live = 0;
            writer.used = 0;
        }
    }

    /// Gives each shard of this empty map a table that holds as many entries as `like`'s
    /// shard holds now, so putting the same keys in grows no table: a key's shard is its
    /// input's bits, the same in both. A table kept at that length is used before a new one.
    pub(crate) fn size_like(&mut self, like: &Map<V>) {
        for (shard, other) in self.shards.iter_mut().zip(&like.shards) {
            let live = other.writer.lock().live;
            // Grows at the insert that would leave it more than half used (`room`).
            let len = (live * 2).next_power_of_two().max(FIRST_TABLE);
            let current = *shard.table.get_mut();
            let writer = shard.writer.get_mut();
            assert!(writer.used == 0, "a map sized like another once it had entries");
            if live == 0 || !current.is_null() && untag(current).1 >= len {
                continue;
            }
            let table = writer.outgrown.take(len).unwrap_or_else(|| new_table(len));
            if !current.is_null() {
                // Empty, and `&mut` excludes every reader: kept as the shard's table of its
                // length.
                writer.outgrown.hold(Grace::start(), Table(current));
                writer.outgrown.settle();
            }
            *shard.table.get_mut() = table;
        }
    }
}

impl<V: Send + Sync + Clone + 'static> Map<V> {
    pub(crate) fn snapshot(&self, task: &Guard) -> Vec<(QueryKey, V)> {
        // alloc: once per image write, the inputs it lists; clone: see above.
        let mut all = Vec::new();
        self.for_each(task, |key, value| all.push((*key, value.clone()))); // clone: see above
        all
    }
}

impl<V> Drop for Map<V> {
    fn drop(&mut self) {
        for shard in self.shards.iter_mut() {
            let table = *shard.table.get_mut();
            if !table.is_null() {
                // SAFETY: the current table, owned by the map; `&mut` excludes every reader.
                // The rows it names are dropped with `rows`.
                drop(unsafe { table_box(table) });
            }
        }
    }
}

/// Chunk `k` of a [`Column`] holds `COLUMN_FIRST << k` places: 29 hold every `u32`.
const COLUMN_FIRST: usize = 16;
const COLUMN_CHUNKS: usize = 29;

fn column_place(index: u32) -> (usize, usize) {
    let biased = index as usize / COLUMN_FIRST + 1;
    let chunk = (usize::BITS - 1 - biased.leading_zeros()) as usize;
    (chunk, index as usize - COLUMN_FIRST * ((1 << chunk) - 1))
}

/// Values by dense index, each published once with a release store and taken out at most
/// once: a lookup is two acquire loads. Chunks never move. For the views: a view's state is a
/// closed view's, emptied, or one allocation when none is ready.
pub(crate) struct Column<T> {
    chunks: [OnceLock<Box<[AtomicPtr<T>]>>; COLUMN_CHUNKS],
    next: AtomicU32,
    _owns: PhantomData<Box<T>>,
}

impl<T: Send + Sync + 'static> Column<T> {
    pub(crate) fn new() -> Column<T> {
        Column { chunks: std::array::from_fn(|_| OnceLock::new()), next: AtomicU32::new(0), _owns: PhantomData }
    }

    fn place(&self, index: u32) -> &AtomicPtr<T> {
        let (chunk, at) = column_place(index);
        &self.chunks[chunk].get_or_init(|| {
            // alloc: once a chunk of the column.
            (0..COLUMN_FIRST << chunk).map(|_| AtomicPtr::new(ptr::null_mut())).collect()
        })[at]
    }

    /// Appends `make(index)` at the next index.
    #[cfg(test)]
    pub(crate) fn push(&self, make: impl FnOnce(u32) -> T) -> u32 {
        // alloc: one a value of a column.
        self.push_boxed(|index| Box::new(make(index)))
    }

    /// Appends the value `make(index)` boxed (a box the caller kept) at the next index.
    pub(crate) fn push_boxed(&self, make: impl FnOnce(u32) -> Box<T>) -> u32 {
        let index = self.next.fetch_add(1, Ordering::Relaxed);
        assert!(index < u32::MAX, "a column of 2^32 values");
        self.place(index).store(Box::into_raw(make(index)), Ordering::Release);
        index
    }

    pub(crate) fn get<'g>(&'g self, _task: &'g Guard, index: u32) -> Option<&'g T> {
        let (chunk, at) = column_place(index);
        let value = self.chunks[chunk].get()?.get(at)?.load(Ordering::Acquire);
        // SAFETY: a published value is freed only after `take` unlinked it and the caller's
        // limbo let a grace pass, or by `&mut`; `_task` keeps this thread in its task.
        (!value.is_null()).then(|| unsafe { &*value })
    }

    /// Unlinks the value at `index`, for the caller to hold in a limbo: a reader may still
    /// have it.
    pub(crate) fn take(&self, index: u32) -> Option<Box<T>> {
        let (chunk, at) = column_place(index);
        let value = self.chunks[chunk].get()?.get(at)?.swap(ptr::null_mut(), Ordering::AcqRel);
        // SAFETY: published by `push` from `Box::into_raw`; the swap made this its one owner.
        (!value.is_null()).then(|| unsafe { Box::from_raw(value) })
    }

    pub(crate) fn for_each<'g>(&'g self, task: &'g Guard, mut f: impl FnMut(&'g T)) {
        for index in 0..self.next.load(Ordering::Acquire) {
            if let Some(value) = self.get(task, index) {
                f(value);
            }
        }
    }

    pub(crate) fn values_mut(&mut self) -> impl Iterator<Item = &mut T> {
        self.chunks.iter_mut().filter_map(OnceLock::get_mut).flat_map(|chunk| chunk.iter_mut()).filter_map(
            |place| {
                let value = *place.get_mut();
                // SAFETY: published from `Box::into_raw` and owned by the column; `&mut`
                // excludes readers.
                (!value.is_null()).then(|| unsafe { &mut *value })
            },
        )
    }

    /// Every value, taken out, and the next index; for [`Column::restore`].
    pub(crate) fn take_all(&mut self) -> (u32, Vec<Box<T>>) {
        let next = *self.next.get_mut();
        (next, (0..next).filter_map(|index| self.take(index)).collect()) // alloc: once a settle
    }

    /// `values` at the indices `index_of` gives, the next index pushed `next`.
    pub(crate) fn restore(&mut self, next: u32, values: Vec<Box<T>>, index_of: impl Fn(&T) -> u32) {
        *self.next.get_mut() = next;
        for value in values {
            let index = index_of(&value);
            assert!(index < next, "a value restored past the column's end");
            let old = self.place(index).swap(Box::into_raw(value), Ordering::AcqRel);
            assert!(old.is_null(), "two values restored at index {index}");
        }
    }
}

impl<T> Drop for Column<T> {
    fn drop(&mut self) {
        for chunk in self.chunks.iter_mut().filter_map(OnceLock::get_mut) {
            for place in chunk.iter_mut() {
                let value = *place.get_mut();
                if !value.is_null() {
                    // SAFETY: published from `Box::into_raw` and never taken; `&mut` excludes
                    // readers.
                    drop(unsafe { Box::from_raw(value) });
                }
            }
        }
    }
}

pub(crate) const EDGE_RECORD: usize = 56;

/// One dependency edge as the database keeps it, in its edge segments and in the image alike:
/// the kind (little-endian), six bytes of padding, the input, and the fingerprint seen
/// (little-endian). Plain bytes, so a mapped image's edge section is read as `&[EdgeRecord]`
/// in place once its bounds and alignment are checked.
#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(C, align(8))]
pub(crate) struct EdgeRecord {
    kind: [u8; 2],
    pad: [u8; 6],
    input: [u8; 32],
    saw: [u8; 16],
}

const _: () = assert!(size_of::<EdgeRecord>() == EDGE_RECORD);
const _: () = assert!(align_of::<EdgeRecord>() == 8);

impl EdgeRecord {
    pub(crate) fn new(read: QueryKey, saw: Fingerprint) -> EdgeRecord {
        EdgeRecord { kind: read.kind.0.to_le_bytes(), pad: [0; 6], input: read.input.0, saw: saw.0.to_le_bytes() }
    }

    pub(crate) fn read(&self) -> QueryKey {
        QueryKey { kind: QueryKind(u16::from_le_bytes(self.kind)), input: ContentId(self.input) }
    }

    pub(crate) fn saw(&self) -> Fingerprint {
        Fingerprint(u128::from_le_bytes(self.saw))
    }

    pub(crate) fn edge(&self) -> Edge {
        Edge { read: self.read(), saw: self.saw() }
    }

    /// The image's order of a memo's edges.
    pub(crate) fn order(&self) -> (QueryKey, Fingerprint) {
        (self.read(), self.saw())
    }

    pub(crate) fn as_bytes(records: &[EdgeRecord]) -> &[u8] {
        // SAFETY: `EdgeRecord` is `repr(C)` byte arrays filling its 56 bytes, with no padding
        // the compiler adds, so every byte of the slice is initialized.
        unsafe { std::slice::from_raw_parts(records.as_ptr().cast::<u8>(), size_of_val(records)) }
    }

    /// `bytes` as records, in place.
    ///
    /// # Safety
    ///
    /// `bytes` is empty or starts at an 8-byte boundary. (Any bytes are a valid record.) An
    /// empty `bytes` may be dangling at any address (`&[]` of `u8` is at 1).
    pub(crate) unsafe fn from_bytes(bytes: &[u8]) -> &[EdgeRecord] {
        if bytes.len() < EDGE_RECORD {
            debug_assert!(bytes.is_empty(), "{} bytes: less than one edge record", bytes.len());
            return &[];
        }
        debug_assert!(bytes.as_ptr().addr() % align_of::<EdgeRecord>() == 0);
        // SAFETY: aligned by the caller's contract, in bounds (`len / 56` whole records), and
        // every bit pattern of a record's byte arrays is valid.
        unsafe { std::slice::from_raw_parts(bytes.as_ptr().cast::<EdgeRecord>(), bytes.len() / EDGE_RECORD) }
    }
}

#[cfg(test)]
#[derive(Clone, Copy)]
#[repr(C, align(16))]
struct Block([u8; 16]);

/// Bytes at a 16-byte-aligned address ([`blaze_format::ALIGN`]), as a mapping a test makes.
#[cfg(test)]
pub(crate) struct Aligned {
    blocks: Box<[Block]>,
    len: usize,
}

#[cfg(test)]
impl Aligned {
    pub(crate) fn copy(bytes: &[u8]) -> Aligned {
        let mut blocks = vec![Block([0; 16]); bytes.len().div_ceil(16)].into_boxed_slice();
        for (block, chunk) in blocks.iter_mut().zip(bytes.chunks(16)) {
            block.0[..chunk.len()].copy_from_slice(chunk);
        }
        Aligned { blocks, len: bytes.len() }
    }

    pub(crate) fn bytes(&self) -> &[u8] {
        // SAFETY: `blocks` is `len.div_ceil(16)` blocks of 16 initialized bytes with no
        // padding (`Block` is a `[u8; 16]`), so its first `len` bytes are in bounds.
        unsafe { std::slice::from_raw_parts(self.blocks.as_ptr().cast::<u8>(), self.len) }
    }
}

#[cfg(test)]
mod tests;
