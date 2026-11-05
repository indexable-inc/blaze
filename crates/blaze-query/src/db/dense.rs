//! The dense engine's memory, the one unsafe concern of laying typed places over reserved
//! regions (docs/v2/plan/design/unsafe.md):
//!
//! - [`Dense<T>`]: a column of atomics over a reserved [`Region`], zero where never written, so
//!   a place is valid as soon as its page is committed. Its published length is the one bound
//!   a read checks: every page under it was committed before it was published
//!   ([`Dense::ensure`]), so `index < len` alone proves place `index` readable. A [`Whole`]
//!   column ([`Dense::sparse`]) is a sparse reservation, every place of which reads zero until
//!   written, so its bound is a constant (and an `Id`'s page needs no check against it); its
//!   writers commit what they write ([`Dense::get_to_write`]), so it counts only that. One made
//!   [`Dense::growing`] grows as its users ask.
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

/// A column of `T` over reserved address space, zero where never written. `WHOLE`: 0 for a
/// growing column, else the capacity of a [`Whole`] one, made by [`Dense::sparse`] with every
/// place readable and writable from the start.
pub(crate) struct Dense<T: Zeroed, const WHOLE: usize = 0> {
    /// The published length: places `0..len` are committed, and stay so while the column
    /// lives. Only `Dense::grow` raises it, after the commit (see the invariant there);
    /// only [`Dense::clear`] lowers it, to 0 under `&mut`. Never past `capacity`. A [`Whole`]
    /// column's is the prefix its writers committed, which bounds no read (every place of its
    /// reservation is readable).
    len: AtomicUsize,
    region: Region,
    /// Places reserved: `capacity * size_of::<T>() <= region.reserved()`.
    capacity: usize,
    /// Which column this is ("node colors", "dependency edges"), for a panic to name: every
    /// query column shares one [`Tag`].
    name: &'static str,
    _holds: PhantomData<T>,
}

/// A column of `N` places, every one readable from when it is made: a read's bound is the
/// constant `N`. A sparse reservation ([`Region::reserve_sparse`]): a place reads zero until
/// written, and only the pages written cost memory, so a directory of `2^20` entries of which a
/// few thousand are used counts (and holds) what it uses, not 8 MiB.
pub(crate) type Whole<T, const N: usize> = Dense<T, N>;

impl<T: Zeroed, const N: usize> Whole<T, N> {
    /// A column of `N` places, all readable now, none committed.
    pub(crate) fn sparse(tag: Tag, name: &'static str) -> Whole<T, N> {
        const { assert!(N > 0, "a whole dense column of no places") };
        // INVARIANT (a whole column): the reservation is sparse, so every byte of its
        // `N * size_of::<T>()` is readable and writable from here until the column is dropped
        // (`Region::reserve_sparse`: mapped so from the start, never decommitted, and released
        // only by the region's `Drop`). So `index < N` alone proves a place valid, with no load
        // of `len` (`Dense::bound`), whatever was committed: committing only counts the pages
        // a writer uses ([`Dense::get_to_write`]).
        Dense::reserve(N, tag, name)
    }

    /// Place `index`, committed first (counted in the column's tag): what a writer stores
    /// through. A store through [`Dense::get`] would be as sound (every place of the sparse
    /// reservation is writable), but uncounted.
    ///
    /// # Panics
    ///
    /// As [`Dense::get`].
    #[inline]
    #[track_caller]
    pub(crate) fn get_to_write(&self, index: usize) -> &T {
        let place = self.get(index);
        self.ensure(index + 1);
        place
    }
}

impl<T: Zeroed> Dense<T> {
    /// A column of at most `capacity` places, none committed; `name` says which, for its
    /// panics.
    pub(crate) fn growing(capacity: usize, tag: Tag, name: &'static str) -> Dense<T> {
        Dense::reserve(capacity, tag, name)
    }
}

impl<T: Zeroed, const WHOLE: usize> Dense<T, WHOLE> {
    fn reserve(capacity: usize, tag: Tag, name: &'static str) -> Dense<T, WHOLE> {
        const { assert!(size_of::<T>() > 0, "a dense column of a zero-sized type") };
        let bytes = capacity.checked_mul(size_of::<T>()).unwrap_or_else(|| {
            panic!("the query database's {capacity}-place column of {name}: its size overflows")
        });
        let region = if WHOLE > 0 {
            Region::reserve_sparse(bytes, tag)
        } else {
            Region::reserve(bytes, tag)
        };
        let region = region.unwrap_or_else(|why| {
            panic!("the query database's {capacity}-place column of {name}: {why}")
        });
        Dense { len: AtomicUsize::new(0), region, capacity, name, _holds: PhantomData }
    }

    /// Commits places `0..len` and publishes them, from any thread.
    #[inline]
    pub(crate) fn ensure(&self, len: usize) {
        if len > self.len.load(Ordering::Acquire) {
            self.grow(len);
        }
    }

    /// The published length: every place below it is committed.
    pub(crate) fn len(&self) -> usize {
        self.len.load(Ordering::Acquire)
    }

    /// Bytes of the column committed now: what it holds of the process's memory.
    pub(crate) fn committed_bytes(&self) -> usize {
        self.region.committed()
    }

    /// Empties the column and gives its pages back to the kernel. `&mut`: no reference into
    /// the column is alive, so the length may go down (the one place it does). A place is
    /// committed again before it is next read: `Dense::grow`'s invariant holds from 0.
    pub(crate) fn clear(&mut self) {
        if let Err(why) = self.region.decommit_after(0) {
            panic!(
                "the query database's column of {} could not give its pages back: {why}",
                self.name
            );
        }
        *self.len.get_mut() = 0;
    }

    /// The bound a read checks: the published length, read with acquire, or a whole column's
    /// capacity, a constant (every place of its sparse reservation readable, see
    /// [`Dense::sparse`]).
    #[inline(always)]
    fn bound(&self) -> usize {
        if WHOLE > 0 { WHOLE } else { self.len.load(Ordering::Acquire) }
    }

    #[cold]
    #[inline(never)]
    fn grow(&self, len: usize) {
        assert!(
            len <= self.capacity,
            "the dense query column of {} of {} places asked for {len}",
            self.name,
            self.capacity
        );
        // `len * size_of::<T>() <= capacity * size_of::<T>()`, which `reserve` checked.
        if let Err(why) = self.region.ensure(len * size_of::<T>()) {
            panic!(
                "the query database's column of {} could not grow to {len} places: {why}",
                self.name
            );
        }
        // INVARIANT (the published length, what `get`'s one compare rests on): every page
        // under `published * size_of::<T>()` is committed before `published` is stored.
        // - `region.ensure` returned, so the region's committed prefix covers `len` places,
        //   and `committed()` reads at least that much (the commit was published with release,
        //   read here with acquire or made by this thread).
        // - `published` is that prefix in whole places, at most `capacity`, so every page
        //   under it is committed: whole, not a prefix of a page.
        // - The prefix never shrinks under a shared borrow: only `Region::decommit_after`
        //   lowers it, which takes `&mut Region`; `Dense` never hands its region out and
        //   calls it only from `Dense::clear`, under `&mut self`, which lowers `len` to 0
        //   with it, so no place below a published length is ever left uncommitted.
        // - Release here, acquire in `get`: a reader that sees `published` sees the commit
        //   that came before it in this thread.
        // - `fetch_max`: racing growers each publish a committed prefix, and the length only
        //   goes up.
        // A whole column's region is sparse: `ensure` there only counts, and the length bounds
        // no read (`Dense::sparse`'s invariant).
        let published = (self.region.committed() / size_of::<T>()).min(self.capacity);
        debug_assert!(published >= len);
        self.len.fetch_max(published, Ordering::Release);
    }

    /// Place `index`, below the published length: an [`Dense::ensure`] covering it happened
    /// before (or below the capacity of a [`Dense::sparse`] column).
    ///
    /// # Panics
    ///
    /// When `index` is at or past the published length, naming both and the column. The
    /// check runs in every build, as this is a safe fn: one load and one compare, since the
    /// published length alone proves the place committed (`Dense::grow`'s invariant).
    #[inline(always)]
    #[track_caller]
    pub(crate) fn get(&self, index: usize) -> &T {
        if index >= self.bound() {
            self.not_a_place(index);
        }
        // SAFETY: `index` is below the bound, checked just above.
        unsafe { self.place(index) }
    }

    /// Place `index` if it is below the published length: for readers that may run ahead of
    /// the committer.
    #[inline]
    pub(crate) fn get_checked(&self, index: usize) -> Option<&T> {
        // SAFETY: `index` is below the bound, checked just before.
        (index < self.bound()).then(|| unsafe { self.place(index) })
    }

    /// Places `start..start + len`, below the bound: one check for a run of places (a span's
    /// edges as they are stored or copied).
    ///
    /// # Panics
    ///
    /// When the run passes the bound, naming its end and the column.
    #[inline]
    #[track_caller]
    pub(crate) fn slice(&self, start: usize, len: usize) -> &[T] {
        let end = start.checked_add(len).unwrap_or(usize::MAX);
        if end > self.bound() {
            self.not_a_place(end - 1);
        }
        if len == 0 {
            return &[];
        }
        // SAFETY: every place of the run is below the bound (checked just above), so `place`'s
        // contract holds for each, and they are consecutive in the reservation: `len` valid
        // `T`s from place `start`, living as long as `self`. The pointer comes from the
        // region, not from `place`'s one-element `&T`, so its provenance covers the whole run.
        unsafe {
            std::slice::from_raw_parts(
                self.region.at_unchecked(start * size_of::<T>()).as_ptr().cast::<T>(),
                len,
            )
        }
    }

    /// Place `index` under `&mut`: no grower runs, so the published length is read without
    /// an atomic load (a writer's path, such as an input's setter).
    ///
    /// # Panics
    ///
    /// As [`Dense::get`].
    #[inline(always)]
    #[track_caller]
    pub(crate) fn get_mut(&mut self, index: usize) -> &mut T {
        if index >= *self.len.get_mut() {
            self.not_a_place(index);
        }
        // SAFETY: `index` is below the published length, read under `&mut` (so no store to it
        // is racing): the place is committed, in the reservation and aligned, as in `place`.
        // `&mut self` makes it this borrow's alone.
        unsafe { &mut *self.region.at_unchecked(index * size_of::<T>()).as_ptr().cast::<T>() }
    }

    /// # Safety
    ///
    /// `index < self.bound()`: below a published length this thread read with acquire (or
    /// published), or below a whole column's capacity.
    #[inline(always)]
    unsafe fn place(&self, index: usize) -> &T {
        // SAFETY:
        // - Accessible: `index < len`, so by `Dense::grow`'s invariant (the published length)
        //   the page holding the place was committed before `len` was published, and the
        //   caller's acquire load sees that; or the column is whole and `index < capacity`, in
        //   a sparse reservation readable and writable whole for the column's life
        //   (`Dense::sparse`'s invariant). It stays so while `self` lives. So the place is
        //   readable and writable and holds a valid `T` (`Zeroed`: zero where never written,
        //   else what a shared `&T` wrote).
        // - In the reservation: `len <= capacity`, so `index * size_of::<T>() <
        //   capacity * size_of::<T>() <= reserved`, which is `at_unchecked`'s `offset <=
        //   reserved`; the multiply does not overflow (`reserve` checked the larger one).
        // - Aligned: the region's base is page-aligned and `T`'s size is a multiple of its
        //   alignment, at most a page for every `Zeroed`.
        // It lives as long as `self`.
        unsafe { &*self.region.at_unchecked(index * size_of::<T>()).as_ptr().cast::<T>() }
    }

    /// Hints that place `index` is read soon. No check: a prefetch never faults, and no
    /// reference is made, so any `index` is sound (one past the column only wastes the hint).
    #[inline(always)]
    pub(crate) fn prefetch(&self, index: usize) {
        prefetch::read(self.region.base().as_ptr().cast::<T>().wrapping_add(index));
    }

    #[cold]
    #[inline(never)]
    #[track_caller]
    fn not_a_place(&self, index: usize) -> ! {
        if WHOLE > 0 {
            panic!(
                "place {index} of the whole dense query column of {} ({:?}): past its {WHOLE} places",
                self.name,
                self.region.tag(),
            );
        }
        panic!(
            "place {index} of the dense query column of {} ({:?}): past its published length \
             of {} places (of {} reserved)",
            self.name,
            self.region.tag(),
            self.len(),
            self.capacity,
        )
    }

    /// The first place, for a hit path that indexes from it. Indexing from it is the caller's
    /// to bound: a place is valid only below the published length ([`Dense::len`], whose
    /// pages are committed by `Dense::grow`'s invariant). A [`Dense::sparse`] column is
    /// readable whole from when it is made, so there any index below its capacity is valid.
    #[inline(always)]
    pub(crate) fn base(&self) -> NonNull<T> {
        self.region.base().cast()
    }
}

/// A prefetch hint for a place read soon (a color the verify walk reaches next). The one
/// unsafe concern: the instruction.
mod prefetch {
    #[inline(always)]
    pub(super) fn read<T>(at: *const T) {
        #[cfg(target_arch = "x86_64")]
        // SAFETY: `prefetcht0` is a hint: it never faults and writes nothing, whatever `at` is.
        unsafe {
            std::arch::asm!("prefetcht0 [{at}]", at = in(reg) at, options(nostack, preserves_flags, readonly));
        }
        #[cfg(target_arch = "aarch64")]
        // SAFETY: `prfm pldl1keep` is a hint: it never faults and writes nothing, whatever
        // `at` is.
        unsafe {
            std::arch::asm!("prfm pldl1keep, [{at}]", at = in(reg) at, options(nostack, preserves_flags, readonly));
        }
        #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
        let _ = at;
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
        let column = Dense::<AtomicU64>::growing(1 << 20, Tag::Query, "a test's column");
        column.ensure(10);
        assert_eq!(column.get(9).load(Ordering::Relaxed), 0);
        column.get(3).store(7, Ordering::Relaxed);
        column.ensure(100_000);
        assert_eq!(column.get(3).load(Ordering::Relaxed), 7);
        assert!(column.get_checked(99_999).is_some());
        let whole = Whole::<AtomicU32, 4096>::sparse(Tag::Query, "a test's whole column");
        assert_eq!(whole.get(4095).load(Ordering::Relaxed), 0);
    }

    /// A whole column reads zero at every place below its capacity before anything is
    /// committed, refuses its capacity (`get_checked` says `None`, `get` panics naming the
    /// column), and counts as committed only what its writers asked for.
    #[test]
    fn a_whole_column_reads_zero_below_its_capacity_refuses_it_and_commits_what_is_written() {
        const N: usize = 1 << 20;
        let whole = Whole::<AtomicU64, N>::sparse(Tag::Query, "a test's directory");
        assert_eq!(whole.len(), 0, "a sparse column commits nothing when it is made");
        assert_eq!(whole.get(N - 1).load(Ordering::Acquire), 0);
        assert_eq!(whole.get(N / 2).load(Ordering::Acquire), 0);
        assert!(whole.get_checked(N - 1).is_some());
        assert!(whole.get_checked(N).is_none());
        let refused = std::panic::catch_unwind(|| whole.get(N).load(Ordering::Relaxed))
            .expect_err("a read at a whole column's capacity panics");
        let message = refused.downcast_ref::<String>().expect("a formatted panic");
        assert!(
            message.contains(&format!(
                "place {N} of the whole dense query column of a test's directory"
            )) && message.contains(&format!("past its {N} places")),
            "{message}"
        );
        whole.get_to_write(1000).store(5, Ordering::Release);
        assert_eq!(whole.get(1000).load(Ordering::Acquire), 5);
        // What a write commits is each platform's (`Region::reserve_sparse`): Linux and macOS
        // count a prefix covering the place written and short of the whole reservation;
        // Windows committed the whole reservation when it was made, so the first write
        // publishes every place.
        let committed = whole.len();
        if cfg!(windows) {
            assert_eq!(
                committed, N,
                "writing place 1000 published {committed} of {N} places committed whole"
            );
        } else {
            assert!(
                (1001..N).contains(&committed),
                "writing place 1000 committed {committed} of {N} places"
            );
        }
        assert!(std::panic::catch_unwind(|| whole.get_to_write(N)).is_err());
    }

    /// Growers race to commit and publish a fresh growing column while readers read at its
    /// published length, every round started together on one barrier. Each round's column is
    /// new, so each races its whole growth (about twenty commits from 64 KiB to 4 MiB), and
    /// the growers write every place as they go, which keeps the growth going for a
    /// millisecond or so rather than the few microseconds the commits alone take. A length
    /// published before its pages are committed lets a reader touch the newest place while
    /// its page is still inaccessible, which faults on macOS and Windows: planted once
    /// (`Dense::grow` storing the asked-for length before `region.ensure`), this test died of
    /// the fault on the first run. On Linux a reservation is readable and writable whole
    /// (blaze-mem's `os::reserve`), so there such a place reads zero and the test checks only
    /// the values.
    /// The readers must also see the length move under them: a run where the growth never
    /// raced a reader proves nothing, so it fails, saying so.
    #[test]
    fn racing_growers_and_readers_only_ever_read_committed_places() {
        use std::sync::Barrier;
        const ROUNDS: usize = 200;
        const CAP: usize = 1 << 19;
        const GROWERS: usize = 4;
        const READERS: usize = 4;
        let raced = AtomicU64::new(0);
        for round in 0..ROUNDS {
            let column = Dense::<AtomicU64>::growing(CAP, Tag::Query, "a raced column");
            let start = Barrier::new(GROWERS + READERS);
            let grown = AtomicUsize::new(0);
            std::thread::scope(|scope| {
                for grower in 0..GROWERS {
                    let (column, start, grown) = (&column, &start, &grown);
                    scope.spawn(move || {
                        start.wait();
                        let mut done = 0;
                        while done < CAP {
                            let at = (done + 4096 + 512 * grower).min(CAP);
                            column.ensure(at);
                            for place in done..at {
                                column.get(place).store(place as u64 + 1, Ordering::Release);
                            }
                            done = at;
                        }
                        grown.fetch_add(1, Ordering::Release);
                    });
                }
                for reader in 0..READERS {
                    let (column, start, grown, raced) = (&column, &start, &grown, &raced);
                    scope.spawn(move || {
                        let mut rng =
                            blaze_ids::rng::SplitMix64::new((round * READERS + reader) as u64);
                        let (mut last, mut moved) = (0, 0);
                        start.wait();
                        while grown.load(Ordering::Acquire) < GROWERS {
                            let len = column.len();
                            if len == 0 {
                                continue;
                            }
                            if len != last {
                                moved += u64::from(last != 0);
                                last = len;
                            }
                            // The newest place: the one a length published before its commit
                            // leaves on an inaccessible page.
                            let newest = column.get(len - 1).load(Ordering::Acquire);
                            assert!(
                                newest == 0 || newest == len as u64,
                                "round {round}: place {} read {newest}",
                                len - 1
                            );
                            let at = rng.index(len);
                            let got =
                                column.get_checked(at).map_or(0, |it| it.load(Ordering::Acquire));
                            assert!(
                                got == 0 || got == at as u64 + 1,
                                "round {round}: place {at} read {got}"
                            );
                        }
                        raced.fetch_add(moved, Ordering::Relaxed);
                    });
                }
            });
            assert_eq!(
                column.len(),
                CAP,
                "round {round}: the growers grew the column to {} places",
                column.len()
            );
        }
        let raced = raced.load(Ordering::Relaxed);
        assert!(
            raced >= ROUNDS as u64,
            "in {ROUNDS} rounds the readers saw the published length move under them only {raced} times: \
             the growth did not race the readers"
        );
    }

    #[test]
    fn a_read_past_the_published_length_is_refused_inside_the_reservation() {
        let column = Dense::<AtomicU64>::growing(1 << 20, Tag::Query, "a test's column");
        column.ensure(10);
        let len = column.len();
        assert!((10..1 << 20).contains(&len), "published {len} places of 2^20 reserved");
        assert!(column.get_checked(len - 1).is_some());
        assert!(column.get_checked(len).is_none());
        let refused = std::panic::catch_unwind(|| column.get(len).load(Ordering::Relaxed))
            .expect_err("a read at the published length panics");
        let message = refused.downcast_ref::<String>().expect("a formatted panic");
        assert!(
            message.contains(&format!("place {len} of the dense query column of a test's column"))
                && message.contains(&format!("published length of {len} places")),
            "{message}"
        );
        // A place still inside the reservation but past every commit is refused the same way.
        assert!(column.get_checked((1 << 20) - 1).is_none());
        assert!(column.slice(len - 1, 1).len() == 1);
        assert!(std::panic::catch_unwind(|| column.slice(len - 1, 2).len()).is_err());
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
