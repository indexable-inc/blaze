//! Typed values over [`crate::arena`]'s untyped stores: each store here holds values of one
//! kind only, written whole before they are published and never moved or freed, so a handle to
//! one reads it in place as a `&'static`.
//!
//! - [`Values<T>`] and [`Ptr<T>`]: one `T` a value ([`crate::Interned`]).
//! - [`Slices<H, S>`] and [`SlicePtr<H, S>`]: a header, a length and items, laid out as one value
//!   behind a thin pointer ([`crate::InternedSlice`]).
//! - [`Records`] and [`Record`]: a [`crate::table`] value made by this process, a 16-byte header
//!   and its bytes, and the column from ID to record.
//!
//! One of `intern`'s two unsafe modules (the other is [`crate::space`]). What its unsafe buys: a
//! handle is one pointer, read with no check (the safe version is an index into a `Vec` behind
//! a lock, a bounds check and a load more a read, or a reference count), and a value is written
//! straight into its arena slot (the safe version boxes it first and copies the box's pointer).
//! The store a typed view wraps is private here, so nothing else puts a value of another kind
//! into it.

use std::{
    alloc::Layout,
    marker::PhantomData,
    ptr::{self, NonNull},
};

use crate::{
    arena::{Entry, Store},
    intern_slice::{HeaderSlice, HeaderWithLength},
    space::Slots,
    sync::atomic::Ordering,
    table::MAX_ALIGN,
};

/// A value of a [`Values`] store, or any `&'static T`: a thin pointer read in place.
pub(crate) struct Ptr<T> {
    /// # Invariant
    ///
    /// Was a `&'static T`.
    at: NonNull<T>,
}

impl<T> Clone for Ptr<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> Copy for Ptr<T> {}

// SAFETY: A `Ptr<T>` is a `&'static T`, which is `Send` exactly when `T: Sync`. Without the
// bound a `T` that is not `Sync` (a `Cell`) could be read from two threads.
unsafe impl<T: Sync> Send for Ptr<T> {}
// SAFETY: As for `Send`: sharing a `&'static T` is sound exactly when `T: Sync`.
unsafe impl<T: Sync> Sync for Ptr<T> {}

impl<T> Ptr<T> {
    #[inline]
    pub(crate) fn new(value: &'static T) -> Ptr<T> {
        Ptr { at: NonNull::from(value) }
    }

    /// The value, for any lifetime: it lives for the process.
    #[inline]
    pub(crate) fn get<'a>(self) -> &'a T {
        // SAFETY: `at` was a `&'static T` (the invariant: `new` takes nothing else), so the `T`
        // lives, unwritten, for the process. A shorter-lived pointer would be read after it was
        // freed.
        unsafe { self.at.as_ref() }
    }

    #[inline]
    pub(crate) fn as_ptr(self) -> *const T {
        self.at.as_ptr()
    }
}

/// A store of `T`s, each interned once.
pub(crate) struct Values<T: ?Sized> {
    store: Store,
    _of: PhantomData<fn() -> *const T>,
}

impl<T: ?Sized> Values<T> {
    pub(crate) fn new() -> Values<T> {
        Values { store: Store::new(), _of: PhantomData }
    }

    /// How many values the store holds.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.store.len()
    }
}

/// `T: Sync`: the value is read from whichever thread finds it.
impl<T: Eq + Sync + 'static> Values<T> {
    /// The one copy of `obj` in the store, under `hash`, moved there if it is not; `obj` is
    /// dropped when a copy is there already.
    #[inline]
    pub(crate) fn intern(&self, hash: u64, obj: T) -> &'static T {
        let same = |at: NonNull<u8>| {
            // SAFETY: `at` is a published value of this store (`Store::entry`'s contract), and
            // every value of it is a `T` written whole by `intern` below before it was published,
            // never written again, moved or freed. A value of another type would be read as a
            // `T`.
            let other = unsafe { at.cast::<T>().as_ref() };
            *other == obj
        };
        let at = match self.store.entry(hash, same) {
            Entry::Found(at) => at,
            Entry::Vacant(vacant) => vacant.insert(Layout::new::<T>(), |to| {
                // SAFETY: `insert` hands `write` fresh memory of `T`'s layout, aligned and
                // written by no one else, so writing a `T` there drops nothing and races with
                // no one.
                unsafe { to.cast::<T>().write(obj) }
            }),
        };
        // SAFETY: As in `same`: a published value of this store, a `T` that lives, unwritten, for
        // the process.
        unsafe { at.cast::<T>().as_ref() }
    }
}

/// What a [`Slices`] value is: its header and length, then its items.
pub(crate) type Pointee<H, S> = HeaderSlice<HeaderWithLength<H>, [S]>;

/// A value of a [`Slices`] store: a thin pointer to a [`Pointee`], read in place.
pub(crate) struct SlicePtr<H, S> {
    /// # Invariant
    ///
    /// The start of a published `Pointee<H, S>` of a [`Slices<H, S>`], never moved or freed.
    at: NonNull<u8>,
    _of: PhantomData<fn() -> (H, S)>,
}

impl<H, S> Clone for SlicePtr<H, S> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<H, S> Copy for SlicePtr<H, S> {}

// SAFETY: A `SlicePtr` is a shared reference to a header and items that live for the process
// and are never written again, which is `Send` exactly when both are `Sync`.
unsafe impl<H: Sync, S: Sync> Send for SlicePtr<H, S> {}
// SAFETY: As for `Send`: sharing one shares only `&H` and `&[S]`, sound when both are `Sync`.
unsafe impl<H: Sync, S: Sync> Sync for SlicePtr<H, S> {}

impl<H, S> SlicePtr<H, S> {
    /// The header, length and items, for any lifetime: they live for the process.
    #[inline]
    pub(crate) fn get<'a>(self) -> &'a Pointee<H, S> {
        // SAFETY: `at` starts a published `Pointee<H, S>` (the invariant: only `Slices::intern`
        // makes one).
        unsafe { pointee::<H, S>(self.at) }
    }

    #[inline]
    pub(crate) fn addr(self) -> usize {
        self.at.as_ptr().addr()
    }
}

impl<H, S> PartialEq for SlicePtr<H, S> {
    #[inline]
    fn eq(&self, other: &Self) -> bool {
        self.at == other.at
    }
}

impl<H, S> Eq for SlicePtr<H, S> {}

/// A store of header-and-items values, each interned once.
pub(crate) struct Slices<H, S> {
    store: Store,
    _of: PhantomData<fn() -> (H, S)>,
}

impl<H, S> Slices<H, S> {
    pub(crate) fn new() -> Slices<H, S> {
        Slices { store: Store::new(), _of: PhantomData }
    }

    /// How many values the store holds.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.store.len()
    }
}

/// `H, S: Sync`: the value is read from whichever thread finds it.
impl<H: Eq + Sync, S: Eq + Copy + Sync> Slices<H, S> {
    /// The one copy of `header` and `slice` in the store, under `hash`, written there if it is
    /// not; `header` is dropped when a copy is there already.
    #[inline]
    pub(crate) fn intern(&self, hash: u64, header: H, slice: &[S]) -> SlicePtr<H, S> {
        let same = |at: NonNull<u8>| {
            // SAFETY: `at` is a published value of this store (`Store::entry`'s contract), and
            // every value of it is a `Pointee<H, S>` that `intern` wrote whole before publishing.
            let other = unsafe { pointee::<H, S>(at) };
            other.header.header == header && other.slice == *slice
        };
        let at = match self.store.entry(hash, same) {
            Entry::Found(at) => at,
            Entry::Vacant(vacant) => vacant.insert(layout::<H, S>(slice.len()), |to| {
                // SAFETY: `insert` hands `write` fresh memory of `layout::<H, S>(slice.len())`,
                // written by no one else.
                unsafe { write::<H, S>(to, header, slice) }
            }),
        };
        SlicePtr { at, _of: PhantomData }
    }
}

/// The layout of a `Pointee<H, S>` of `len` items, as `repr(C)` lays it out.
fn layout<H, S>(len: usize) -> Layout {
    let (layout, _) = Layout::new::<HeaderWithLength<H>>()
        .extend(Layout::array::<S>(len).expect("an interned slice's layout"))
        .expect("an interned slice's layout");
    layout.pad_to_align()
}

/// A fat pointer to the `Pointee<H, S>` at `at`, of `len` items.
fn fat<H, S>(at: NonNull<u8>, len: usize) -> *mut Pointee<H, S> {
    ptr::slice_from_raw_parts_mut(at.as_ptr().cast::<S>(), len) as *mut Pointee<H, S>
}

/// Writes `header`, `slice.len()` and `slice`'s items at `to` as one `Pointee<H, S>`.
///
/// # Safety
///
/// `to` is writable memory of `layout::<H, S>(slice.len())` that nothing else reads or writes
/// until the value is published.
unsafe fn write<H, S: Copy>(to: NonNull<u8>, header: H, slice: &[S]) {
    let fat = fat::<H, S>(to, slice.len());
    // SAFETY: `fat` points at writable memory of `layout::<H, S>(slice.len())` (the
    // precondition), so its `header` field is in bounds and aligned (`repr(C)`); this takes the
    // field's address without reading it.
    let header_at = unsafe { &raw mut (*fat).header };
    // SAFETY: `header_at` is in bounds, aligned and not initialized yet; `write` drops nothing.
    unsafe { header_at.write(HeaderWithLength { header, length: slice.len() }) };
    // SAFETY: As for `header_at`: the address of the `slice` field, `slice.len()` items long.
    let items_at = unsafe { &raw mut (*fat).slice }.cast::<S>();
    // SAFETY: `items_at` has room for `slice.len()` items and is aligned for them, and it is
    // fresh memory, so it cannot overlap `slice`; `S: Copy`, so copying the bytes is a copy.
    unsafe { items_at.copy_from_nonoverlapping(slice.as_ptr(), slice.len()) };
}

/// The `Pointee<H, S>` at `at`.
///
/// # Safety
///
/// `at` points at the start of a published `Pointee<H, S>` of a [`Slices<H, S>`], which lives,
/// unwritten, for the process.
unsafe fn pointee<'a, H, S>(at: NonNull<u8>) -> &'a Pointee<H, S> {
    let header = at.cast::<HeaderWithLength<H>>().as_ptr();
    // SAFETY: `at` starts a published `Pointee<H, S>` (the precondition), which starts with its
    // initialized `HeaderWithLength`; this takes the `length` field's address.
    let length_at = unsafe { &raw const (*header).length };
    // SAFETY: `length_at` is the initialized `length` of a value never written again.
    let length = unsafe { length_at.read() };
    // SAFETY: `length` is the value's own item count, so the fat pointer covers exactly the
    // value, which lives for the process. A wrong length would read past it.
    unsafe { &*fat::<H, S>(at, length) }
}

/// A record's header: its order key, ID and length, then its bytes, in one allocation.
#[repr(C, align(16))]
struct Head {
    key: u64,
    id: u32,
    len: u32,
}

const _: () = assert!(size_of::<Head>() == MAX_ALIGN);

fn record_layout(len: usize) -> Layout {
    Layout::from_size_align(size_of::<Head>() + len, MAX_ALIGN).expect("an interned value's layout")
}

/// A value of a [`Records`] store: its ID, order key and bytes, read in place.
#[derive(Clone, Copy)]
pub(crate) struct Record {
    /// # Invariant
    ///
    /// A header that `Records::intern` wrote, with its `len` bytes right after it. A raw
    /// pointer, not a `&'static Head`: the bytes are read through it, past the header, which a
    /// reference's provenance would not cover.
    head: NonNull<Head>,
}

impl Record {
    #[inline]
    fn head(self) -> &'static Head {
        // SAFETY: `head` is a header `Records::intern` wrote before publishing it (the
        // invariant), never written again, moved or freed.
        unsafe { self.head.as_ref() }
    }

    #[inline]
    pub(crate) fn id(self) -> u32 {
        self.head().id
    }

    #[inline]
    pub(crate) fn key(self) -> u64 {
        self.head().key
    }

    #[inline]
    pub(crate) fn bytes(self) -> &'static [u8] {
        let after = self.head.as_ptr().wrapping_add(1).cast::<u8>();
        // SAFETY: `Records::intern` allocates `len` bytes right after the header, in the same
        // allocation, and writes them before publishing it (the invariant); the arena never
        // writes, moves or frees them. A header of another allocation would read its neighbour.
        unsafe { std::slice::from_raw_parts(after, self.head().len as usize) }
    }

    /// The record at `at`.
    ///
    /// # Safety
    ///
    /// `at` is a published value of a [`Records`] store, or a pointer its column holds: a
    /// `Head` and its bytes, never moved or freed. The record's reads rest on it.
    #[inline]
    unsafe fn at(at: NonNull<u8>) -> Record {
        Record { head: at.cast() }
    }
}

/// The values a [`crate::table::Table`] makes in this process, each a header and its bytes,
/// with the column from ID to record: slot `id - first` for an ID not compiled in, set once,
/// before the store publishes the value, so a reader that finds the ID finds its bytes.
pub(crate) struct Records {
    store: Store,
    column: Slots<Head>,
    /// IDs below this are compiled in, kept out of the column.
    first: u32,
}

impl Records {
    pub(crate) fn new(first: u32) -> Records {
        Records { store: Store::new(), column: Slots::new(), first }
    }

    /// The record of `bytes`, under `hash`.
    #[inline]
    pub(crate) fn find(&self, hash: u64, bytes: &[u8]) -> Option<Record> {
        // SAFETY: `find` hands `matches` and returns published values of this store only.
        let same = |at: NonNull<u8>| unsafe { Record::at(at) }.bytes() == bytes;
        let at = self.store.find(hash, same)?;
        // SAFETY: As in `same`.
        Some(unsafe { Record::at(at) })
    }

    /// The record of `bytes`, under `hash`, written with the ID and order key `make` gives if
    /// there is none. `make` runs under the shard's lock, once, only when the bytes are new.
    pub(crate) fn intern(&self, hash: u64, bytes: &[u8], make: impl FnOnce() -> (u32, u64)) -> Record {
        // SAFETY: As in `find`: `entry` hands `matches` and returns published values only.
        let same = |at: NonNull<u8>| unsafe { Record::at(at) }.bytes() == bytes;
        let at = match self.store.entry(hash, same) {
            Entry::Found(at) => at,
            Entry::Vacant(vacant) => {
                let (id, key) = make();
                let len = u32::try_from(bytes.len()).expect("an interned value of 4 GiB or more");
                vacant.insert(record_layout(bytes.len()), |to| self.write(to, Head { key, id, len }, bytes))
            }
        };
        // SAFETY: As in `same`.
        unsafe { Record::at(at) }
    }

    /// Writes `head` and `bytes` at `to`, fresh memory of `record_layout(bytes.len())`, and sets
    /// the column's slot for an ID made here.
    fn write(&self, to: NonNull<u8>, head: Head, bytes: &[u8]) {
        let (id, at) = (head.id, to.cast::<Head>());
        // SAFETY: `to` is fresh memory of `record_layout(len)` that `Vacant::insert` handed us,
        // aligned for a `Head` and with room for it and `len` bytes after it, written by no one
        // else.
        unsafe { at.write(head) };
        let after = at.as_ptr().wrapping_add(1).cast::<u8>();
        // SAFETY: the `len` bytes after the header are in the allocation and fresh, so they
        // cannot overlap `bytes`.
        unsafe { after.copy_from_nonoverlapping(bytes.as_ptr(), bytes.len()) };
        if id >= self.first {
            // Release: a reader that loads the slot sees the header and bytes written above.
            self.column.ensure((id - self.first) as usize).store(at.as_ptr(), Ordering::Release);
        }
    }

    /// The record of `id`, an ID made by this process; `None` when it never was.
    #[inline]
    pub(crate) fn get(&self, id: u32) -> Option<Record> {
        let index = id.checked_sub(self.first)? as usize;
        let at = NonNull::new(self.column.get(index)?.load(Ordering::Acquire))?;
        // SAFETY: only `write` sets a slot, to a header it wrote with its bytes before the store
        // published it, never moved or freed.
        Some(unsafe { Record::at(at.cast()) })
    }
}
