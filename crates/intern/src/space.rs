//! The reserved region every interner of the process keeps its values and probe tables in
//! (docs/v2/plan/design/memory.md, "tables, interners and columns"): 64 GiB of address space
//! reserved on first use, committed as it fills, never moved, reused or freed, its pages
//! counted under [`Tag::Intern`] and backed by huge pages where the kernel offers them.
//!
//! Space is claimed with one `fetch_add` on a cursor and one [`Region::ensure`]: a shard's bump
//! allocator claims a chunk at a time (2 MiB once warm), a probe table all its groups at once.
//! The kernel hands out committed pages zeroed and no byte is claimed twice, so claimed space
//! reads as zeroes: a probe table needs no initializing.
//!
//! Cost: a claim is one `fetch_add`, one acquire load of the committed length and, a quarter of
//! growth at a time, a commit system call.
//!
//! One of `intern`'s two unsafe modules (the other is [`crate::typed`]), with a safe API:
//!
//! - [`zeroed`] reads fresh pages as a table of [`Zeroed`] types in place. The safe version
//!   writes every group and hash of a probe table once more each time it doubles (64 bytes a
//!   group, as many as the copy into it), and faults in its pages before the first probe.
//! - [`place`] is `Box::leak` into the region, without the global allocator.
//! - [`Published`] hands out the `&'static` it was given, from an atomic pointer: the safe
//!   version is a lock or a reference count per lookup.
//! - [`Slots`] reads committed region words as atomic pointers in place: the safe version is
//!   a `Vec` behind a lock, as an ID column grows while others read it.

use std::{
    alloc::Layout,
    marker::PhantomData,
    ptr::NonNull,
    sync::{
        OnceLock,
        atomic::{AtomicUsize, Ordering},
    },
};

use blaze_mem::{Region, Tag};

use crate::sync::atomic::{AtomicPtr, AtomicU64};

/// Address space for every interned value and probe table of the process.
const RESERVED: usize = 64 << 30;

/// Every claim starts and ends on a cache line, so two claims never share one.
pub(crate) const LINE: usize = 64;

struct Space {
    region: Region,
    /// Bytes from the region's base claimed so far.
    cursor: AtomicUsize,
}

fn space() -> &'static Space {
    static SPACE: OnceLock<Space> = OnceLock::new();
    SPACE.get_or_init(|| Space {
        region: Region::reserve_huge(RESERVED, Tag::Intern)
            .unwrap_or_else(|error| panic!("intern: {error}")),
        cursor: AtomicUsize::new(0),
    })
}

/// `layout.size()` fresh bytes (at least one) aligned to `layout.align()`, all zero, never freed,
/// as their offset from the region's base. Stops with the region's error when its reservation is
/// used up or the kernel refuses to commit (no-fallback).
#[track_caller]
pub(crate) fn claim_offset(layout: Layout) -> usize {
    let space = space();
    let size = layout.size().max(1).next_multiple_of(LINE);
    let align = layout.align().max(LINE);
    // The base is page-aligned and the cursor moves by whole lines, so `align - LINE` bytes of
    // padding reach any alignment.
    let start = space.cursor.fetch_add(size + (align - LINE), Ordering::Relaxed);
    let base = space.region.base().as_ptr().addr();
    let at = (base + start).next_multiple_of(align) - base;
    if let Err(error) = space.region.ensure(at + size) {
        panic!("intern: {error}");
    }
    at
}

/// The address `offset` bytes into the region: of a claim, or inside one.
#[inline]
pub(crate) fn at(offset: usize) -> NonNull<u8> {
    space().region.at(offset)
}

/// [`claim_offset`]'s bytes, as their address.
#[track_caller]
pub(crate) fn claim(layout: Layout) -> NonNull<u8> {
    at(claim_offset(layout))
}

/// Types for which all-zero bytes are a value.
///
/// # Safety
///
/// An implementor's every bit being zero is a valid, initialized value of it. Implemented here
/// only, for the types [`zeroed`] tables are made of.
#[cfg_attr(blaze_check, allow(dead_code, reason = "a model build's tables are boxes of the checker's atomics"))]
pub(crate) unsafe trait Zeroed {}

// SAFETY: An `AtomicU64` has `u64`'s representation; zero is the integer 0.
#[cfg(not(blaze_check))]
unsafe impl Zeroed for AtomicU64 {}

// SAFETY: An `AtomicPtr<T>` has `*mut T`'s representation; zero is the null pointer.
#[cfg(not(blaze_check))]
unsafe impl<T> Zeroed for AtomicPtr<T> {}

// SAFETY: A `Group` is `repr(C)` of an `AtomicU64` and `AtomicPtr`s, both `Zeroed` (above): a
// zero control word (every slot empty) and null pointers.
#[cfg(not(blaze_check))]
unsafe impl Zeroed for Group {}

/// `len` `T`s in fresh claimed space, every byte zero, never freed.
#[cfg_attr(blaze_check, allow(dead_code, reason = "a model build's tables are boxes of the checker's atomics"))]
pub(crate) fn zeroed<T: Zeroed>(len: usize) -> &'static [T] {
    let at = claim(Layout::array::<T>(len).expect("an interner table's layout")).cast::<T>();
    // SAFETY: `claim` gives `len * size_of::<T>()` committed bytes aligned for `T`, all zero
    // (fresh pages, claimed by no one before) and never freed; zero bytes are a `T` (`Zeroed`),
    // so every element is initialized. A wrong length or alignment would read past the claim.
    unsafe { std::slice::from_raw_parts(at.as_ptr(), len) }
}

/// `value` moved into fresh claimed space, never dropped or freed: `Box::leak` without the
/// global allocator.
#[cfg_attr(blaze_check, allow(dead_code, reason = "a model build leaks its tables as boxes"))]
pub(crate) fn place<T>(value: T) -> &'static T {
    let at = claim(Layout::new::<T>()).cast::<T>();
    // SAFETY: `at` is committed, aligned for `T`, room for one, claimed by this call alone and
    // never freed, so nothing else reads or writes it; it holds no value to drop.
    unsafe { at.write(value) };
    // SAFETY: `at` was initialized just above and is never written, moved or freed again, so a
    // shared reference to it is good for `'static`. Without the write it would read zeroes as a
    // `T`.
    unsafe { at.as_ref() }
}

/// Seven slots and their tags in one cache line: a group of [`crate::arena`]'s probe tables.
///
/// `ctrl` byte `i` (from the least significant) is slot `i`'s tag: 0 when empty, `0x80 | ` the
/// hash's top seven bits when full. Only the holder of the shard's lock writes, and each slot
/// once: its `value` first, then its tag into `ctrl` with `Release`. A reader that loads `ctrl`
/// with `Acquire` and sees the tag sees the pointer and the value's bytes.
#[repr(C, align(64))]
pub(crate) struct Group {
    pub(crate) ctrl: AtomicU64,
    pub(crate) values: [AtomicPtr<u8>; GROUP],
}

/// Slots in a group: tag bytes 0 to 6 of the group's control word; byte 7 stays 0.
pub(crate) const GROUP: usize = 7;

#[cfg(not(blaze_check))]
const _: () = assert!(size_of::<Group>() == 64);

/// An atomic pointer that only ever holds a `&'static T` (or null): what a probe table's shard
/// publishes its current table through. Loading it is one acquire load, with no lock and no
/// count.
pub(crate) struct Published<T: 'static> {
    at: AtomicPtr<T>,
    /// Hands out `&'static T` to any thread: `Send` and `Sync` only where `&T` is.
    _shares: PhantomData<&'static T>,
}

impl<T: 'static> Published<T> {
    pub(crate) fn new() -> Published<T> {
        Published { at: AtomicPtr::new(std::ptr::null_mut()), _shares: PhantomData }
    }

    /// What was stored last, as `order` sees it; `None` before the first store.
    #[inline]
    pub(crate) fn load(&self, order: Ordering) -> Option<&'static T> {
        let at = self.at.load(order);
        // SAFETY: `at` is null or was a `&'static T` (`store` takes nothing else), so it points
        // at a `T` that lives, unwritten but through its own atomics, for the process. A pointer
        // of a shorter life would be read after it was freed.
        unsafe { at.as_ref() }
    }

    #[inline]
    pub(crate) fn store(&self, value: &'static T, order: Ordering) {
        self.at.store(std::ptr::from_ref(value).cast_mut(), order);
    }
}

/// A column of atomic pointers indexed from 0, every slot null until set, over a region reserved
/// for every `u32` index (32 GiB of address space, committed as slots are set, counted under
/// [`Tag::Intern`], on huge pages past its first 2 MiB): what [`crate::typed::Records`] maps a
/// made ID to its record with.
#[cfg(not(blaze_check))]
pub(crate) struct Slots<T> {
    slots: Region,
    _of: PhantomData<fn() -> T>,
}

#[cfg(not(blaze_check))]
const SLOT: usize = size_of::<AtomicPtr<()>>();

#[cfg(not(blaze_check))]
impl<T> Slots<T> {
    pub(crate) fn new() -> Slots<T> {
        Slots {
            slots: Region::reserve_huge(u32::MAX as usize * SLOT, Tag::Intern)
                .unwrap_or_else(|error| panic!("intern: {error}")),
            _of: PhantomData,
        }
    }

    /// Slot `index`, `None` when it was never committed (so never set).
    #[inline]
    pub(crate) fn get(&self, index: usize) -> Option<&AtomicPtr<T>> {
        if (index + 1) * SLOT > self.slots.committed() {
            return None;
        }
        Some(self.word(index))
    }

    /// Slot `index`, committed first. Stops with the region's error when the kernel refuses.
    pub(crate) fn ensure(&self, index: usize) -> &AtomicPtr<T> {
        if let Err(error) = self.slots.ensure((index + 1) * SLOT) {
            panic!("intern: {error}");
        }
        self.word(index)
    }

    /// Slot `index`, which the caller saw committed.
    fn word(&self, index: usize) -> &AtomicPtr<T> {
        let at = self.slots.at(index * SLOT).cast::<AtomicPtr<T>>();
        // SAFETY: the slot is committed (`get` checks the committed length, `ensure` commits it
        // or stops), so inside the reservation, and aligned (a page-aligned base plus a multiple
        // of 8); committed pages are zeroed by the kernel and never decommitted while the region
        // lives, and zero bytes are a null `AtomicPtr` (`Zeroed`). The reference lives as long as
        // `self`. An uncommitted slot would fault.
        unsafe { at.as_ref() }
    }
}

/// The model checker's atomics are not plain words, so a model build keeps the column in levels
/// of doubling size, each made on first use: level `k` holds indices `2^k - 1 .. 2^(k+1) - 1`.
#[cfg(blaze_check)]
pub(crate) struct Slots<T> {
    levels: [crate::sync::OnceLock<Box<[AtomicPtr<T>]>>; 32],
}

#[cfg(blaze_check)]
impl<T> Slots<T> {
    pub(crate) fn new() -> Slots<T> {
        Slots { levels: std::array::from_fn(|_| crate::sync::OnceLock::new()) }
    }

    fn level(index: usize) -> (usize, usize) {
        let level = (index + 1).ilog2() as usize;
        (level, index + 1 - (1 << level))
    }

    pub(crate) fn get(&self, index: usize) -> Option<&AtomicPtr<T>> {
        let (level, at) = Self::level(index);
        self.levels[level].get()?.get(at)
    }

    pub(crate) fn ensure(&self, index: usize) -> &AtomicPtr<T> {
        let (level, at) = Self::level(index);
        // alloc: the model checker's build only, once a doubling of the column
        let slots = self.levels[level].get_or_init(|| (0..1usize << level).map(|_| AtomicPtr::new(std::ptr::null_mut())).collect());
        &slots[at]
    }
}

#[cfg(all(test, not(blaze_check)))]
mod tests {
    use super::*;

    /// Claims from many threads at once never overlap, keep their alignment and read as zero.
    #[test]
    fn claims_are_disjoint_aligned_and_zero() {
        let taken: Vec<(usize, usize)> = std::thread::scope(|scope| {
            let workers: Vec<_> = (0..8usize)
                .map(|worker| {
                    scope.spawn(move || {
                        (0..2_000usize)
                            .map(|n| {
                                let size = (n * 37 + worker) % 5_000;
                                let align = 1 << ((n + worker) % 13);
                                let at = claim(Layout::from_size_align(size, align).unwrap());
                                assert_eq!(at.as_ptr().addr() % align.max(LINE), 0);
                                // SAFETY: `claim` gave `size.max(1)` committed bytes.
                                let bytes = unsafe { std::slice::from_raw_parts(at.as_ptr(), size) };
                                assert!(bytes.iter().all(|&byte| byte == 0), "fresh space is zero");
                                // SAFETY: as above; the bytes are this claim's alone.
                                unsafe { at.as_ptr().write_bytes(0xa5, size.max(1)) };
                                (at.as_ptr().addr(), size.max(1))
                            })
                            .collect::<Vec<_>>()
                    })
                })
                .collect();
            workers.into_iter().flat_map(|it| it.join().unwrap()).collect()
        });
        let mut taken = taken;
        taken.sort_unstable();
        for pair in taken.windows(2) {
            assert!(pair[0].0 + pair[0].1 <= pair[1].0, "{pair:?}");
        }
    }

    /// A claim's offset and address agree, and an offset inside it is the address inside it.
    #[test]
    fn offsets_are_addresses() {
        let offset = claim_offset(Layout::from_size_align(300, 8).unwrap());
        let base = at(offset).as_ptr().addr();
        assert_eq!(at(offset + 17).as_ptr().addr(), base + 17);
        assert_eq!(base % LINE, 0);
    }

    /// Zeroed tables read as zero, placed values read back, and a published reference is the one
    /// stored.
    #[test]
    fn zeroed_placed_and_published() {
        let groups: &[Group] = zeroed(16);
        assert!(groups.iter().all(|group| group.ctrl.load(Ordering::Relaxed) == 0));
        assert!(groups.iter().flat_map(|group| &group.values).all(|value| value.load(Ordering::Relaxed).is_null()));
        let hashes: &[AtomicU64] = zeroed(100);
        assert!(hashes.iter().all(|hash| hash.load(Ordering::Relaxed) == 0));
        let placed = place([7u64, 8, 9]);
        assert_eq!(*placed, [7, 8, 9]);
        let published = Published::<[u64; 3]>::new();
        assert!(published.load(Ordering::Acquire).is_none());
        published.store(placed, Ordering::Release);
        assert!(std::ptr::eq(published.load(Ordering::Acquire).unwrap(), placed));
    }

    /// Slots read null until set, far apart and committed lazily.
    #[test]
    fn slots_commit_as_set() {
        let slots = Slots::<u64>::new();
        assert!(slots.get(0).is_none_or(|slot| slot.load(Ordering::Relaxed).is_null()));
        let value = place(5u64);
        let far = 1 << 20;
        slots.ensure(far).store(std::ptr::from_ref(value).cast_mut(), Ordering::Release);
        assert_eq!(slots.get(far).unwrap().load(Ordering::Acquire), std::ptr::from_ref(value).cast_mut());
        assert!(slots.get(far - 1).unwrap().load(Ordering::Acquire).is_null());
        assert!(slots.get(far * 64).is_none());
    }

    /// A warm claim allocates nothing from the global allocator.
    #[test]
    fn claims_allocate_nothing() {
        claim(Layout::new::<u64>());
        blaze_alloc_count::assert_at_most(0, "claiming interner space", || {
            for n in 1..10_000 {
                claim(Layout::from_size_align(n % 3_000, 8).unwrap());
            }
            place([7u64; 4]);
        });
    }
}
