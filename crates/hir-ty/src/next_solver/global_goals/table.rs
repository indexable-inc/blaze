//! The global-goal table: [`GoalKey`] to a `Copy` value (for the solver, an
//! [`Answer`](super::Answer) and the reads it depends on), read without a lock or a write to
//! shared memory.
//!
//! A table is up to [`LEVELS`] open-addressed levels of 64-byte slots, the first of
//! `1 << FIRST_BITS` slots and each next four times larger, allocated on the first insert that
//! finds the older ones half full. A slot's word is 0 while empty, the key with
//! [`GoalKey::PENDING`] while its one writer fills the entry, and the key once the entry is
//! published with a release store. A reader loads the word with acquire, so a published key's
//! entry is complete for it; a pending slot is a miss. Levels are never moved or freed before
//! the table is dropped, so a hit is one hash, a few loads and a copy of the value.
//!
//! Two writers of one key in one level meet at its slot, since both walk the same probe
//! sequence and claim the first empty slot with a compare-exchange; the loser sees the key
//! and stops. Two writers of one key may still land in two levels when one of them finds the
//! older level full: both entries are the answer of one goal in one revision, so either is
//! right for a reader.

use std::{
    alloc::{self, Layout},
    cell::UnsafeCell,
    mem::MaybeUninit,
    ptr,
    sync::atomic::{AtomicPtr, AtomicU64, AtomicUsize, Ordering},
};

use super::GoalKey;

const FIRST_BITS: u32 = 12;
const GROWTH_BITS: u32 = 2;
const LEVELS: usize = 5;

#[repr(C, align(64))]
struct Slot<V> {
    word: AtomicU64,
    entry: UnsafeCell<MaybeUninit<V>>,
}

/// A slot's size for values of type `V`: one cache line while `V` is at most 56 bytes.
pub(super) const fn slot_size<V>() -> usize {
    size_of::<Slot<V>>()
}

pub(crate) struct Table<V> {
    levels: [AtomicPtr<Slot<V>>; LEVELS],
    /// Slots claimed in each level; a level takes no claim past half its slots, so every
    /// probe sequence meets an empty slot.
    claimed: [AtomicUsize; LEVELS],
}

// SAFETY: a `Table` owns its levels and the `V`s in them. A slot's entry is written only by
// the thread that won its claim, before the release store that publishes it, and read only
// after an acquire load that saw it published; it is never written again.
unsafe impl<V: Copy + Send> Send for Table<V> {}
// SAFETY: as for `Send`: every shared access to an entry is ordered after its one write, and
// a reader gets a copy of the `V`, so `V: Send` is all that crosses threads.
unsafe impl<V: Copy + Send> Sync for Table<V> {}

impl<V> Default for Table<V> {
    fn default() -> Self {
        Table {
            levels: [const { AtomicPtr::new(ptr::null_mut()) }; LEVELS],
            claimed: [const { AtomicUsize::new(0) }; LEVELS],
        }
    }
}

#[inline]
fn level_bits(level: usize) -> u32 {
    FIRST_BITS + GROWTH_BITS * level as u32
}

#[inline]
fn layout<V>(level: usize) -> Layout {
    Layout::array::<Slot<V>>(1 << level_bits(level)).expect("a level is at most 64 MiB of slots")
}

#[inline]
fn start(key: GoalKey, bits: u32) -> usize {
    (key.bits().wrapping_mul(0x9E37_79B9_7F4A_7C15) >> (64 - bits)) as usize
}

enum Probe<V> {
    Published(V),
    Pending,
    Absent,
}

impl<V: Copy> Table<V> {
    #[inline]
    pub(crate) fn get(&self, key: GoalKey) -> Option<V> {
        for level in 0..LEVELS {
            let base = self.levels[level].load(Ordering::Acquire);
            if base.is_null() {
                return None;
            }
            match Self::probe(base, level, key) {
                Probe::Published(value) => return Some(value),
                Probe::Pending => return None,
                Probe::Absent => {}
            }
        }
        None
    }

    /// Keeps `value` for `key` unless the table already has the key or is full.
    pub(crate) fn insert(&self, key: GoalKey, value: V) {
        for level in 0..LEVELS {
            let base = self.levels[level].load(Ordering::Acquire);
            if base.is_null() {
                break;
            }
            if !matches!(Self::probe(base, level, key), Probe::Absent) {
                return;
            }
        }
        for level in 0..LEVELS {
            let half = 1usize << (level_bits(level) - 1);
            if self.claimed[level].fetch_add(1, Ordering::Relaxed) >= half {
                self.claimed[level].fetch_sub(1, Ordering::Relaxed);
                continue;
            }
            let base = self.level(level);
            if !Self::claim(base, level, key, value) {
                self.claimed[level].fetch_sub(1, Ordering::Relaxed);
            }
            return;
        }
    }

    #[inline]
    fn probe(base: *mut Slot<V>, level: usize, key: GoalKey) -> Probe<V> {
        let bits = level_bits(level);
        let mask = (1usize << bits) - 1;
        let mut at = start(key, bits);
        loop {
            // SAFETY: `base` is a live level of `1 << bits` slots and `at` is masked to it.
            let slot = unsafe { &*base.add(at) };
            let word = slot.word.load(Ordering::Acquire);
            if word == key.bits() {
                // SAFETY: the acquire load saw the key published, after its writer's
                // initialization of the entry; the entry is not written again.
                let value = unsafe { (*slot.entry.get()).assume_init() };
                return Probe::Published(value);
            }
            if word == key.bits() | GoalKey::PENDING {
                return Probe::Pending;
            }
            if word == 0 {
                return Probe::Absent;
            }
            at = (at + 1) & mask;
        }
    }

    /// Claims the first empty slot on `key`'s probe sequence and publishes `value` in it;
    /// `false` when the key was already there.
    fn claim(base: *mut Slot<V>, level: usize, key: GoalKey, value: V) -> bool {
        let bits = level_bits(level);
        let mask = (1usize << bits) - 1;
        let mut at = start(key, bits);
        loop {
            // SAFETY: as in `probe`.
            let slot = unsafe { &*base.add(at) };
            match slot.word.compare_exchange(
                0,
                key.bits() | GoalKey::PENDING,
                Ordering::Acquire,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    // SAFETY: winning the claim makes this thread the entry's only writer,
                    // and no reader reads it before the release store below.
                    unsafe { (*slot.entry.get()).write(value) };
                    slot.word.store(key.bits(), Ordering::Release);
                    return true;
                }
                Err(word) if word & !GoalKey::PENDING == key.bits() => return false,
                Err(_) => at = (at + 1) & mask,
            }
        }
    }

    /// The level's slots, allocated zeroed (every word empty) by the first thread to need it.
    fn level(&self, level: usize) -> *mut Slot<V> {
        let current = self.levels[level].load(Ordering::Acquire);
        if !current.is_null() {
            return current;
        }
        let layout = layout::<V>(level);
        // SAFETY: `layout` has a non-zero size. A zero word is an empty slot, and an entry is
        // read only once its slot is published.
        // alloc: one level, at most five per table generation, on an insert past half full.
        let fresh = unsafe { alloc::alloc_zeroed(layout) }.cast::<Slot<V>>();
        if fresh.is_null() {
            alloc::handle_alloc_error(layout);
        }
        match self.levels[level].compare_exchange(
            ptr::null_mut(),
            fresh,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => fresh,
            Err(won) => {
                // SAFETY: `fresh` came from `alloc_zeroed(layout)` and was never shared.
                unsafe { alloc::dealloc(fresh.cast(), layout) };
                won
            }
        }
    }
}

impl<V> Drop for Table<V> {
    fn drop(&mut self) {
        for (level, base) in self.levels.iter_mut().enumerate() {
            let base = *base.get_mut();
            if !base.is_null() {
                // SAFETY: `base` came from `alloc_zeroed(layout(level))` in `Table::level`,
                // and `&mut self` means no reader holds it. Values come only
                // through `insert`, which takes `V: Copy`: none needs a drop.
                unsafe { alloc::dealloc(base.cast(), layout::<V>(level)) };
            }
        }
    }
}
