//! The dense engine's memory, the one unsafe concern of laying typed places over reserved
//! regions (docs/v2/plan/design/unsafe.md):
//!
//! - [`Dense<T>`]: a column of atomics over a reserved [`Region`], zero where never written, so
//!   a place is valid as soon as its page is committed. A column made [`Dense::committed`] is
//!   committed whole once, so a read is an index and a load, with no bounds load on the hit
//!   path; one made [`Dense::growing`] commits as its users ask ([`Dense::ensure`]).
//! - [`Heap`]: a bump allocator of row pages over one reserved region: pages are carved once,
//!   zeroed by the kernel, never moved and never freed but with the heap.

use std::{
    marker::PhantomData,
    ptr::NonNull,
    sync::atomic::{AtomicPtr, AtomicU32, AtomicU64, AtomicUsize, Ordering},
};

use blaze_mem::{Region, Tag};

/// A type every place of which is valid all zero, and which is `Sync` by its own atomics.
///
/// # Safety
///
/// All-zero bytes are a valid value of the type, and a shared reference to it may be used from
/// any thread.
pub(crate) unsafe trait Zeroed: Sync {}

// SAFETY: an atomic integer's zero bytes are the value 0; atomics are `Sync`.
unsafe impl Zeroed for AtomicU32 {}
// SAFETY: as for `AtomicU32`.
unsafe impl Zeroed for AtomicU64 {}
// SAFETY: as for `AtomicU32`.
unsafe impl Zeroed for AtomicUsize {}
// SAFETY: zero bytes are the null pointer; `AtomicPtr` is `Sync`.
unsafe impl<T> Zeroed for AtomicPtr<T> {}
// SAFETY: an array's bytes are its elements', each valid zero and `Sync`.
unsafe impl<T: Zeroed, const N: usize> Zeroed for [T; N] {}

/// A column of `T` over reserved address space, zero where never written.
pub(crate) struct Dense<T: Zeroed> {
    region: Region,
    len: usize,
    _holds: PhantomData<T>,
}

impl<T: Zeroed> Dense<T> {
    /// A column of `len` places, all committed now: `get` needs no bounds but `len`'s.
    pub(crate) fn committed(len: usize, tag: Tag) -> Dense<T> {
        let column = Dense::growing(len, tag);
        column.ensure(len);
        column
    }

    /// A column of at most `len` places, none committed.
    pub(crate) fn growing(len: usize, tag: Tag) -> Dense<T> {
        let bytes = len.checked_mul(size_of::<T>()).expect("a dense column's size overflows");
        let region = Region::reserve(bytes, tag)
            .unwrap_or_else(|why| panic!("the query database's {len}-place column: {why}"));
        Dense { region, len, _holds: PhantomData }
    }

    pub(crate) fn len(&self) -> usize {
        self.len
    }

    /// Commits places `0..len`, from any thread.
    #[inline]
    pub(crate) fn ensure(&self, len: usize) {
        assert!(len <= self.len, "a dense column of {} places asked for {len}", self.len);
        if let Err(why) = self.region.ensure(len * size_of::<T>()) {
            panic!("the query database's column could not grow to {len} places: {why}");
        }
    }

    /// Place `index`, which an [`Dense::ensure`] covering it happened before (or the column is
    /// [`Dense::committed`]).
    #[inline(always)]
    pub(crate) fn get(&self, index: usize) -> &T {
        debug_assert!(index < self.len);
        debug_assert!((index + 1) * size_of::<T>() <= self.region.committed());
        // SAFETY: `index < len`, so the place is inside the reservation and aligned (the
        // region's base is page-aligned and `T`'s size is a multiple of its alignment); the
        // caller's `ensure` committed it, so it is readable and writable and holds a valid `T`
        // (`Zeroed`: zero where never written, else what a shared `&T` wrote). It lives as
        // long as `self`.
        unsafe { &*self.region.at(index * size_of::<T>()).as_ptr().cast::<T>() }
    }

    /// Place `index` if it is committed: for readers that may run ahead of the committer.
    #[inline]
    pub(crate) fn get_checked(&self, index: usize) -> Option<&T> {
        ((index + 1) * size_of::<T>() <= self.region.committed()).then(|| self.get(index))
    }

    /// The first place, for a hit path that indexes from it.
    #[inline(always)]
    pub(crate) fn base(&self) -> NonNull<T> {
        self.region.base().cast()
    }
}

/// Row pages: carved from one reserved region by a bump pointer, zeroed by the kernel.
pub(crate) struct Heap {
    region: Region,
    top: AtomicUsize,
}

/// The largest alignment a row asks of its page.
pub(crate) const MAX_ALIGN: usize = 4096;

impl Heap {
    pub(crate) fn new(bytes: usize, tag: Tag) -> Heap {
        let region = Region::reserve(bytes, tag)
            .unwrap_or_else(|why| panic!("the query database's {bytes}-byte page heap: {why}"));
        Heap { region, top: AtomicUsize::new(0) }
    }

    /// `bytes` of zeroes aligned to `align` (a power of two, at most [`MAX_ALIGN`]), from any
    /// thread, valid while the heap lives.
    pub(crate) fn alloc(&self, bytes: usize, align: usize) -> NonNull<u8> {
        assert!(align.is_power_of_two() && align <= MAX_ALIGN);
        let mut top = self.top.load(Ordering::Relaxed);
        let start = loop {
            let start = top.next_multiple_of(align);
            let end = start.checked_add(bytes).expect("a row page's size overflows");
            assert!(
                end <= self.region.reserved(),
                "the query database's page heap is full ({} bytes)",
                self.region.reserved()
            );
            match self.top.compare_exchange_weak(top, end, Ordering::Relaxed, Ordering::Relaxed) {
                Ok(_) => break start,
                Err(now) => top = now,
            }
        };
        if let Err(why) = self.region.ensure(start + bytes) {
            panic!("the query database's page heap could not grow by {bytes} bytes: {why}");
        }
        self.region.at(start)
    }

    /// Bytes carved so far.
    #[cfg(test)]
    fn used(&self) -> usize {
        self.top.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dense_places_start_zero_and_keep_what_was_stored() {
        let column = Dense::<AtomicU64>::growing(1 << 20, Tag::Query);
        column.ensure(10);
        assert_eq!(column.get(9).load(Ordering::Relaxed), 0);
        column.get(3).store(7, Ordering::Relaxed);
        column.ensure(100_000);
        assert_eq!(column.get(3).load(Ordering::Relaxed), 7);
        assert!(column.get_checked(99_999).is_some());
        let whole = Dense::<AtomicU32>::committed(4096, Tag::Query);
        assert_eq!(whole.get(4095).load(Ordering::Relaxed), 0);
    }

    #[test]
    fn heap_pages_are_aligned_zeroed_and_apart() {
        let heap = Heap::new(1 << 24, Tag::Query);
        let a = heap.alloc(100, 8);
        let b = heap.alloc(4000, 64);
        assert!(b.as_ptr().addr().is_multiple_of(64));
        assert!(b.as_ptr().addr() >= a.as_ptr().addr() + 100);
        // SAFETY: the 4000 bytes at `b` were just carved for this test alone.
        let bytes = unsafe { std::slice::from_raw_parts(b.as_ptr(), 4000) };
        assert!(bytes.iter().all(|&it| it == 0));
        assert!(heap.used() >= 4100);
    }
}
