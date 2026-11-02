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
    sync::atomic::{AtomicPtr, AtomicU32, AtomicU64, Ordering},
};

use blaze_mem::Tag;
use parking_lot::Mutex;

use super::{
    dense::{Dense, Heap, MAX_ALIGN},
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
/// `meta`: made inside a cycle: never backdated, and verified only on the claimed path, which
/// assumes the cycle's other rows (on its stack) unchanged until all of them are proved.
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

/// One function's memo of one key.
#[repr(C)]
pub(crate) struct Row<V> {
    pub(crate) header: Header,
    value: UnsafeCell<MaybeUninit<V>>,
}

impl<V> Row<V> {
    /// The value. The caller saw [`VALUE`] in `verified` with acquire in this view (rule 2),
    /// or is the flight holder of the row (rule 1), or reads a provisional value of its own
    /// cycle (rule 4).
    ///
    /// # Safety
    ///
    /// As above: the value is initialized and no write of it runs.
    #[inline(always)]
    pub(crate) unsafe fn value(&self) -> &V {
        // SAFETY: the caller's contract: initialized, and not written while the reference
        // lives (rules 1 to 4 of the module).
        unsafe { (*self.value.get()).assume_init_ref() }
    }

    /// Writes `value` into a row that holds none.
    ///
    /// # Safety
    ///
    /// The caller holds the row's flight with [`RUNNING`] set, or a provisional row of its own
    /// cycle between rounds, or `&mut` of the storage; the row holds no value.
    pub(crate) unsafe fn put(&self, value: V) {
        // SAFETY: the caller is the row's one writer and no reader reads it (rules 1, 4).
        unsafe { (*self.value.get()).write(value) };
    }

    /// Drops the value the row holds.
    ///
    /// # Safety
    ///
    /// As for [`Row::put`], and the row holds a value, which no live reference reads (rule 3,
    /// or rule 4 for a provisional one).
    pub(crate) unsafe fn drop_value(&self) {
        // SAFETY: the caller's contract: initialized, written by nobody else, read by nobody.
        unsafe { (*self.value.get()).assume_init_drop() };
    }
}

/// An ingredient's rows: one page of [`PAGE_LEN`] rows of `size` bytes per ID page that has
/// any, found through a directory committed whole (one load, no bounds load).
pub(crate) struct Pages {
    dir: Dense<AtomicPtr<u8>>,
    grow: Mutex<()>,
}

impl Pages {
    pub(crate) fn new(tag: Tag) -> Pages {
        Pages { dir: Dense::committed(MAX_PAGES, tag), grow: Mutex::new(()) }
    }

    /// The directory's first entry, for a hit path that indexes from it.
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
        self.dir.get(page).store(at.as_ptr(), Ordering::Release);
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
    /// under `&mut` of the storage.
    pub(crate) fn made(&self, pages: usize) -> impl Iterator<Item = (usize, NonNull<u8>)> + '_ {
        (0..pages.min(self.dir.len())).filter_map(|page| {
            NonNull::new(self.dir.get(page).load(Ordering::Relaxed)).map(|at| (page, at))
        })
    }
}

/// Row `slot` of type `R` in a page made for `R`.
///
/// # Safety
///
/// `page` was made by [`Pages::page_or_make`] with `size_of::<R>()` rows, and `slot < PAGE_LEN`.
#[inline(always)]
pub(crate) unsafe fn row_at<'a, R>(page: NonNull<u8>, slot: usize) -> &'a R {
    debug_assert!(slot < PAGE_LEN);
    // SAFETY: the caller's contract, as in `Pages::row`.
    unsafe { &*page.as_ptr().add(slot * size_of::<R>()).cast::<R>() }
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
pub(crate) type Owners = Dense<AtomicU32>;
