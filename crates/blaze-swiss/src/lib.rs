//! The SwissTable group probe every interner's table shares (docs/v2/plan/design/data.md,
//! "interner and memo lookups"): `intern`'s arena store and mapped index, and the query engine's
//! interned key map. `blaze-mem` re-exports it as `blaze_mem::swiss` and `intern` as
//! `intern::swiss`; it is a crate of its own with no dependencies, so `blaze-ids`, at the bottom
//! of the tree, reaches it without `blaze-mem`'s allocators, channels and system calls
//! (docs/v2/plan/design/bottom.md, "Also decided on 2026-11-15").
//!
//! A table is a power of two of groups; a group is a [`Ctrl`] word of tag bytes beside its
//! slots. A lookup of `hash` starts at the group `hash & mask` and goes on group by group
//! until one has an empty slot, comparing only the slots whose tag is the hash's top seven
//! bits. A group is one word, so matching its slots is a few ALU operations and no branch per
//! slot (SWAR, as hashbrown's generic group): a 64-bit word holds eight tags, where SSE2 or
//! NEON would hold sixteen, but a slot of a pointer or ID stays in the group's own line, which
//! a 16-wide group can't.
//!
//! [`find`] and [`vacant`] load no memory: the caller hands them a closure that loads a
//! group's word, with whatever atomics and ordering its table uses (std's, the model
//! checker's, shuttle's, or a mapped `&[u64]`), so one probe serves them all. [`Index`] is the
//! one table built on them here: `u32`s naming a column the caller keeps, which every
//! interner keyed by an index (`blaze_ids::Sharded`, the hash-conser, base-db's tables) uses.

#![no_std]
#![feature(allocator_ext)]
// `core::hint::prefetch_read`: a batch's lookups start every home group's load before the
// first probe, so their misses overlap ([`Index::find_many`], `blaze_hash`'s `get_many`).
#![feature(hint_prefetch)]
// blaze's lints and its core marker name their tool; inert under rustc.
#![feature(register_tool, custom_inner_attributes)]
#![register_tool(blaze)]
// No IO, no clock, no environment, no process state: atomics, the allocator an `Index` is
// handed, and backoffs bounded by a `BackoffLimits` value (`sync`).
#![blaze::sans_io]
#![deny(blaze::sans_io_core)]
#![deny(blaze::std_use)]
#![deny(unsafe_code)]

// The tests' threads, collections and counting allocator: `core` has none.
#[cfg(test)]
extern crate std;

use blaze_own::planted::{NoPlant, Planted, Plants};

use core::{alloc::Allocator, fmt};

use blaze_int::Int;
use blaze_own::{Box, Vec};

// The atomics are `core`'s, or under `cfg(blaze_check)` the model checker's
// (docs/v2/plan/design/model-checker.md "The switch"), as is the once-cell.
use crate::sync::{
    Once,
    atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering},
};

pub mod sync;

/// Counts allocations so the probe's tests hold it to none.
#[cfg(test)]
#[global_allocator]
static ALLOC: blaze_alloc_count::Counting = blaze_alloc_count::Counting;

/// The tag of an empty slot.
pub const EMPTY: u8 = 0;

/// The tag of a slot whose key was removed: full, so probes go on past it, and never a key's
/// tag in a table that removes ([`live_tag`]).
pub const DEAD: u8 = 0x80;

/// A full slot's tag: `0x80 |` the hash's top seven bits. Tables that never remove a key use
/// it; it is part of `intern`'s image layout, so it never changes.
#[inline]
pub fn tag(hash: u64) -> u8 {
    // `hash >> 57` is seven bits: it fits a `u8`.
    0x80 | (hash >> 57).narrow_or_stop::<u8>()
}

/// [`tag`] with 0 folded into 1, so no key's tag is [`DEAD`]: for tables that remove keys.
#[inline]
pub fn live_tag(hash: u64) -> u8 {
    // `hash >> 57` is seven bits: it fits a `u8`.
    0x80 | (hash >> 57).narrow_or_stop::<u8>().max(1)
}

/// A group's control word: byte `i` (from the least significant) is slot `i`'s tag, [`EMPTY`]
/// or `0x80 |` seven bits. Bytes `SLOTS..8` stay 0 and are masked out of every answer, so a
/// group of seven pointers and its word fill one 64-byte line.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Ctrl<const SLOTS: usize>(pub u64);

impl<const SLOTS: usize> Ctrl<SLOTS> {
    /// Bit 0 of each slot byte.
    pub const LOW: u64 = {
        assert!(SLOTS >= 1 && SLOTS <= 8, "a control word holds one to eight slots");
        let mut low = 0u64;
        let mut slot = 0;
        while slot < SLOTS {
            low |= 1 << (slot * 8);
            slot += 1;
        }
        low
    };
    /// Bit 7 of each slot byte.
    pub const HIGH: u64 = Self::LOW << 7;
    /// Bits 0 to 6 of each slot byte.
    const SEVEN: u64 = Self::LOW * 0x7f;

    /// The slots tagged `tag`, exactly: `x` is zero in the bytes equal to `tag`, and a byte's
    /// low seven bits plus `0x7f` sets its top bit exactly when they are not all zero, without
    /// carrying into the next byte.
    #[inline]
    pub fn matching(self, tag: u8) -> Slots {
        let x = self.0 ^ (Self::LOW * u64::from(tag));
        Slots(!(((x & Self::SEVEN) + Self::SEVEN) | x) & Self::HIGH)
    }

    /// The empty slots: a full byte has its top bit set.
    #[inline]
    pub fn empty(self) -> Slots {
        Slots(!self.0 & Self::HIGH)
    }

    /// The full slots, dead ones included.
    #[inline]
    pub fn full(self) -> Slots {
        Slots(self.0 & Self::HIGH)
    }

    /// The full slots that hold a key: not [`DEAD`], whose low seven bits are all zero.
    #[inline]
    pub fn live(self) -> Slots {
        Slots(self.0 & ((self.0 & Self::SEVEN) + Self::SEVEN) & Self::HIGH)
    }

    /// Slot `slot`'s tag.
    #[inline]
    pub fn tag_at(self, slot: usize) -> u8 {
        debug_assert!(slot < SLOTS);
        // The slot's byte: the bytes above it are dropped on purpose.
        (self.0 >> (slot * 8)).truncate_to::<u8>()
    }

    /// The word with slot `slot`'s tag set to `tag`.
    #[inline]
    pub fn with(self, slot: usize, tag: u8) -> Ctrl<SLOTS> {
        debug_assert!(slot < SLOTS);
        let shift = slot * 8;
        Ctrl((self.0 & !(0xff << shift)) | (u64::from(tag) << shift))
    }
}

/// A set of a group's slots, as the top bit of each one's byte; iterates in slot order.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Slots(u64);

impl Slots {
    #[inline]
    pub fn any(self) -> bool {
        self.0 != 0
    }

    #[inline]
    pub fn first(self) -> Option<usize> {
        self.any().then(|| (self.0.trailing_zeros() / 8).widen_to::<usize>())
    }
}

impl Iterator for Slots {
    type Item = usize;

    #[inline]
    fn next(&mut self) -> Option<usize> {
        let slot = self.first()?;
        self.0 &= self.0 - 1;
        Some(slot)
    }
}

/// A slot of a table: its group, its place in the group, and the group's word as the probe
/// loaded it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct At<const SLOTS: usize> {
    pub group: usize,
    pub slot: usize,
    pub ctrl: Ctrl<SLOTS>,
}

impl<const SLOTS: usize> At<SLOTS> {
    /// The slot's index among the table's, for a column beside the groups (a slot's hash).
    #[inline]
    pub fn index(self) -> usize {
        self.group * SLOTS + self.slot
    }

    /// The group's word with this slot tagged `tag`: what the inserter publishes.
    #[inline]
    pub fn tagged(self, tag: u8) -> Ctrl<SLOTS> {
        self.ctrl.with(self.slot, tag)
    }
}

/// The group a probe of `hash` starts at in a table of `groups` (a power of two): where
/// [`find`] and [`vacant`] begin, and what a prefetch ahead of either loads.
#[inline]
pub fn home(hash: u64, groups: usize) -> usize {
    debug_assert!(groups.is_power_of_two());
    hash.truncate_to::<usize>() & (groups - 1)
}

/// Starts loading the cache line at `at` for a read soon, into the nearest cache: a hint that
/// loads nothing the caller can see and faults on no address.
#[inline(always)]
pub fn prefetch<T>(at: *const T) {
    core::hint::prefetch_read(at, core::hint::Locality::L1);
}

/// The first slot tagged `tag` that `hit` accepts, probing from `hash`'s home group in a table
/// of `groups` (a power of two) until a group with an empty slot. `ctrl` loads group `g`'s
/// word; a table never fills, so a miss ends.
#[inline]
pub fn find<const SLOTS: usize, T>(
    hash: u64,
    tag: u8,
    groups: usize,
    ctrl: impl FnMut(usize) -> Ctrl<SLOTS>,
    hit: impl FnMut(At<SLOTS>) -> Option<T>,
) -> Option<T> {
    find_or_vacant(hash, tag, groups, ctrl, hit).ok()
}

/// [`find`], and on a miss the slot an insert of `hash` takes: the first empty slot of the
/// group the probe stopped at, which is where [`vacant`] would put it. So a get-or-insert
/// probes once either way; the vacancy holds until the table changes.
#[inline]
pub fn find_or_vacant<const SLOTS: usize, T>(
    hash: u64,
    tag: u8,
    groups: usize,
    mut ctrl: impl FnMut(usize) -> Ctrl<SLOTS>,
    mut hit: impl FnMut(At<SLOTS>) -> Option<T>,
) -> Result<T, At<SLOTS>> {
    let mask = groups - 1;
    let mut group = home(hash, groups);
    loop {
        let word = ctrl(group);
        for slot in word.matching(tag) {
            if let Some(found) = hit(At { group, slot, ctrl: word }) {
                return Ok(found);
            }
        }
        if let Some(slot) = word.empty().first() {
            return Err(At { group, slot, ctrl: word });
        }
        group = (group + 1) & mask;
    }
}

/// The first empty slot from `hash`'s home group, where an insert of `hash` goes: the group a
/// [`find`] of it stops at. For the table's one writer.
#[inline]
pub fn vacant<const SLOTS: usize>(
    hash: u64,
    groups: usize,
    mut ctrl: impl FnMut(usize) -> Ctrl<SLOTS>,
) -> At<SLOTS> {
    let mask = groups - 1;
    let mut group = home(hash, groups);
    loop {
        let word = ctrl(group);
        if let Some(slot) = word.empty().first() {
            return At { group, slot, ctrl: word };
        }
        group = (group + 1) & mask;
    }
}

/// Slots in a group of an [`Index`]: a control word and six `u32`s, 32 bytes aligned to 32, so
/// two groups fill a 64-byte line and none straddles two. Eight slots made a 40-byte group, half
/// of which crossed a line: about a third of the slots a hit reads were a line away from their
/// control word.
const INDEX_SLOTS: usize = 6;
/// Table `k` of an [`Index`] has `1 << k` groups: 31 levels hold more than `2^32` values at
/// three quarters full (`2^30 × 6 × 3 / 4`).
const LEVELS: usize = 31;

#[repr(C, align(32))]
struct Group {
    ctrl: AtomicU64,
    values: [AtomicU32; INDEX_SLOTS],
}

#[cfg(not(blaze_check))]
const _: () = assert!(size_of::<Group>() == 32 && align_of::<Group>() == 32);

/// A table of `len` empty groups in `alloc`: growth, one table per doubling of the values,
/// over the inserts that filled the last.
fn new_table<A: Allocator>(len: usize, alloc: A) -> Box<[Group], A> {
    let mut groups = Vec::with_capacity_in(len, alloc);
    groups.resize_with(len, || Group {
        ctrl: AtomicU64::new(0),
        values: core::array::from_fn(|_| AtomicU32::new(0)),
    });
    groups.into_boxed_slice()
}

/// One level of an [`Index`]: its groups, and the log of the values inserted into them while
/// the next level is built from them outside the writer's lock ([`Grow`]).
struct Table<A: Allocator> {
    groups: Box<[Group], A>,
    /// One entry per group: more than the inserts a table takes between three quarters full,
    /// where its growth starts, and seven eighths, where inserts wait for it ([`Full`]).
    log: Box<[AtomicU32], A>,
}

impl<A: Allocator + Clone> Table<A> {
    fn new_in(groups: usize, alloc: A) -> Table<A> {
        // clone: the allocator's handle, once per level.
        let mut log = Vec::with_capacity_in(groups, alloc.clone());
        log.resize_with(groups, || AtomicU32::new(0));
        Table { groups: new_table(groups, alloc), log: log.into_boxed_slice() }
    }
}

/// Whether a table of `groups` takes its `len + 1`th value without starting to grow: at most
/// three quarters full after it.
fn below_growth(len: usize, groups: usize) -> bool {
    (len + 1) * 4 <= groups * INDEX_SLOTS * 3
}

/// Whether a table of `groups` has room for its `len + 1`th value while it grows: at most
/// seven eighths full after it, so a probe always meets an empty slot.
fn below_full(len: usize, groups: usize) -> bool {
    (len + 1) * 8 <= groups * INDEX_SLOTS * 7
}

/// [`Index::find`] in the level `groups`.
#[inline]
fn find_in(groups: &[Group], hash: u64, mut is: impl FnMut(u32) -> bool) -> Option<u32> {
    let ctrl = |at: usize| Ctrl::<INDEX_SLOTS>(groups[at].ctrl.load(Ordering::Acquire));
    find(hash, tag(hash), groups.len(), ctrl, |at| {
        // Relaxed: the tag's `Acquire` ordered the slot's store before us.
        let value = groups[at.group].values[at.slot].load(Ordering::Relaxed);
        is(value).then_some(value)
    })
}

/// Whether `groups` holds `value` under `hash`, for the one thread filling them.
fn holds(groups: &[Group], hash: u64, value: u32) -> bool {
    let ctrl = |at: usize| Ctrl::<INDEX_SLOTS>(groups[at].ctrl.load(Ordering::Relaxed));
    find(hash, tag(hash), groups.len(), ctrl, |at| {
        (groups[at.group].values[at.slot].load(Ordering::Relaxed) == value).then_some(())
    })
    .is_some()
}

/// A SwissTable of `u32`s, each naming a value in a column the caller keeps, found by a hash
/// the caller computed once: what an interner keyed by a dense index puts beside its arena.
///
/// A [`Index::find`] takes no lock and writes nothing: an acquire load of the level, one of
/// the home group's word (almost always the only one) and one slot from the same 32 bytes, one
/// cache line, then the caller's check, which reads its column only on a tag match (one in 128
/// for another value). Inserts are the caller's to serialize (its shard's lock).
///
/// A table that reaches three quarters full hands its writer a [`Grow`]: the next level, twice
/// its size, is built from it outside the caller's lock ([`Grow::build`]), while other writers
/// go on inserting into it to seven eighths full and log what they insert; then, under the
/// lock again, [`Built::publish`] re-checks the log, copies what came in during the build and
/// publishes the level with one release store. A reader never waits: it finishes on the level
/// it loaded, and the levels outgrown stay readable until [`Index::release_outgrown`] frees
/// them with `&mut`. So the lock is held for O(1) per insert and O(inserts during the build)
/// per growth, never for the O(n) copy. A value costs 7.6 to 15.2 bytes of table and log, and
/// nothing is stored of its hash: growing asks the caller for it. [`Index::insert`] does all
/// of it in one call, for a writer whose lock nobody else waits on.
///
/// Its tables are in the allocator `A`, which the maker names ([`Index::new_in`]); an index
/// with no value has no table and allocates nothing.
pub struct Index<A: Allocator + Clone> {
    /// How many levels are published; the last is in use.
    level: AtomicUsize,
    tables: [Once<Table<A>>; LEVELS],
    /// A [`Grow`] of the level in use is out: written under the writer's lock, and cleared by
    /// a [`Grow`] or [`Built`] dropped unpublished.
    growing: AtomicBool,
    /// Entries of the log of the level in use since its [`Grow`] began, written under the
    /// writer's lock.
    logged: AtomicUsize,
    alloc: A,
}

/// Room for one more value in an [`Index`], made under the writer's lock by
/// [`Index::vacancy`]: [`Vacancy::fill`] puts it in.
#[must_use = "a vacancy is room for one value; fill it under the same lock"]
pub struct Vacancy<'a, A: Allocator + Clone> {
    index: &'a Index<A>,
    level: usize,
    /// The log entry to write too, while a [`Grow`] of the level is out.
    log: Option<usize>,
    /// The growth this insert starts, handed to its writer.
    grow: Option<Grow<'a, A>>,
}

/// The table in use is seven eighths full and its next level is being built: the writer
/// unlocks, runs `grow` if it holds it (the grower before it panicked), and waits for
/// [`Index::level`] to pass `level`.
#[must_use = "an insert that met a full table did not happen"]
pub struct Full<'a, A: Allocator + Clone> {
    pub level: usize,
    pub grow: Option<Grow<'a, A>>,
}

impl<A: Allocator + Clone> fmt::Debug for Vacancy<'_, A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Vacancy")
            .field("level", &self.level)
            .field("log", &self.log)
            .field("grows", &self.grow.is_some())
            .finish()
    }
}

impl<A: Allocator + Clone> fmt::Debug for Full<'_, A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Full")
            .field("level", &self.level)
            .field("grow", &self.grow.as_ref().map(|grow| grow.level))
            .finish()
    }
}

/// The right and duty to build the next level of an [`Index`], outside the writer's lock.
/// Dropped unbuilt (its writer panicked), the next writer to insert starts the growth again.
#[must_use = "a table at three quarters full grows only through its Grow"]
pub struct Grow<'a, A: Allocator + Clone> {
    growing: Growing<'a, A>,
    level: usize,
}

/// A next level built from the one in use and the log up to `copied`; [`Built::publish`]
/// finishes it under the writer's lock.
#[must_use = "a built level is used only once published under the writer's lock"]
pub struct Built<'a, A: Allocator + Clone> {
    growing: Growing<'a, A>,
    level: usize,
    table: Table<A>,
    copied: usize,
}

/// Clears `growing` when a growth ends, published or dropped.
struct Growing<'a, A: Allocator + Clone> {
    index: &'a Index<A>,
}

impl<A: Allocator + Clone> Drop for Growing<'_, A> {
    fn drop(&mut self) {
        // Release: a writer that sees no growth sees the level it published, if it did.
        self.index.growing.store(false, Ordering::Release);
    }
}

// `Default` and `Index::new()` (an index in `A::default()`, an allocator chosen by no caller:
// ambient, D170 C1) were removed on 2026-11-20; nothing in the tree called either.
// [`Index::new_in`] is the one constructor.

/// The bugs a model test plants in an [`Index`]'s growth, through the [`Planted`] seam
/// (`blaze_own::planted`, D165). Shipped code passes [`NoPlant`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum IndexPlant {
    /// The index as it is.
    #[cfg_attr(not(all(test, blaze_check)), expect(dead_code, reason = "named by the model tests alone"))]
    None,
    /// [`Built::publish`] swaps in what the build saw without re-checking the log under the
    /// writer's lock: an insert made during the build is lost.
    SkipsRecheck,
}

impl Plants for IndexPlant {}

impl<A: Allocator + Clone> Index<A> {
    /// An empty index whose tables go in `alloc`: no allocation until the first insert.
    pub fn new_in(alloc: A) -> Index<A> {
        Index {
            level: AtomicUsize::new(0),
            tables: core::array::from_fn(|_| Once::new()),
            growing: AtomicBool::new(false),
            logged: AtomicUsize::new(0),
            alloc,
        }
    }

    fn table_at(&self, level: usize) -> Option<&Table<A>> {
        let level = level.checked_sub(1)?;
        Some(self.tables[level].get().expect("a published index level holds its table"))
    }

    fn at_level(&self, level: usize) -> Option<&[Group]> {
        self.table_at(level).map(|table| &*table.groups)
    }

    /// How many levels are published: what a writer that met [`Full`] waits to see pass.
    pub fn level(&self) -> usize {
        // Acquire: a level seen is a table whose slots are seen.
        self.level.load(Ordering::Acquire)
    }

    /// The value inserted under `hash` that `is` accepts. `is` is asked only of values whose
    /// insert was published, so what the caller wrote of one before [`Index::insert`] is there.
    #[inline]
    pub fn find(&self, hash: u64, is: impl FnMut(u32) -> bool) -> Option<u32> {
        let groups = self.at_level(self.level.load(Ordering::Acquire))?;
        find_in(groups, hash, is)
    }

    /// Starts loading `hash`'s home group (its word and its slots: one 32-byte group, in one
    /// line), for a caller that interleaves its own lookups: a [`Index::find`] of `hash` soon
    /// after finds the line on its way.
    #[inline]
    pub fn prefetch(&self, hash: u64) {
        if let Some(groups) = self.at_level(self.level.load(Ordering::Acquire)) {
            prefetch(core::ptr::from_ref(&groups[home(hash, groups.len())]));
        }
    }

    /// [`Index::find`] of each of `hashes` (`is(at, value)` asks of `hashes[at]`), as group
    /// prefetching: every home group's load is started first, then each home word is read and
    /// `ahead` told the first value under its tag (the caller prefetches that value's place in
    /// its column: a hint, which a miss of another value's tag makes wasted, never wrong), then
    /// each is probed in order. A table larger than the cache pays one miss's latency for the
    /// batch's groups and one for their columns, not two per lookup.
    ///
    /// When it pays (`examples/index_cost.rs`, an M-series Mac): on 10^7 values, batches of
    /// 32 to 64 run hits 2.0 to 2.4 times and misses 1.6 to 2.0 times as fast as `find` one
    /// at a time (a hit there is two dependent misses, which out-of-order execution cannot
    /// overlap by itself); batches of 4 to 8 gain little. On an index the cache holds (10^3
    /// to 10^5 values) every batch size loses, to 0.76 times: call `find` there.
    pub fn find_many<const N: usize>(
        &self,
        hashes: [u64; N],
        mut ahead: impl FnMut(u32),
        mut is: impl FnMut(usize, u32) -> bool,
    ) -> [Option<u32>; N] {
        let Some(groups) = self.at_level(self.level.load(Ordering::Acquire)) else {
            return [None; N];
        };
        for &hash in &hashes {
            prefetch(core::ptr::from_ref(&groups[home(hash, groups.len())]));
        }
        for &hash in &hashes {
            let group = &groups[home(hash, groups.len())];
            // Acquire, as `find`'s word: the slot's store is ordered before its tag's.
            let word = Ctrl::<INDEX_SLOTS>(group.ctrl.load(Ordering::Acquire));
            if let Some(slot) = word.matching(tag(hash)).next() {
                ahead(group.values[slot].load(Ordering::Relaxed));
            }
        }
        core::array::from_fn(|at| find_in(groups, hashes[at], |value| is(at, value)))
    }

    /// Publishes `value` under `hash` and, if that fills the table to three quarters, grows it
    /// in place: for a writer no other thread waits on, or one whose lock is held for the
    /// copy anyway. `len` is how many values the index held before this one, and `hash_of` the
    /// hash a held value was inserted under, which growing asks for. Whatever `find`'s `is`
    /// reads of `value` must be written before this call. A writer that others wait on calls
    /// [`Index::vacancy`] and runs the [`Grow`] after unlocking.
    pub fn insert(&self, len: usize, hash: u64, value: u32, hash_of: impl Fn(u32) -> u64) {
        loop {
            match self.vacancy(len) {
                Ok(vacancy) => {
                    if let Some(grow) = vacancy.fill(hash, value) {
                        grow.build(&hash_of).publish(&hash_of);
                    }
                    return;
                }
                Err(Full { grow: Some(grow), .. }) => grow.build(&hash_of).publish(&hash_of),
                Err(Full { level, grow: None }) => panic!(
                    "Index::insert met level {level} growing in another thread: `insert` is \
                     for a writer that grows its own index"
                ),
            }
        }
    }

    /// Room for one more value, for the one writer at a time the caller lets in; `len` is how
    /// many values the index holds. O(1): the first table (one group) is made here, and every
    /// later one by the [`Grow`] that fills it.
    pub fn vacancy(&self, len: usize) -> Result<Vacancy<'_, A>, Full<'_, A>> {
        // Relaxed: only the one writer stores `level`.
        let level = self.level.load(Ordering::Relaxed);
        let Some(table) = self.table_at(level) else {
            // clone: the allocator's handle, once per index.
            if self.tables[0].set(Table::new_in(1, self.alloc.clone())).is_err() {
                unreachable!("index level 0 published twice");
            }
            // Release: a reader that loads the level sees the table's empty groups.
            self.level.store(1, Ordering::Release);
            return Ok(Vacancy { index: self, level: 1, log: None, grow: None });
        };
        let groups = table.groups.len();
        // Acquire: a growth dropped unpublished outside the lock released its end.
        if self.growing.load(Ordering::Acquire) {
            // Relaxed: only the one writer stores `logged` while a growth is out.
            let logged = self.logged.load(Ordering::Relaxed);
            if below_full(len, groups) && logged < table.log.len() {
                return Ok(Vacancy { index: self, level, log: Some(logged), grow: None });
            }
            return Err(Full { level, grow: None });
        }
        let room = below_full(len, groups);
        if room && below_growth(len, groups) {
            return Ok(Vacancy { index: self, level, log: None, grow: None });
        }
        assert!(level < LEVELS, "an index of more than 2^32 values");
        self.growing.store(true, Ordering::Relaxed);
        self.logged.store(0, Ordering::Relaxed);
        let grow = Grow { growing: Growing { index: self }, level };
        // The value that crosses three quarters goes in before the growth's scan, which sees
        // it; past seven eighths (a growth before was dropped) it waits for this one.
        if room {
            Ok(Vacancy { index: self, level, log: None, grow: Some(grow) })
        } else {
            Err(Full { level, grow: Some(grow) })
        }
    }

    /// How many values the table in use holds at most before it grows: 0 before the first insert.
    pub fn capacity(&self) -> usize {
        self.at_level(self.level.load(Ordering::Acquire))
            .map_or(0, |groups| groups.len() * INDEX_SLOTS * 3 / 4)
    }

    /// Frees the tables the index outgrew. With `&mut`, no reader is probing one.
    pub fn release_outgrown(&mut self) {
        let level = self.level.load(Ordering::Relaxed);
        for table in &mut self.tables[..level.saturating_sub(1)] {
            *table = Once::new();
        }
    }
}

impl<'a, A: Allocator + Clone> Vacancy<'a, A> {
    /// Publishes `value` under `hash`, still under the lock that made the vacancy; whatever
    /// `find`'s `is` reads of `value` must be written before. `Some`: this insert filled the
    /// table to three quarters, and its writer runs the [`Grow`] once it unlocks.
    pub fn fill(self, hash: u64, value: u32) -> Option<Grow<'a, A>> {
        let index = self.index;
        let table = index.table_at(self.level).expect("a vacancy's level is published");
        put(&table.groups, hash, value, Ordering::Release);
        if let Some(entry) = self.log {
            table.log[entry].store(value, Ordering::Relaxed);
            // Release: a grower that loads the count sees the entries below it.
            index.logged.store(entry + 1, Ordering::Release);
        }
        self.grow
    }
}

impl<'a, A: Allocator + Clone> Grow<'a, A> {
    /// The next level, built outside the writer's lock from the level in use and the log so
    /// far: O(n), while writers go on inserting into the level in use and readers probe it.
    pub fn build(self, hash_of: impl Fn(u32) -> u64) -> Built<'a, A> {
        let index = self.growing.index;
        let level = self.level;
        let from = index.table_at(level).expect("a growth's level is published");
        let table = Table::new_in(1 << level, index.alloc.clone());
        // Every value in `from` when the growth began: its writer handed us the growth, and
        // its lock ordered what came before.
        for group in &*from.groups {
            // Acquire: a full slot's value was stored before its tag was released.
            for slot in Ctrl::<INDEX_SLOTS>(group.ctrl.load(Ordering::Acquire)).full() {
                let value = group.values[slot].load(Ordering::Relaxed);
                put(&table.groups, hash_of(value), value, Ordering::Relaxed);
            }
        }
        // Acquire: pairs with each logging writer's release of the count.
        let copied = index.logged.load(Ordering::Acquire);
        copy_log(&table, &from.log[..copied], &hash_of);
        Built { growing: self.growing, level, table, copied }
    }
}

impl<A: Allocator + Clone> Built<'_, A> {
    /// Re-checks the log under the writer's lock, copies the values inserted since
    /// [`Grow::build`] read it, and publishes the level: O(inserts during the build).
    pub fn publish(self, hash_of: impl Fn(u32) -> u64) {
        self.publish_planted(hash_of, NoPlant);
    }

    /// [`Built::publish`] with a plant: [`IndexPlant::SkipsRecheck`] publishes what the build
    /// saw, without the re-check (the model test's planted bug).
    pub(crate) fn publish_planted(self, hash_of: impl Fn(u32) -> u64, plant: impl Planted<IndexPlant>) {
        if !plant.has(IndexPlant::SkipsRecheck) {
            let index = self.growing.index;
            let from = index.table_at(self.level).expect("a growth's level is published");
            // Relaxed: the writer's lock, which we hold, ordered every logging insert.
            let logged = index.logged.load(Ordering::Relaxed);
            copy_log(&self.table, &from.log[self.copied..logged], &hash_of);
        }
        self.swap();
    }

    fn swap(self) {
        let Built { growing, level, table, copied: _ } = self;
        let index = growing.index;
        if index.tables[level].set(table).is_err() {
            unreachable!("index level {level} published twice");
        }
        // Release: a reader that loads the new level sees the table's slots and, through the
        // caller's order, the values they name.
        index.level.store(level + 1, Ordering::Release);
        drop(growing);
    }
}

/// Puts each logged value not in `table` already: the build's scan may have seen it too.
fn copy_log<A: Allocator>(table: &Table<A>, log: &[AtomicU32], hash_of: impl Fn(u32) -> u64) {
    for entry in log {
        let value = entry.load(Ordering::Relaxed);
        let hash = hash_of(value);
        if !holds(&table.groups, hash, value) {
            put(&table.groups, hash, value, Ordering::Relaxed);
        }
    }
}

/// Fills the first empty slot from `hash`'s home group of `groups`, for the one writer.
fn put(groups: &[Group], hash: u64, value: u32, publish: Ordering) {
    let at = vacant(hash, groups.len(), |at: usize| {
        Ctrl::<INDEX_SLOTS>(groups[at].ctrl.load(Ordering::Relaxed))
    });
    let group = &groups[at.group];
    group.values[at.slot].store(value, Ordering::Relaxed);
    group.ctrl.store(at.tagged(tag(hash)).0, publish);
}

#[cfg(test)]
mod tests {
    use std::{boxed::Box, vec, vec::Vec};

    // The index's tests alone, which a model build leaves out.
    #[cfg(not(blaze_check))]
    use blaze_own::testing::heap;

    use blaze_sim::{Config, Tape};

    use super::*;

    /// The copies this module replaced (`intern`'s arena and mapped index, the old query
    /// engine's interned table and node map), as the spec: the subtraction trick, whose borrow
    /// may also flag the byte above a match, filtered by comparing the byte.
    mod reference {
        pub fn matching(word: u64, tag: u64, low: u64) -> u64 {
            let high = low << 7;
            let x = word ^ (low * tag);
            let mut hits = x.wrapping_sub(low) & !x & high;
            let mut exact = 0;
            while hits != 0 {
                let byte = hits.trailing_zeros() / 8;
                hits &= hits - 1;
                if (word >> (byte * 8)) & 0xff == tag {
                    exact |= 0x80 << (byte * 8);
                }
            }
            exact
        }

        pub fn empty(word: u64, low: u64) -> u64 {
            !word & (low << 7)
        }

        pub fn live(word: u64, low: u64) -> u64 {
            let high = low << 7;
            word & high & ((word & !high).wrapping_add(low * 0x7f) & high)
        }

        /// The probe loop as each copy wrote it, over plain words.
        pub fn find(
            words: &[u64],
            slots: usize,
            hash: u64,
            tag: u64,
            low: u64,
            hit: impl Fn(usize) -> bool,
        ) -> Option<usize> {
            let mask = words.len() - 1;
            let mut at = hash as usize & mask;
            loop {
                let word = words[at];
                let mut hits = matching(word, tag, low);
                while hits != 0 {
                    let byte = (hits.trailing_zeros() / 8) as usize;
                    hits &= hits - 1;
                    if hit(at * slots + byte) {
                        return Some(at * slots + byte);
                    }
                }
                if empty(word, low) != 0 {
                    return None;
                }
                at = (at + 1) & mask;
            }
        }
    }

    fn bits(slots: Slots) -> u64 {
        slots.fold(0, |bits, slot| bits | 0x80 << (slot * 8))
    }

    /// A word of `SLOTS` slot bytes drawn from `bytes`, empty, dead or tags, the rest zero.
    fn word<const SLOTS: usize>(bytes: [u8; 8]) -> Ctrl<SLOTS> {
        let mut word = 0;
        for (slot, &byte) in bytes.iter().enumerate().take(SLOTS) {
            let byte = match byte % 4 {
                0 => EMPTY,
                1 => DEAD,
                _ => 0x80 | byte,
            };
            word |= u64::from(byte) << (slot * 8);
        }
        Ctrl(word)
    }

    fn agrees<const SLOTS: usize>(bytes: [u8; 8], tag: u8) {
        let word = word::<SLOTS>(bytes);
        let low = Ctrl::<SLOTS>::LOW;
        assert_eq!(
            bits(word.matching(tag)),
            reference::matching(word.0, u64::from(tag), low),
            "{word:x?} {tag:x}"
        );
        assert_eq!(bits(word.empty()), reference::empty(word.0, low));
        assert_eq!(bits(word.live()), reference::live(word.0, low));
        let naive: Vec<usize> = (0..SLOTS).filter(|&slot| word.tag_at(slot) == tag).collect();
        assert_eq!(word.matching(tag).collect::<Vec<_>>(), naive);
        let full: Vec<usize> = (0..SLOTS).filter(|&slot| word.tag_at(slot) & 0x80 != 0).collect();
        assert_eq!(word.full().collect::<Vec<_>>(), full);
    }

    #[test]
    fn words_agree_with_the_copies() {
        blaze_sim::check(Config::cases(CASES), |tape| {
            let bytes: [u8; 8] = tape.array("bytes", |tape| tape.int::<u8>("byte"));
            let tag: u8 = tape.int_in(0x80..=u8::MAX, "tag");
            agrees::<7>(bytes, tag);
            agrees::<8>(bytes, tag);
            agrees::<4>(bytes, tag);
        })
    }

    /// `find` against the copies' loop, over tables a third to seven eighths full whose
    /// keys share few hashes, and `vacant` against the group `find` stops at.
    #[test]
    fn probes_agree_with_the_copies() {
        blaze_sim::check(Config::cases(CASES), |tape| probes_agree(tape))
    }

    /// Cases of each property: few under Miri, which interprets every one
    /// (`cargo miri test -p blaze-swiss` in under five minutes).
    const CASES: u32 = if cfg!(miri) { 16 } else { Config::DEFAULT.cases };

    /// A bound that is `small` under Miri.
    macro_rules! sized {
        ($full:expr, $small:expr) => {
            if cfg!(miri) { $small } else { $full }
        };
    }

    fn probes_agree(tape: &Tape) {
        let log_groups: u32 = tape.int_in(0..=sized!(5u32, 2), "log groups");
        let fill: u64 = tape.int_in(3..=7u64, "fill eighths");
        let hash: u64 = tape.int("hash");
        const SLOTS: usize = 7;
        let groups = 1usize << log_groups;
        let mut words = vec![0u64; groups];
        let mut keys = vec![u64::MAX; groups * SLOTS];
        let total = (groups * SLOTS) as u64 * fill / 8;
        for _ in 0..total {
            let key = tape.next_u64("key") >> 60;
            let key_hash = key.wrapping_mul(0x9e37_79b9_7f4a_7c15) ^ (key << 57);
            let at = vacant::<SLOTS>(key_hash, groups, |g| Ctrl(words[g]));
            words[at.group] = at.tagged(tag(key_hash)).0;
            keys[at.index()] = key;
        }
        let key = hash >> 60;
        let hash = key.wrapping_mul(0x9e37_79b9_7f4a_7c15) ^ (key << 57);
        let low = Ctrl::<SLOTS>::LOW;
        let want =
            reference::find(&words, SLOTS, hash, u64::from(tag(hash)), low, |i| keys[i] == key);
        let got = find::<SLOTS, usize>(
            hash,
            tag(hash),
            groups,
            |g| Ctrl(words[g]),
            |at| (keys[at.index()] == key).then_some(at.index()),
        );
        assert_eq!(got, want);
        if words.iter().any(|&w| Ctrl::<SLOTS>(w).empty().any()) {
            let stop = vacant::<SLOTS>(hash, groups, |g| Ctrl(words[g]));
            assert!(Ctrl::<SLOTS>(words[stop.group]).empty().any());
            assert_eq!(stop.ctrl.0, words[stop.group]);
        }
    }

    #[test]
    fn tags_are_full_and_live_tags_never_dead() {
        for top in 0..128u64 {
            assert_eq!(tag(top << 57), 0x80 | top as u8);
            assert!((0x81..=0xff).contains(&live_tag(top << 57)));
        }
    }

    /// The cases the copies tested, kept: every equal byte found among unequal and dead ones,
    /// the bytes past the slots never free, dead not free, and dead not live.
    #[test]
    fn matching_finds_every_equal_byte() {
        for tag in [0x80u8, 0x81, 0xfe, 0xff] {
            for slots in 0..=127u64 {
                let mut word = 0;
                let mut want = 0;
                for byte in 0..7 {
                    if slots & (1 << byte) != 0 {
                        word |= u64::from(tag) << (byte * 8);
                        want |= 0x80 << (byte * 8);
                    } else if byte % 3 == 0 {
                        word |= u64::from(tag ^ 1) << (byte * 8);
                    } else if byte % 3 == 1 && tag != DEAD {
                        word |= u64::from(DEAD) << (byte * 8);
                    }
                }
                assert_eq!(bits(Ctrl::<7>(word).matching(tag)), want, "{word:x} {tag:x}");
            }
        }
        assert_eq!(bits(Ctrl::<7>(0x0000_ff00_0081_0000).empty()), 0x0080_0080_8000_8080);
        assert_eq!(
            bits(Ctrl::<7>(0x00ff_ffff_ffff_ffff).empty()),
            0,
            "byte 7 is never a free slot"
        );
        assert_eq!(bits(Ctrl::<7>(0x80).empty()), 0x0080_8080_8080_8000, "dead is not free");
        assert_eq!(bits(Ctrl::<8>(0x0081_80ff_0000_8082).live()), 0x0080_0080_0000_0080);
        assert_eq!(bits(Ctrl::<8>(0x8080_8080_8080_8080).live()), 0);
    }

    #[test]
    fn live_skips_empty_and_dead() {
        for word in
            [0u64, 0x80, 0x0081_80ff_0000_8082, 0x00ff_ffff_ffff_ffff, 0x0080_8080_8080_8080]
        {
            let want: Vec<usize> = (0..7)
                .filter(|&slot| {
                    let byte = (word >> (slot * 8)) as u8;
                    byte & 0x80 != 0 && byte != DEAD
                })
                .collect();
            assert_eq!(Ctrl::<7>(word).live().collect::<Vec<_>>(), want, "{word:x}");
        }
    }

    #[test]
    fn with_sets_one_slot() {
        let word = Ctrl::<7>(0x0000_0000_0081_0000);
        assert_eq!(word.with(0, 0xaa).0, 0x0000_0000_0081_00aa);
        assert_eq!(word.with(2, DEAD).0, 0x0000_0000_0080_0000);
        assert_eq!(word.with(2, DEAD).tag_at(2), DEAD);
    }

    /// Few distinct hashes for many keys, some differing only in the tag or only in the home
    /// group, so tags and groups collide.
    fn hash_of(key: u64) -> u64 {
        match key % 3 {
            0 => key % 97,
            1 => ((key % 61) << 57) | 5,
            _ => key.wrapping_mul(0xff51_afd7_ed55_8ccd),
        }
    }

    /// The index against a `BTreeMap` numbering keys in first-seen order, over seeded keys:
    /// the same values found, every one kept across growths and `release_outgrown`.
    #[cfg(not(blaze_check))]
    #[test]
    fn index_agrees_with_a_map() {
        let (steps, distinct) = sized!((40_000, 9_000), (600, 150));
        for seed in 0..sized!(4u64, 1) {
            let mut index = Index::new_in(heap().shared());
            assert_eq!(index.capacity(), 0);
            let mut keys: Vec<u64> = Vec::new();
            let mut model: std::collections::BTreeMap<u64, u32> = std::collections::BTreeMap::new();
            let mut rng = blaze_rng::SplitMix64::new(seed);
            for step in 0..steps {
                let key = rng.below(distinct);
                let found = index.find(hash_of(key), |value| keys[value as usize] == key);
                assert_eq!(found, model.get(&key).copied(), "seed {seed} step {step}");
                if found.is_none() {
                    let value = keys.len() as u32;
                    keys.push(key);
                    index.insert(model.len(), hash_of(key), value, |value| {
                        hash_of(keys[value as usize])
                    });
                    model.insert(key, value);
                }
                if step % (steps / 4) == 0 {
                    index.release_outgrown();
                }
            }
            assert!(index.capacity() >= model.len());
            index.release_outgrown();
            for (&key, &value) in &model {
                assert_eq!(index.find(hash_of(key), |it| keys[it as usize] == key), Some(value));
            }
        }
    }

    /// `find_many` answers what `find` answers of each of its hashes, duplicates and absent
    /// keys among them, over indexes from empty to several levels grown, with colliding
    /// hashes; `ahead` is told only values the index holds.
    #[cfg(not(blaze_check))]
    #[test]
    fn find_many_is_find_of_each() {
        blaze_sim::check(Config::cases(CASES), |tape: &Tape| {
            let index = Index::new_in(heap().shared());
            let mut keys: Vec<u64> = Vec::new();
            let held = tape.below(sized!(400, 60), "held");
            for _ in 0..held {
                let key = tape.below(500, "key");
                if index.find(hash_of(key), |value| keys[value as usize] == key).is_none() {
                    let value = keys.len() as u32;
                    keys.push(key);
                    index.insert(keys.len() - 1, hash_of(key), value, |value| {
                        hash_of(keys[value as usize])
                    });
                }
            }
            for _ in 0..4 {
                let mut asked: [u64; 8] = core::array::from_fn(|_| tape.below(600, "asked"));
                // A duplicate in every batch: keys above 500 are never held.
                asked[7] = asked[tape.index(7, "duplicated")];
                let found = index.find_many(
                    asked.map(hash_of),
                    |value| assert!((value as usize) < keys.len(), "ahead of a held value"),
                    |at, value| keys[value as usize] == asked[at],
                );
                for (at, &key) in asked.iter().enumerate() {
                    let one = index.find(hash_of(key), |value| keys[value as usize] == key);
                    assert_eq!(found[at], one, "key {key} at {at} of {asked:?}");
                }
            }
        });
    }

    /// One writer at a time behind a lock, readers with none: every value a reader finds is
    /// the key's, and every key inserted before a reader starts is found.
    #[cfg(not(blaze_check))]
    #[test]
    #[expect(
        clippy::disallowed_types,
        reason = "below blaze-core and its named locks, with no dependencies: the test's one writer at a time, which `Index` asks of its caller, a test thread"
    )]
    fn readers_race_a_growing_index() {
        use std::sync::{Mutex, atomic::AtomicU64 as Key};

        const KEYS: usize = sized!(20_000, 400);
        const EARLY: u64 = sized!(100, 20);
        let index = Index::new_in(heap().shared());
        let column: Box<[Key]> = (0..KEYS).map(|_| Key::new(u64::MAX)).collect();
        let len = Mutex::new(0usize);
        let intern = |key: u64| -> u32 {
            let is = |value: u32| column[value as usize].load(Ordering::Relaxed) == key;
            if let Some(value) = index.find(hash_of(key), is) {
                return value;
            }
            let mut len = len.lock().expect("the test's writer lock: no writer panicked");
            if let Some(value) = index.find(hash_of(key), is) {
                return value;
            }
            let value = *len as u32;
            column[value as usize].store(key, Ordering::Relaxed);
            index.insert(*len, hash_of(key), value, |value| {
                hash_of(column[value as usize].load(Ordering::Relaxed))
            });
            *len += 1;
            value
        };
        let early: Vec<u32> = (0..EARLY).map(intern).collect();
        std::thread::scope(|scope| {
            for worker in 0..sized!(8u64, 3) {
                let (intern, early, column, index) = (&intern, &early, &column, &index);
                scope.spawn(move || {
                    for n in 0..KEYS as u64 / 2 {
                        let key = (n * 7 + worker * 131) % (KEYS as u64 / 2);
                        let value = intern(key);
                        assert_eq!(column[value as usize].load(Ordering::Relaxed), key);
                        let first = (n % EARLY) as usize;
                        let found = index.find(hash_of(first as u64), |it| {
                            column[it as usize].load(Ordering::Relaxed) == first as u64
                        });
                        assert_eq!(found, Some(early[first]));
                    }
                });
            }
        });
        assert_eq!(*len.lock().expect("the test's writer lock: no writer panicked"), KEYS / 2);
    }

    /// A probe reads one line: every group of every level starts on a 32-byte boundary, so its
    /// word and slots never cross a 64-byte line.
    #[cfg(not(blaze_check))]
    #[test]
    fn index_groups_never_straddle_a_line() {
        let index = Index::new_in(heap().shared());
        // Under Miri, enough for six levels: the sixth (32 groups) is made at the 73rd value.
        let keys: Vec<u64> = (0..sized!(5_000, 200)).collect();
        for (len, &key) in keys.iter().enumerate() {
            index.insert(len, hash_of(key), len as u32, |value| hash_of(keys[value as usize]));
        }
        let levels = index.level.load(Ordering::Relaxed);
        assert!(levels > 5);
        for table in index.tables[..levels].iter().filter_map(Once::get) {
            for group in table.groups.iter() {
                let at = std::ptr::from_ref(group).addr();
                assert_eq!(at / 64, (at + size_of::<Group>() - 1) / 64, "a group in one line");
            }
        }
    }

    #[cfg(not(blaze_check))]
    #[test]
    fn an_index_allocates_only_to_grow() {
        let index = blaze_alloc_count::assert_at_most(0, "a new index", || Index::new_in(heap().shared()));
        let keys: Vec<u64> = (0..sized!(1_000, 100)).collect();
        for (len, &key) in keys.iter().enumerate() {
            index.insert(len, hash_of(key), len as u32, |value| hash_of(keys[value as usize]));
        }
        blaze_alloc_count::assert_at_most(0, "an index's finds", || {
            for &key in &keys {
                assert_eq!(
                    index.find(hash_of(key), |it| keys[it as usize] == key),
                    Some(key as u32)
                );
            }
            assert_eq!(index.find(hash_of(5_000), |it| keys[it as usize] == 5_000), None);
        });
    }

    #[cfg(not(blaze_check))]
    #[test]
    fn a_probe_allocates_nothing() {
        let words = [0x0081_8283_8485_8687u64, 0x0000_0000_0000_0081];
        blaze_alloc_count::assert_at_most(0, "a probe", || {
            let found =
                find::<7, usize>(1 << 57, 0x81, 2, |g| Ctrl(words[g]), |at| Some(at.index()));
            let free = vacant::<7>(0, 2, |g| Ctrl(words[g]));
            (found, free)
        });
    }
}

/// Under the model checker (`RUSTFLAGS="--cfg blaze_check" blaze test -p blaze-swiss --release
/// model_swiss`): one writer filling an [`Index`]'s first table and growing it while a reader
/// finds a value from before the growth and one inserted during it.
#[cfg(all(test, blaze_check))]
mod model {
    use std::sync::Arc;

    use blaze_check::thread;
    use blaze_own::testing::heap;

    use super::*;
    use crate::sync::Spin;

    /// Every value is its own key, all of one hash: one home group and one tag, so every probe
    /// asks the caller and four values fill the first table (one group of six) to three
    /// quarters.
    const HASH: u64 = 3;

    #[test]
    fn model_swiss_reader_races_growth() {
        blaze_check::model(|| {
            let index = Arc::new(Index::new_in(heap().shared()));
            for value in 0..4u32 {
                index.insert(value as usize, HASH, value, |_| HASH);
            }
            assert_eq!(index.capacity(), 4);
            let reader = {
                let index = index.clone();
                thread::spawn(move || {
                    (index.find(HASH, |it| it == 0), index.find(HASH, |it| it == 4))
                })
            };
            index.insert(4, HASH, 4, |_| HASH);
            index.insert(5, HASH, 5, |_| HASH);
            let (zero, four) = reader.join().unwrap();
            assert_eq!(zero, Some(0), "a value from before the growth is found");
            assert!(four.is_none() || four == Some(4), "a value is found whole or not at all");
            for value in 0..6 {
                assert_eq!(
                    index.find(HASH, |it| it == value),
                    Some(value),
                    "kept across the growth"
                );
            }
        });
    }

    /// Values in the third table (four groups of six) at which it starts to grow: the
    /// nineteenth fills it past three quarters, and it takes up to twenty-one, seven eighths,
    /// while the next is built.
    const BEFORE_GROWTH: u32 = 18;

    /// The swap of a growth built outside the writer's lock: one thread starts the growth,
    /// builds the next level unlocked and publishes it under the lock, while another inserts
    /// under the lock at any point of that. [`IndexPlant::SkipsRecheck`] is the planted bug, a
    /// publish that does not re-check the log after the build: the other's value is lost in
    /// some schedule.
    fn swap_keeps_an_insert_during_the_build(plant: IndexPlant) {
        blaze_check::model(move || {
            let index = Arc::new(Index::new_in(heap().shared()));
            for value in 0..BEFORE_GROWTH {
                index.insert(value as usize, HASH, value, |_| HASH);
            }
            let len = Arc::new(Spin::new("swiss.test.writer", BEFORE_GROWTH as usize));
            let grow = {
                let mut len = len.lock();
                let vacancy = index.vacancy(*len).expect("room below seven eighths");
                let grow = vacancy.fill(HASH, BEFORE_GROWTH).expect("three quarters: a growth");
                *len += 1;
                grow
            };
            let other = {
                let (index, len) = (index.clone(), len.clone());
                thread::spawn(move || {
                    let mut len = len.lock();
                    let vacancy = index.vacancy(*len).expect("room below seven eighths");
                    assert!(vacancy.fill(HASH, BEFORE_GROWTH + 1).is_none(), "one growth");
                    *len += 1;
                })
            };
            let built = grow.build(|_| HASH);
            {
                let _len = len.lock();
                built.publish_planted(|_| HASH, plant);
            }
            other.join().unwrap();
            assert_eq!(index.level(), 4, "one growth published");
            for value in 0..BEFORE_GROWTH + 2 {
                assert_eq!(
                    index.find(HASH, |it| it == value),
                    Some(value),
                    "value {value} kept across the swap"
                );
            }
        });
    }

    #[test]
    fn model_swiss_swap_keeps_an_insert_during_the_build() {
        swap_keeps_an_insert_during_the_build(IndexPlant::None);
    }

    /// The control: without the re-check the model finds the lost insert.
    #[test]
    #[should_panic(expected = "kept across the swap")]
    fn model_swiss_swap_without_recheck_loses_an_insert() {
        swap_keeps_an_insert_during_the_build(IndexPlant::SkipsRecheck);
    }
}
