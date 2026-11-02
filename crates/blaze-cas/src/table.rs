//! An append-only open-addressed table that any number of threads read and insert into with no
//! lock (docs/v2/plan/design/concurrency.md "Reads").
//!
//! - A slot holds a pointer to an immutable value, null until a value is published into it
//!   with one compare-and-swap. A lookup is acquire loads and key compares: it writes nothing.
//! - A key never moves: a full level is not rehashed; the table adds a level twice as large,
//!   and lookups look through every level, oldest first.
//! - A key is in one level at most. An insert walks the levels oldest first too, and leaves a
//!   level it finds too full by sealing the empty slot that ends the key's run there: every
//!   inserter of the key meets that same slot, so one compare-and-swap decides for all of them
//!   whether the key lands in that level or every one of them goes on to the next.
//! - Each row (a slot, numbered across levels) has flag bits in words beside the slots, set
//!   with one relaxed `fetch_or` and only when not set already.
//!
//! Its memory is game-engine memory (docs/v2/plan/design/memory.md): the slots and flag words of
//! every level sit end to end on regions reserved for the most rows the table can have
//! (`words.rs`), and the values in slots of a third region, all tagged [`Tag::Store`]. Making a
//! level commits pages and an insert carves the next value slot with one `fetch_add`: a lookup,
//! an insert and a new level call no allocator, copy nothing and move nothing.
//!
//! Only the store (the one writer) swaps a published value for another of the same key
//! ([`Table::replace`]). The new value takes a slot the store freed before, and the old one stays
//! in its slot, readable, until a grace begun after the swap has passed: then
//! [`Table::collect`] drops it and keeps the slot for the next swap. So the warm loop of moves
//! (a commit pointing keys at their packs, a settle) reuses the same slots.

use std::{
    cell::UnsafeCell,
    marker::PhantomData,
    mem,
    ptr::{self, NonNull},
};

use blaze_ids::bits::AtomicBitSlice;
use blaze_mem::{MemError, Region, RegionVec, Tag};
use blaze_reclaim::{Grace, Guard};

use crate::{
    sync::{
        OnceLock,
        atomic::{AtomicBool, AtomicPtr, AtomicU64, AtomicUsize, Ordering},
    },
    words::{STRETCHES, Words, stop},
};

const LEVELS: usize = STRETCHES;

/// The rows a table reserves room for (unless its first level alone is more): 64 M slots of 8
/// bytes, so at most 32 M values, far past what a delta holds before an index takes it
/// (`INDEX_AFTER`) or what the values made whole hold before they are let go (`OWNED_MOST`).
/// A table that would grow past it stops, naming the store.
const ROWS_MOST: usize = 1 << 26;

/// What a sealed slot holds: an address no value has, never dereferenced.
static SEAL: u8 = 0;

fn seal<T>() -> *mut T {
    ptr::from_ref(&SEAL).cast_mut().cast()
}

/// What a probe of a level for a key met.
enum Probe<'a, T> {
    /// The key, at this slot.
    Found(usize, &'a T),
    /// The empty slot that ends the key's run: the key is not in this level yet.
    Empty(usize),
    /// A seal ending the key's run, or a level with no empty slot: the key is not in this level
    /// and never will be.
    Past,
}

/// A value of a table: immutable once published, found by its key.
pub(crate) trait Keyed: Send + Sync {
    type Key: Copy + Eq;
    fn key(&self) -> Self::Key;
    /// Where its probing starts: uniform over 64 bits.
    fn hash(key: &Self::Key) -> u64;
}

/// Proof that what a table gives out stays valid for `'a`: the reader is in a task
/// ([`Held::task`]), or it is the store borrowed shared ([`Held::writer`]).
///
/// A value unlinked from a table is dropped only by [`Table::collect`], which the store calls
/// under `&mut` of itself once a grace begun after the unlink has passed, and a table itself is
/// freed only by the store's limbo. So while a task that began before the grace runs, or while
/// the store is borrowed shared, nothing it found is freed or overwritten.
#[derive(Clone, Copy)]
pub(crate) struct Held<'a>(PhantomData<&'a ()>);

impl<'a> Held<'a> {
    pub(crate) fn task(_guard: &'a Guard) -> Held<'a> {
        Held(PhantomData)
    }

    /// For the store's own reads through `&'a Store`: only its `&mut` methods free anything.
    pub(crate) fn writer<T: ?Sized>(_store: &'a T) -> Held<'a> {
        Held(PhantomData)
    }
}

struct Level {
    used: AtomicUsize,
}

/// A table's values: slots of `T` in a region that never moves, carved in order by any inserter.
struct Values<T> {
    region: Region,
    /// Slots carved so far.
    carved: AtomicUsize,
    max: usize,
    _owns: PhantomData<T>,
}

impl<T> Values<T> {
    const SIZE: usize = {
        assert!(mem::size_of::<T>() != 0, "a table of a zero-sized value holds nothing");
        assert!(mem::align_of::<T>() <= 4096, "a table's values align to at most a page");
        mem::size_of::<T>()
    };

    fn new(max: usize) -> Values<T> {
        let bytes = max.checked_mul(Self::SIZE).unwrap_or_else(|| {
            stop(MemError::TooLarge { tag: Tag::Store, what: "a table's values", wanted: max })
        });
        match Region::reserve_huge(bytes, Tag::Store) {
            Ok(region) => Values { region, carved: AtomicUsize::new(0), max, _owns: PhantomData },
            Err(error) => stop(error),
        }
    }

    fn slot(&self, index: usize) -> NonNull<T> {
        debug_assert!(index < self.max);
        self.region.at(index * Self::SIZE).cast::<T>()
    }

    fn index_of(&self, value: *const T) -> u32 {
        let index = (value.addr() - self.region.base().as_ptr().addr()) / Self::SIZE;
        debug_assert!(index < self.max);
        index as u32
    }

    /// Writes `value` into a fresh slot, committed: from any thread. The value is the caller's
    /// until it publishes the slot.
    fn place(&self, value: T) -> *mut T {
        let index = self.carved.fetch_add(1, Ordering::Relaxed);
        if index >= self.max {
            self.carved.fetch_min(self.max, Ordering::Relaxed);
            stop(MemError::Exhausted {
                tag: Tag::Store,
                reserved: self.region.reserved(),
                wanted: (index + 1) * Self::SIZE,
            });
        }
        if let Err(error) = self.region.ensure((index + 1) * Self::SIZE) {
            stop(error)
        }
        let slot = self.slot(index);
        // SAFETY: the slot is committed (just ensured), inside the reservation (`index < max`),
        // aligned (a page-aligned base plus a multiple of `size_of::<T>()`), and claimed by this
        // thread alone (the `fetch_add`); no reader reaches it before the caller publishes it.
        unsafe { slot.write(value) };
        slot.as_ptr()
    }
}

/// What only the store reads and writes: the slots it freed, and the values it swapped out, each
/// with the grace begun after its swap.
struct Recycle {
    /// Values swapped out, oldest grace first: `held[head..]` still hold their values.
    held: RegionVec<(Grace, u32)>,
    head: usize,
    /// Slots whose values were dropped, to take the next swapped-in values.
    free: RegionVec<u32>,
}

/// The recycling state, behind a flag that stops a second caller at once instead of racing:
/// only the store calls the methods that reach it, one at a time.
struct Owner {
    busy: AtomicBool,
    recycle: UnsafeCell<Recycle>,
}

impl Owner {
    fn new(max: usize) -> Owner {
        let made = RegionVec::new(max, Tag::Store).and_then(|held| Ok((held, RegionVec::new(max, Tag::Store)?)));
        match made {
            Ok((held, free)) => {
                Owner { busy: AtomicBool::new(false), recycle: UnsafeCell::new(Recycle { held, head: 0, free }) }
            }
            Err(error) => stop(error),
        }
    }

    #[track_caller]
    fn with<R>(&self, f: impl FnOnce(&mut Recycle) -> R) -> R {
        assert!(
            !self.busy.swap(true, Ordering::Acquire),
            "two threads swapped values of one table at once; only the store does, one call at a time"
        );
        struct Done<'a>(&'a AtomicBool);
        impl Drop for Done<'_> {
            fn drop(&mut self) {
                self.0.store(false, Ordering::Release);
            }
        }
        let _done = Done(&self.busy);
        // SAFETY: `busy` was clear and stays set until `_done` drops, so this is the one
        // reference to the recycling state; the acquire swap orders it after the last call's
        // release store.
        f(unsafe { &mut *self.recycle.get() })
    }
}

pub(crate) struct Table<T: Keyed> {
    levels: [OnceLock<Level>; LEVELS],
    /// Levels it may make: as many as fit in `ROWS_MOST` rows, at least one.
    most: usize,
    /// Slots of level 0, a power of two; level `n` has `first << n`.
    first: usize,
    columns: usize,
    /// Every level's slots, level after level: level `n`'s start at row `base(n)`.
    slots: Words<AtomicPtr<T>>,
    /// `columns` words a 64 rows, level after level, each level's column by column.
    bits: Words<AtomicU64>,
    /// What its slots point to: it owns them.
    values: Values<T>,
    owner: Owner,
}

// SAFETY: the recycling state is reached only inside `Owner::with`, one call at a time (its
// flag); the rest is atomics, regions and values handed out as `&T`, and `Keyed` values are
// `Send + Sync`, so they may be read, written and dropped from any thread.
unsafe impl<T: Keyed> Sync for Table<T> {}

/// Where a value is: its row, and the value.
pub(crate) type Hit<'a, T> = (u64, &'a T);

/// Walks `key`'s run in a level's `slots` to the key, or to the null or seal that ends the run.
fn probe<'a, T: Keyed>(slots: &'a [AtomicPtr<T>], key: &T::Key) -> Probe<'a, T> {
    let mask = slots.len() - 1;
    let mut at = T::hash(key) as usize & mask;
    for _ in 0..slots.len() {
        let found = slots[at].load(Ordering::Acquire);
        if found.is_null() {
            return Probe::Empty(at);
        }
        if found == seal() {
            return Probe::Past;
        }
        // SAFETY: a published value is dropped or overwritten only as `Held` says, and the
        // caller holds one for the lifetime it gives out.
        let found = unsafe { &*found };
        if found.key() == *key {
            return Probe::Found(at, found);
        }
        at = (at + 1) & mask;
    }
    Probe::Past
}

impl<T: Keyed> Table<T> {
    /// A table whose first level has `first` slots (rounded up to a power of two, at least 64)
    /// and `columns` flag bits a row.
    pub(crate) fn new(first: usize, columns: usize) -> Table<T> {
        let first = first.next_power_of_two().max(64);
        let mut most = 1;
        while most < LEVELS && first.checked_mul((1 << (most + 1)) - 1).is_some_and(|rows| rows <= ROWS_MOST) {
            most += 1;
        }
        let rows = first * ((1 << most) - 1);
        let table = Table {
            levels: [const { OnceLock::new() }; LEVELS],
            most,
            first,
            columns,
            slots: Words::new(rows),
            bits: Words::new(columns * rows / 64),
            // At most half of every level is values, and as many again swapped out awaiting
            // their grace or free for the next swap.
            values: Values::new(rows),
            owner: Owner::new(rows),
        };
        table.level(0);
        table
    }

    fn cap(&self, level: usize) -> usize {
        self.first << level
    }

    /// The row of level `level`'s first slot.
    fn base(&self, level: usize) -> u64 {
        (self.first as u64) * ((1u64 << level) - 1)
    }

    fn level(&self, level: usize) -> &Level {
        if level >= self.most {
            stop(MemError::Exhausted {
                tag: Tag::Store,
                reserved: self.base(self.most) as usize * mem::size_of::<AtomicPtr<T>>(),
                wanted: (self.base(level) as usize + self.cap(level)) * mem::size_of::<AtomicPtr<T>>(),
            });
        }
        self.levels[level].get_or_init(|| {
            let (base, cap) = (self.base(level) as usize, self.cap(level));
            self.slots.make(level, base, cap);
            self.bits.make(level, self.columns * base / 64, self.columns * cap / 64);
            Level { used: AtomicUsize::new(0) }
        })
    }

    /// Level `level`'s slots: it is made.
    fn slots_of(&self, level: usize) -> &[AtomicPtr<T>] {
        self.slots.get(level, self.base(level) as usize, self.cap(level))
    }

    /// Level `level`'s flag words: it is made.
    fn bits_of(&self, level: usize) -> &[AtomicU64] {
        let (base, cap) = (self.base(level) as usize, self.cap(level));
        self.bits.get(level, self.columns * base / 64, self.columns * cap / 64)
    }

    fn made(&self) -> impl Iterator<Item = (usize, &Level)> {
        self.levels.iter().enumerate().map_while(|(at, level)| level.get().map(|it| (at, it)))
    }

    pub(crate) fn get<'a>(&'a self, _held: Held<'a>, key: &T::Key) -> Option<Hit<'a, T>> {
        for (number, _) in self.made() {
            if let Probe::Found(at, found) = probe(self.slots_of(number), key) {
                return Some((self.base(number) + at as u64, found));
            }
        }
        None
    }

    /// Publishes `value` unless its key is there already: then it gives `value` back with
    /// what is there. Of inserts of one key racing, exactly one lands. Any thread.
    pub(crate) fn insert<'a>(&'a self, _held: Held<'a>, value: T) -> Result<Hit<'a, T>, (T, Hit<'a, T>)> {
        let key = value.key();
        // Placed in a slot of its own once there is an empty slot to publish it in.
        let mut value = Some(value);
        let mut raw: *mut T = ptr::null_mut();
        let mut number = 0;
        loop {
            let level = self.level(number);
            let slots = self.slots_of(number);
            let at = match probe(slots, &key) {
                Probe::Found(at, found) => {
                    let value = match value.take() {
                        Some(value) => value,
                        // SAFETY: `raw` was placed below and never published, so it holds the
                        // value, read out once here; its slot is left empty for good.
                        None => unsafe { raw.read() },
                    };
                    return Err((value, (self.base(number) + at as u64, found)));
                }
                Probe::Past => {
                    number += 1;
                    continue;
                }
                Probe::Empty(at) => at,
            };
            // A level half full takes no more: probes stay short. Its runs are sealed rather
            // than left open, so no inserter of this key can land here after this one moved on.
            let full = level.used.load(Ordering::Relaxed) * 2 >= slots.len();
            let put = match (full, value.take()) {
                (true, value_back) => {
                    value = value_back;
                    seal()
                }
                (false, Some(value)) => {
                    raw = self.values.place(value);
                    raw
                }
                (false, None) => raw,
            };
            match slots[at].compare_exchange(ptr::null_mut(), put, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) if full => number += 1,
                Ok(_) => {
                    level.used.fetch_add(1, Ordering::Relaxed);
                    // SAFETY: published just now; dropped or overwritten only as `Held` says.
                    return Ok((self.base(number) + at as u64, unsafe { &*raw }));
                }
                // Another value or a seal took the slot: probing the level again sees which.
                Err(_) => {}
            }
        }
    }

    /// Swaps the value of `value`'s key for `value`, keeping its row, and gives back the old
    /// value, which stays readable until a grace begun after the swap has passed and
    /// [`Table::collect`] after that drops it. Gives `value` back when the key is not there.
    /// Only the store calls it, one call at a time.
    pub(crate) fn replace<'a>(&'a self, _held: Held<'a>, value: T) -> Result<&'a T, T> {
        let key = value.key();
        let found = self.made().find_map(|(number, _)| match probe(self.slots_of(number), &key) {
            Probe::Found(at, _) => Some((number, at)),
            _ => None,
        });
        let Some((number, at)) = found else { return Err(value) };
        self.owner.with(|recycle| {
            let raw = match recycle.free.pop() {
                Some(index) => {
                    let slot = self.values.slot(index as usize);
                    // SAFETY: a free slot's value was dropped once its grace had passed, so no
                    // reader can reach it, and only the store (this call) writes it.
                    unsafe { slot.write(value) };
                    slot.as_ptr()
                }
                None => self.values.place(value),
            };
            // A slot holding a value is only ever swapped for another placed value, never cleared
            // or sealed, so `old` is a value of this table: the swap unlinked it.
            let old = self.slots_of(number)[at].swap(raw, Ordering::AcqRel);
            let grace = Grace::start();
            if recycle.head == recycle.held.len() {
                recycle.held.clear();
                recycle.head = 0;
            } else if recycle.head > 0
                && (recycle.head * 2 >= recycle.held.len() || recycle.held.len() == recycle.held.max_len())
            {
                let live = recycle.held.len() - recycle.head;
                recycle.held.copy_within(recycle.head.., 0);
                recycle.held.truncate(live);
                recycle.head = 0;
            }
            recycle.held.push((grace, self.values.index_of(old)));
            // SAFETY: unlinked just now, it stays in its slot until a collect after its grace,
            // which the store runs only under `&mut` of itself, so not while `Held` lives.
            Ok(unsafe { &*old })
        })
    }

    /// Drops the values swapped out whose grace has passed and keeps their slots for the next
    /// swaps; says how many. Only the store calls it, one call at a time.
    pub(crate) fn collect(&self) -> usize {
        self.owner.with(|recycle| {
            let passed = recycle.held[recycle.head..].partition_point(|(grace, _)| grace.passed());
            for at in recycle.head..recycle.head + passed {
                let index = recycle.held[at].1;
                // SAFETY: the slot holds the value swapped out of it, which no reader can reach
                // once its grace has passed; the head moves past it below, so it is dropped once.
                unsafe { self.values.slot(index as usize).drop_in_place() };
                recycle.free.push(index);
            }
            recycle.head += passed;
            passed
        })
    }

    /// Values swapped out whose grace has not yet been collected.
    pub(crate) fn retired(&self) -> usize {
        self.owner.with(|recycle| recycle.held.len() - recycle.head)
    }

    /// Every value with its row.
    pub(crate) fn iter<'a>(&'a self, _held: Held<'a>) -> impl Iterator<Item = Hit<'a, T>> + 'a {
        self.made().flat_map(move |(number, _)| {
            let base = self.base(number);
            self.slots_of(number).iter().enumerate().filter_map(move |(at, slot)| {
                let found = slot.load(Ordering::Acquire);
                // SAFETY: as in `probe`.
                (!found.is_null() && found != seal()).then(|| (base + at as u64, unsafe { &*found }))
            })
        })
    }

    pub(crate) fn len(&self) -> usize {
        self.made().map(|(_, level)| level.used.load(Ordering::Relaxed)).sum()
    }

    /// Column `column` of level `level`'s flags: it is made.
    fn flag_column(&self, level: usize, column: usize) -> AtomicBitSlice<'_> {
        let words = self.cap(level) / 64;
        AtomicBitSlice::from_words(&self.bits_of(level)[column * words..(column + 1) * words])
    }

    /// `column` of `row`'s level and the row's bit in it; `None` past the rows made.
    fn flag_bit(&self, row: u64, column: usize) -> Option<(AtomicBitSlice<'_>, usize)> {
        debug_assert!(column < self.columns);
        let (mut number, mut at) = (0, row);
        while at >= self.cap(number) as u64 {
            at -= self.cap(number) as u64;
            number += 1;
            if number >= self.most {
                return None;
            }
        }
        self.levels[number].get()?;
        Some((self.flag_column(number, column), at as usize))
    }

    pub(crate) fn flag(&self, row: u64, column: usize) -> bool {
        self.flag_bit(row, column).is_some_and(|(bits, at)| bits.contains(at, Ordering::Relaxed))
    }

    /// Sets a flag: a hint about bytes that never change at a place, so relaxed.
    pub(crate) fn set_flag(&self, row: u64, column: usize) {
        if let Some((bits, at)) = self.flag_bit(row, column) {
            bits.insert_if_clear(at, Ordering::Relaxed);
        }
    }

    /// Moves column `from` into column `to` and clears `from`: a new period of reading.
    /// Only the store calls it; a flag set meanwhile lands in one period or the other.
    pub(crate) fn rotate(&self, from: usize, to: usize) {
        for (number, _) in self.made() {
            self.flag_column(number, from).move_into(self.flag_column(number, to), Ordering::Relaxed);
        }
    }
}

impl<T: Keyed> Drop for Table<T> {
    fn drop(&mut self) {
        if !mem::needs_drop::<T>() {
            return;
        }
        for (number, _) in self.made() {
            for slot in self.slots_of(number) {
                let value = slot.load(Ordering::Relaxed);
                if !value.is_null() && value != seal() {
                    // SAFETY: the table owns what its slots point to, each a placed value in one
                    // slot only, and nothing reads a table being dropped.
                    unsafe { value.drop_in_place() };
                }
            }
        }
        let recycle = self.owner.recycle.get_mut();
        for at in recycle.head..recycle.held.len() {
            let index = recycle.held[at].1;
            // SAFETY: a value swapped out and not yet collected, so still in its slot and
            // dropped nowhere else; nothing reads a table being dropped.
            unsafe { self.values.slot(index as usize).drop_in_place() };
        }
    }
}

#[cfg(test)]
#[cfg(not(blaze_check))]
mod tests {
    use blaze_hash::HashMap;

    use super::*;

    #[derive(Debug)]
    struct Value {
        key: u64,
        n: u64,
    }

    impl Keyed for Value {
        type Key = u64;
        fn key(&self) -> u64 {
            self.key
        }
        fn hash(key: &u64) -> u64 {
            // Clustered on purpose: neighbours probe the same run.
            *key / 3
        }
    }

    fn value(key: u64, n: u64) -> Value {
        Value { key, n }
    }

    #[test]
    fn inserts_are_found_across_levels_and_a_second_of_a_key_is_refused() {
        let table: Table<Value> = Table::new(64, 2);
        let held = Held::writer(&table);
        for key in 0..10_000 {
            assert!(table.insert(held, value(key, key * 2)).is_ok());
        }
        assert!(table.made().count() > 1, "it grew");
        for key in 0..10_000 {
            let (_, found) = table.get(held, &key).unwrap();
            assert_eq!(found.n, key * 2);
        }
        let (back, (_, kept)) = table.insert(held, value(7, 0)).unwrap_err();
        assert_eq!((back.n, kept.n), (0, 14));
        assert!(table.get(held, &10_001).is_none());
        assert_eq!(table.len(), 10_000);
        assert_eq!(table.iter(held).count(), 10_000);
    }

    #[test]
    fn a_replace_keeps_the_row_and_the_old_value_until_it_is_collected() {
        let table: Table<Value> = Table::new(64, 1);
        let held = Held::writer(&table);
        let (row, first) = table.insert(held, value(5, 1)).unwrap();
        let old = table.replace(held, value(5, 2)).ok().unwrap();
        assert_eq!((first.n, old.n), (1, 1), "the old value is still whole");
        assert_eq!(table.retired(), 1);
        let (again, found) = table.get(held, &5).unwrap();
        assert_eq!((again, found.n), (row, 2));
        assert!(table.replace(held, value(6, 0)).is_err());
    }

    /// The table against a `HashMap` model across many levels, every commit step of its regions
    /// and every kind of operation: inserts of new and present keys, replaces of present and
    /// absent keys, collects, lookups, flags and rotations give the answers the model gives.
    #[test]
    fn the_table_answers_as_a_map_does_across_growth() {
        let table: Table<Value> = Table::new(64, 2);
        let held = Held::writer(&table);
        let mut model: HashMap<u64, u64> = HashMap::default();
        let mut rows: HashMap<u64, u64> = HashMap::default();
        let mut flags: HashMap<u64, [bool; 2]> = HashMap::default();
        let mut rng = blaze_ids::rng::SplitMix64::new(11);
        for step in 0..200_000u64 {
            let key = rng.next_u64() % 60_000;
            match rng.next_u64() % 8 {
                0..=3 => match (table.insert(held, value(key, step)), model.get(&key)) {
                    (Ok((row, found)), None) => {
                        assert_eq!(found.n, step);
                        model.insert(key, step);
                        rows.insert(key, row);
                    }
                    (Err((back, (row, kept))), Some(&n)) => {
                        assert_eq!((back.n, kept.n, Some(&row)), (step, n, rows.get(&key)));
                    }
                    (got, want) => panic!("step {step}: insert of {key} was {:?}, the model has {want:?}", got.is_ok()),
                },
                4 => match (table.replace(held, value(key, step)), model.get_mut(&key)) {
                    (Ok(old), Some(n)) => {
                        assert_eq!(old.n, *n, "step {step}: the value swapped out");
                        *n = step;
                    }
                    (Err(back), None) => assert_eq!(back.n, step),
                    (got, want) => panic!("step {step}: replace of {key} was {:?}, the model has {want:?}", got.is_ok()),
                },
                5 => {
                    if let Some(&row) = rows.get(&key) {
                        let column = (step % 2) as usize;
                        table.set_flag(row, column);
                        flags.entry(key).or_default()[column] = true;
                    }
                }
                6 if step % 1000 == 6 => {
                    table.rotate(0, 1);
                    for it in flags.values_mut() {
                        *it = [false, it[0]];
                    }
                }
                _ => {
                    let got = table.get(held, &key).map(|(row, it)| (row, it.n));
                    assert_eq!(got, model.get(&key).map(|n| (rows[&key], *n)), "step {step}: {key}");
                }
            }
            if step % 4096 == 0 {
                // Nothing is in a task here, so every grace passes.
                table.collect();
            }
        }
        assert!(table.made().count() >= 8, "it grew past several levels: {}", table.made().count());
        assert_eq!(table.len(), model.len());
        let mut seen: Vec<(u64, u64)> = table.iter(held).map(|(_, it)| (it.key, it.n)).collect();
        let mut want: Vec<(u64, u64)> = model.iter().map(|(k, n)| (*k, *n)).collect();
        seen.sort_unstable();
        want.sort_unstable();
        assert_eq!(seen, want);
        for (key, row) in &rows {
            let want = flags.get(key).copied().unwrap_or_default();
            assert_eq!([table.flag(*row, 0), table.flag(*row, 1)], want, "flags of {key}");
        }
    }

    /// Swapped-out values are dropped only by a collect after their grace, each once, and their
    /// slots take the next swaps: a warm loop of replaces carves no new slot.
    #[test]
    fn collected_slots_take_the_next_swaps() {
        use std::sync::{Arc, atomic::AtomicUsize as Count};

        struct Counted {
            key: u64,
            drops: Arc<Count>,
        }
        impl Keyed for Counted {
            type Key = u64;
            fn key(&self) -> u64 {
                self.key
            }
            fn hash(key: &u64) -> u64 {
                *key
            }
        }
        impl Drop for Counted {
            fn drop(&mut self) {
                self.drops.fetch_add(1, Ordering::Relaxed);
            }
        }
        let drops = Arc::new(Count::new(0));
        let counted = |key| Counted { key, drops: drops.clone() };
        let table: Table<Counted> = Table::new(64, 0);
        let held = Held::writer(&table);
        for key in 0..20 {
            assert!(table.insert(held, counted(key)).is_ok());
        }
        let guard = blaze_reclaim::enter();
        for key in 0..20 {
            assert!(table.replace(held, counted(key)).is_ok());
        }
        assert_eq!(table.collect(), 0, "a task that began before the swaps holds them");
        assert_eq!(drops.load(Ordering::Relaxed), 0);
        drop(guard);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while table.retired() > 0 {
            table.collect();
            assert!(std::time::Instant::now() < deadline, "a grace never passed");
            std::thread::yield_now();
        }
        assert_eq!(drops.load(Ordering::Relaxed), 20, "each swapped-out value dropped once");
        let carved = table.values.carved.load(Ordering::Relaxed);
        for round in 0..10 {
            for key in 0..20 {
                assert!(table.replace(held, counted(key)).is_ok());
            }
            while table.retired() > 0 {
                table.collect();
                std::thread::yield_now();
            }
            assert_eq!(table.values.carved.load(Ordering::Relaxed), carved, "round {round} carved a slot");
        }
        drop(table);
        assert_eq!(drops.load(Ordering::Relaxed), 20 + 10 * 20 + 20, "every value dropped once");
    }

    #[test]
    fn flags_are_per_row_and_rotate() {
        let table: Table<Value> = Table::new(64, 2);
        let held = Held::writer(&table);
        for key in 0..300 {
            table.insert(held, value(key, 0)).unwrap();
        }
        let rows: Vec<u64> = (0..300).map(|key| table.get(held, &key).unwrap().0).collect();
        table.set_flag(rows[3], 0);
        table.set_flag(rows[299], 0);
        assert!(table.flag(rows[3], 0) && table.flag(rows[299], 0));
        assert!(!table.flag(rows[4], 0) && !table.flag(rows[3], 1));
        table.rotate(0, 1);
        assert!(!table.flag(rows[3], 0) && table.flag(rows[3], 1) && table.flag(rows[299], 1));
        assert!(!table.flag(1 << 50, 0), "past the rows made is clear");
    }

    /// A lookup that hits allocates nothing, nor does an insert, whether it lands in a level made
    /// already or makes the next one past the table's high-water mark, nor a warm replace.
    #[test]
    fn hits_inserts_new_levels_and_warm_replaces_allocate_nothing() {
        let table: Table<Value> = Table::new(64, 2);
        let held = Held::writer(&table);
        blaze_alloc_count::assert_at_most(0, "inserts across new levels", || {
            for key in 0..50_000 {
                assert!(table.insert(held, value(key, key)).is_ok());
            }
        });
        assert!(table.made().count() > 6, "the inserts made new levels");
        blaze_alloc_count::assert_at_most(0, "hits", || {
            for key in 0..50_000 {
                assert_eq!(table.get(held, &key).map(|(_, it)| it.n), Some(key));
            }
        });
        // The recycling lists' first commit happens on the first swap: warm them.
        for key in 0..64 {
            assert!(table.replace(held, value(key, 0)).is_ok());
        }
        while table.retired() > 0 {
            table.collect();
            std::thread::yield_now();
        }
        blaze_alloc_count::assert_at_most(0, "warm replaces and collects", || {
            for key in 0..64 {
                assert!(table.replace(held, value(key, 1)).is_ok());
            }
            table.collect();
        });
    }

    /// Threads insert overlapping keys at once: each key ends with one value, every insert
    /// that lost found the winner, and every reader during it saw only whole values.
    #[test]
    fn racing_inserts_keep_one_value_a_key() {
        use std::{sync::Arc, thread};

        let table: Arc<Table<Value>> = Arc::new(Table::new(64, 0));
        let threads: Vec<_> = (0..8u64)
            .map(|t| {
                let table = table.clone();
                thread::spawn(move || {
                    let guard = blaze_reclaim::enter();
                    let held = Held::task(&guard);
                    let (mut won, mut lost) = (0, 0);
                    for key in 0..20_000u64 {
                        let key = (key * 7 + t * 3) % 20_000;
                        match table.insert(held, value(key, key + 1)) {
                            Ok(_) => won += 1,
                            Err((_, (_, found))) => {
                                assert_eq!(found.n, key + 1);
                                lost += 1;
                            }
                        }
                        if let Some((_, found)) = table.get(held, &((key + 1) % 20_000)) {
                            assert_eq!(found.n, (key + 1) % 20_000 + 1);
                        }
                    }
                    (won, lost)
                })
            })
            .collect();
        let won: u64 = threads.into_iter().map(|it| it.join().unwrap().0).sum();
        let held = Held::writer(&*table);
        for key in 0..20_000 {
            assert_eq!(table.get(held, &key).map(|(_, it)| it.n), Some(key + 1));
        }
        assert_eq!(won, 20_000, "one insert of each key lands");
        assert_eq!(table.len(), 20_000);
        assert_eq!(table.iter(held).count(), 20_000);
    }
}
