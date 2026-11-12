//! Rows: a tracked function's memo of one key, in place in a page of its ingredient, found by
//! the key's [`Id`] (`page → row page`, `slot → row`). The one unsafe concern of a value kept in
//! place and read while other tasks run.
//!
//! # The value-in-place rule
//!
//! A row's value is an `UnsafeCell<MaybeUninit<V>>` beside its [`Header`]. It is sound to hand
//! out `&'db V` from `&'db Storage` because:
//!
//! 1. **Written only by the flight holder.** A value is written (or dropped) only by the task
//!    that holds the key's flight ([`blaze_graph::pump::Flights`]) after it set
//!    [`RUNNING`] in `verified` with a compare-and-swap from a word naming an earlier view, or
//!    with no [`VALUE`]; under `&mut Storage` (eviction, compaction, drop), where nothing reads.
//! 2. **Read only once published in this view.** A reader reads the value only after loading
//!    `verified == view | VALUE` with acquire, which the writer stored with release after the
//!    value: the value is whole and stays so for the view, as no writer may start
//!    (its compare-and-swap needs a word of an earlier view or no value).
//! 3. **A view ends only under `&mut`.** The view is bumped only by a write through
//!    `Database::storage_mut`, which ends every `&'db` borrow of the storage, so no reference
//!    of an earlier view is alive when a value of that view is dropped or replaced.
//! 4. **Provisional values are their cycle's thread's.** A [`PROVISIONAL`] value (fixpoint
//!    iteration) is written and read only by the task whose stack holds its cycle's head, and
//!    replaced only between rounds, when every frame that read the last round's has returned;
//!    other tasks never read it, they wait on the head's flight.
//!
//! Every `unsafe` block below names which of these it rests on.

use std::{
    cell::UnsafeCell,
    mem::MaybeUninit,
    ptr::NonNull,
    sync::atomic::{AtomicPtr, AtomicU32, AtomicU64, AtomicUsize, Ordering},
};

use blaze_mem::Tag;

use super::{
    dense::{Heap, MAX_ALIGN, Whole},
    id::{Id, MAX_PAGES, PAGE_LEN},
};

/// The view bits of `verified`: views run from 1 to this.
pub(crate) const VIEW_MASK: u32 = (1 << 28) - 1;
/// The row holds a value.
pub(crate) const VALUE: u32 = 1 << 28;
/// The flight holder is writing the row: only it may touch the value.
pub(crate) const RUNNING: u32 = 1 << 29;
/// The value is a cycle's, not final: only its head's task reads it.
pub(crate) const PROVISIONAL: u32 = 1 << 30;

/// `meta`: the row's durability (the lowest kind it read).
pub(crate) const DURABILITY: u32 = 0b11;
/// `meta`: made inside a cycle: never backdated. Once final it is verified as any row is, by
/// its edges: a head's are what its whole cycle read outside itself over every round, in read
/// order (a run's reads sit where its reader read it; a run met only by a verify comes after
/// its round), and a member's one edge is its head, so a cycle whose reads are unchanged is
/// reused whole and one whose reads changed runs again from its head (`function::hand_over`,
/// `function::finalize`).
pub(crate) const CYCLE: u32 = 1 << 2;
/// `meta`: made an untracked read, so it runs again in every view.
pub(crate) const UNTRACKED: u32 = 1 << 3;
/// `meta`: a cycle head (re-entered) whose value carries into the next round.
pub(crate) const HEAD: u32 = 1 << 4;
/// `meta`: a carried head value of an earlier round: run again before it is read.
pub(crate) const STALE: u32 = 1 << 5;
/// `meta`: `extra` is the 64-bit hash of the value an eviction dropped.
pub(crate) const HASHED: u32 = 1 << 6;

/// A row's header: the same for every function, so the verify walk reads any row untyped.
#[repr(C, align(8))]
pub(crate) struct Header {
    /// The view the row was last verified in ([`VIEW_MASK`]) and [`VALUE`], [`RUNNING`],
    /// [`PROVISIONAL`].
    pub(crate) verified: AtomicU32,
    /// The view its value last changed in: the fingerprint its readers' edges compare.
    pub(crate) changed: AtomicU32,
    /// Its node in the dependency graph, 0 before its first run.
    pub(crate) node: AtomicU32,
    /// [`DURABILITY`], [`CYCLE`], [`UNTRACKED`], [`HEAD`], [`STALE`], [`HASHED`].
    pub(crate) meta: AtomicU32,
    /// Its edges: a span of the storage's edge array ([`span`]).
    pub(crate) edges: AtomicU64,
    /// The value's hash ([`HASHED`]), or while [`PROVISIONAL`] the node of its cycle's head.
    pub(crate) extra: AtomicU64,
}

const _: () = assert!(size_of::<Header>() == 32);

impl Header {
    #[inline]
    pub(crate) fn durability(&self) -> u8 {
        (self.meta.load(Ordering::Relaxed) & DURABILITY) as u8
    }
}

/// An edge span: start (40 bits) and length (24 bits) in the edge array.
pub(crate) mod span {
    pub(crate) const MAX_LEN: u64 = (1 << 24) - 1;

    #[inline]
    pub(crate) fn pack(start: u64, len: u64) -> u64 {
        assert!(len <= MAX_LEN, "a query read {len} dependencies, more than {MAX_LEN}");
        assert!(start < 1 << 40, "the edge array passed 2^40 edges");
        (start << 24) | len
    }

    #[inline]
    pub(crate) fn start(span: u64) -> u64 {
        span >> 24
    }

    #[inline]
    pub(crate) fn len(span: u64) -> u64 {
        span & MAX_LEN
    }
}

/// The largest value a row keeps in place. A larger one is kept on the global heap and the row
/// holds its address: a row is as long as its value whether it holds one or not, so a function
/// whose values are large and let go (every MIR, at each release: 588 bytes a row of
/// `monomorphized_runtime_mir_body_query`, 3.37 GiB of its 5.73 million rows after their
/// values had gone, lane mem-1111, 2026-11-11) or whose pages are sparsely used (a page is
/// 1024 rows whatever ran) kept the bytes for good. Out of line, a value's bytes go back as
/// it is dropped, and the row is 40 bytes. At most this, a value costs no second load on a hit.
pub(crate) const INLINE_MOST: usize = 128;

/// One function's memo of one key: a handle to its row in a page, a [`Header`] and then the
/// value's place, which holds the value itself ([`INLINE_MOST`] bytes at most) or the address
/// of its own block of the global heap ([`Row::FAR`]).
///
/// The invariant: a row whose `verified` has [`VALUE`] holds, in its place, a whole `V` or the
/// address of a live block made by [`Row::put`] holding a whole `V`, and whoever may read or
/// write the value by the module's rules may read or write the place and that block; a row
/// without [`VALUE`] owns no block.
pub(crate) struct Row<'a, V> {
    pub(crate) header: &'a Header,
    /// The row's first byte, with its page's provenance: the place is past the header.
    at: NonNull<u8>,
    value: std::marker::PhantomData<&'a V>,
}

impl<V> Clone for Row<'_, V> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<V> Copy for Row<'_, V> {}

impl<'a, V> Row<'a, V> {
    /// Whether the value is kept out of line.
    pub(crate) const FAR: bool = size_of::<V>() > INLINE_MOST;
    const PLACE_SIZE: usize = if Self::FAR { size_of::<*mut V>() } else { size_of::<V>() };
    const PLACE_ALIGN: usize = if Self::FAR { align_of::<*mut V>() } else { align_of::<V>() };
    /// Where the place starts in the row.
    const PLACE_AT: usize = size_of::<Header>().next_multiple_of(Self::PLACE_ALIGN);
    /// A row's alignment, and its bytes: what its page is made of ([`Pages::page_or_make`]).
    pub(crate) const ALIGN: usize = if Self::PLACE_ALIGN > align_of::<Header>() {
        Self::PLACE_ALIGN
    } else {
        align_of::<Header>()
    };
    pub(crate) const SIZE: usize =
        (Self::PLACE_AT + Self::PLACE_SIZE).next_multiple_of(Self::ALIGN);
    /// What a value holds on the global heap for being out of line.
    pub(crate) const FAR_BYTES: usize = if Self::FAR { size_of::<V>() } else { 0 };

    /// Row `slot` of a page made for this function's rows.
    ///
    /// # Safety
    ///
    /// `page` was made by [`Pages::page_or_make`] with rows of [`Row::SIZE`] bytes aligned to
    /// [`Row::ALIGN`], `slot < PAGE_LEN`, and `'a` does not outlive the storage that owns it.
    #[inline(always)]
    pub(crate) unsafe fn at(page: NonNull<u8>, slot: usize) -> Row<'a, V> {
        debug_assert!(slot < PAGE_LEN);
        // SAFETY: the caller's contract: the row is inside the page and aligned; its header is
        // all atomics, valid zero or as a writer stored it, and lives as long as the storage.
        let at = unsafe { page.add(slot * Self::SIZE) };
        // SAFETY: as above.
        Row { header: unsafe { at.cast::<Header>().as_ref() }, at, value: std::marker::PhantomData }
    }

    /// The row of `id` in `pages`, this function's, if its page was made.
    #[inline]
    pub(crate) fn of(pages: &'a Pages, id: Id) -> Option<Row<'a, V>> {
        let page = NonNull::new(pages.page(id.page()))?;
        // SAFETY: every caller of one function's `Pages` names its `V`, so the page was made
        // for these rows; `id.slot() < PAGE_LEN`; the page lives as long as the storage's
        // heap, which outlives `pages` (the storage drops both together).
        Some(unsafe { Row::at(page, id.slot()) })
    }

    /// The value's place: the value, or where its address is.
    #[inline(always)]
    fn place(self) -> *mut u8 {
        // SAFETY: inside the row (`PLACE_AT + PLACE_SIZE <= SIZE`), which is inside its page.
        unsafe { self.at.as_ptr().add(Self::PLACE_AT) }
    }

    /// The value. The caller saw [`VALUE`] in `verified` with acquire in this view (rule 2),
    /// or is the flight holder of the row (rule 1), or reads a provisional value of its own
    /// cycle (rule 4).
    ///
    /// # Safety
    ///
    /// As above: the value is initialized and no write of it runs.
    #[inline(always)]
    pub(crate) unsafe fn value(self) -> &'a V {
        let place = self.place();
        // SAFETY: the caller's contract: the place is initialized and not written while the
        // reference lives (rules 1 to 4 of the module); a far value's block is live and whole
        // for as long (the invariant: only `drop_value` frees it, a write).
        unsafe {
            match Self::FAR {
                true => &**place.cast::<*mut V>(),
                false => &*place.cast::<V>(),
            }
        }
    }

    /// Writes `value` into a row that holds none.
    ///
    /// # Safety
    ///
    /// The caller holds the row's flight with [`RUNNING`] set, or a provisional row of its own
    /// cycle between rounds, or `&mut` of the storage; the row holds no value.
    pub(crate) unsafe fn put(self, value: V) {
        let place = self.place();
        // SAFETY: the caller is the row's one writer and no reader reads it (rules 1, 4); the
        // place is the row's own, aligned for what is written.
        unsafe {
            match Self::FAR {
                // alloc: a value past `INLINE_MOST`, its own block until the row drops it
                true => place.cast::<*mut V>().write(Box::into_raw(Box::new(value))),
                false => place.cast::<V>().write(value),
            }
        }
    }

    /// Drops the value the row holds.
    ///
    /// # Safety
    ///
    /// As for [`Row::put`], and the row holds a value, which no live reference reads (rule 3,
    /// or rule 4 for a provisional one).
    pub(crate) unsafe fn drop_value(self) {
        let place = self.place();
        // SAFETY: the caller's contract: initialized, written by nobody else, read by nobody;
        // a far value's address is the block `put` made, freed once, here.
        unsafe {
            match Self::FAR {
                true => drop(Box::from_raw(place.cast::<*mut V>().read())),
                false => place.cast::<V>().drop_in_place(),
            }
        }
    }
}

/// An ingredient's rows: one page of [`PAGE_LEN`] rows of `size` bytes per ID page that has
/// any, found through a directory readable at every ID page (one load, no bounds load), of
/// which only the entries written are committed.
pub(crate) struct Pages {
    dir: Whole<AtomicPtr<u8>, MAX_PAGES>,
    #[expect(clippy::disallowed_types, reason = "held only while a row page is made")]
    grow: blaze_core::lock::Lock<()>,
    /// Pages made: what the memory report counts without reading the directory.
    count: AtomicUsize,
}

impl Pages {
    #[expect(clippy::disallowed_types, reason = "makes the locks its fields allow")]
    pub(crate) fn new(tag: Tag) -> Pages {
        Pages {
            dir: Whole::sparse(tag, "an ingredient's row page directory"),
            grow: blaze_core::lock::Lock::new("query.rows.grow", ()),
            count: AtomicUsize::new(0),
        }
    }

    /// How many pages were made.
    pub(crate) fn count(&self) -> usize {
        self.count.load(Ordering::Relaxed)
    }

    /// The directory's first entry, for a hit path that indexes from it: the directory is a
    /// [`Whole`] column of `MAX_PAGES` entries, a sparse reservation readable and writable whole
    /// from [`Pages::new`] until the `Pages` is dropped (`Dense::sparse`'s invariant), so
    /// `dir.add(i)` is a valid entry for every `i < MAX_PAGES`, such as any `Id`'s `page()`
    /// (masked below `MAX_PAGES`): null until [`Pages::page_or_make`] commits and stores it.
    /// The unchecked reads in `function::fetch`, `function::member_header` and
    /// `verify::function_header` rest on this.
    #[inline(always)]
    pub(crate) fn dir(&self) -> NonNull<AtomicPtr<u8>> {
        self.dir.base()
    }

    /// The row page of ID page `page`, null when it has none.
    #[inline]
    pub(crate) fn page(&self, page: usize) -> *mut u8 {
        self.dir.get(page).load(Ordering::Acquire)
    }

    /// The row page of ID page `page`, made of zeroes from `heap` when it has none.
    pub(crate) fn page_or_make(
        &self,
        page: usize,
        heap: &Heap,
        size: usize,
        align: usize,
    ) -> NonNull<u8> {
        if let Some(at) = NonNull::new(self.page(page)) {
            return at;
        }
        let _one = self.grow.lock();
        if let Some(at) = NonNull::new(self.page(page)) {
            return at;
        }
        assert!(page != 0, "a row page for ID page 0, which no ingredient is ever given");
        assert!(align <= MAX_ALIGN, "a row aligned to more than {MAX_ALIGN} bytes");
        let at = heap.alloc(PAGE_LEN * size, align.max(8));
        // Release: a reader that loads the page sees its zeroes, and whatever the maker
        // writes into its rows before publishing them on its own.
        self.dir.get_to_write(page).store(at.as_ptr(), Ordering::Release);
        self.count.fetch_add(1, Ordering::Relaxed);
        at
    }

    /// The row of `id` in a page of rows of type `R`, if its page was made.
    #[inline]
    pub(crate) fn row<R>(&self, id: Id) -> Option<&R> {
        let page = NonNull::new(self.page(id.page()))?;
        // SAFETY: the page was made by `page_or_make` with `size_of::<R>()` per row (every
        // caller of one `Pages` names one `R`), so slot `id.slot() < PAGE_LEN` is inside it and
        // aligned; it is zero or what a writer stored, both valid for the `R`s used here (every
        // field an atomic or an `UnsafeCell<MaybeUninit<_>>`), and lives as long as the heap,
        // which outlives `self`'s users (the storage drops both together).
        Some(unsafe { &*page.as_ptr().add(id.slot() * size_of::<R>()).cast::<R>() })
    }

    /// Every made page's index and base, in ID-page order: for sweeps, compaction and drop,
    /// under `&mut` of the storage, and the memory report's walk under `&` (acquire: a page
    /// seen is seen with its zeroes, as by [`Pages::page`]). Past the directory's committed
    /// prefix nothing was stored.
    pub(crate) fn made(&self, pages: usize) -> impl Iterator<Item = (usize, NonNull<u8>)> + '_ {
        (0..pages.min(self.dir.len())).filter_map(|page| {
            NonNull::new(self.dir.get(page).load(Ordering::Acquire)).map(|at| (page, at))
        })
    }
}

/// What a page of rows of type `R` occupies, and its alignment.
pub(crate) const fn layout<R>() -> (usize, usize) {
    (size_of::<R>(), align_of::<R>())
}

/// The header of row `slot` of a function's page whose rows are `size` bytes, untyped.
///
/// # Safety
///
/// `page` is a function's row page made with rows of `size` bytes, `slot < PAGE_LEN`, and `'a`
/// does not outlive the storage that owns the page.
#[inline(always)]
pub(crate) unsafe fn header_at<'a>(page: NonNull<u8>, slot: usize, size: usize) -> &'a Header {
    debug_assert!(slot < PAGE_LEN);
    // SAFETY: every function row is `#[repr(C)]` with its `Header` first, `size` is the row's
    // size as its ingredient made the page, and `slot < PAGE_LEN`; the header is all atomics,
    // valid zero, and the page lives as long as the storage (the caller's `'a`).
    unsafe { &*page.as_ptr().add(slot * size).cast::<Header>() }
}

/// A column of one `AtomicU32` per ID page: the ingredient owning it (0 for none).
pub(crate) type Owners = Whole<AtomicU32, MAX_PAGES>;

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    static DROPS: AtomicUsize = AtomicUsize::new(0);

    /// A value that counts its drops, `N` words long.
    struct Counted<const N: usize>([u64; N]);

    impl<const N: usize> Drop for Counted<N> {
        fn drop(&mut self) {
            DROPS.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// The invariant: a row holds its value in place up to [`INLINE_MOST`] bytes and past it
    /// the address of the value's own block, which `drop_value` frees with the value; either
    /// way the row reads back what was put, its neighbours untouched, and a row is no longer
    /// than its header and its place.
    #[test]
    fn a_large_value_is_kept_out_of_line_and_a_small_one_in_place() {
        type Near<'a> = Row<'a, Counted<2>>;
        type Far<'a> = Row<'a, Counted<64>>;
        assert!(!Near::FAR && Far::FAR);
        assert_eq!(Near::SIZE, size_of::<Header>() + 16);
        assert_eq!(Far::SIZE, size_of::<Header>() + size_of::<usize>());
        assert_eq!((Far::FAR_BYTES, Near::FAR_BYTES), (512, 0));
        assert!(!Row::<[u8; INLINE_MOST]>::FAR, "a value of the bound stays in place");
        assert_eq!(Row::<u128>::SIZE, 48);
        assert_eq!(Row::<()>::SIZE, size_of::<Header>());

        // A page of three rows of each, zeroes as `Pages::page_or_make` makes it.
        let mut near_page = [0u64; 3 * 6]; // 3 rows of 48 bytes
        let mut far_page = [0u64; 3 * 5]; // 3 rows of 40 bytes
        let near_at = NonNull::from(&mut near_page).cast::<u8>();
        let far_at = NonNull::from(&mut far_page).cast::<u8>();
        let before = DROPS.load(Ordering::Relaxed);
        for slot in 0..3 {
            // SAFETY: pages of three rows of the rows' sizes, 8-aligned; the test is the only
            // reader and writer, and no row holds a value yet.
            unsafe {
                Near::at(near_at, slot).put(Counted([slot as u64, 7]));
                Far::at(far_at, slot).put(Counted([slot as u64 + 100; 64]));
            }
        }
        for slot in 0..3 {
            // SAFETY: each row holds the value put above, which nothing writes.
            unsafe {
                assert_eq!(Near::at(near_at, slot).value().0, [slot as u64, 7]);
                assert_eq!(Far::at(far_at, slot).value().0, [slot as u64 + 100; 64]);
                // The header before each place was not written over.
                assert_eq!(Far::at(far_at, slot).header.node.load(Ordering::Relaxed), 0);
                assert_eq!(Near::at(near_at, slot).header.extra.load(Ordering::Relaxed), 0);
            }
        }
        assert_eq!(DROPS.load(Ordering::Relaxed), before, "a read drops nothing");
        for slot in 0..3 {
            // SAFETY: each row holds a value no reference reads; dropped once.
            unsafe {
                Near::at(near_at, slot).drop_value();
                Far::at(far_at, slot).drop_value();
            }
        }
        assert_eq!(DROPS.load(Ordering::Relaxed), before + 6, "each value dropped once");
    }
}
