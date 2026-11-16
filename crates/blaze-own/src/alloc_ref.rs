//! The allocator as a value of one type: [`Alloc<'a>`], one machine word, `Copy`, an
//! [`Allocator`] (docs/v2/plan/design/std.md, "Memory is explicit", "One handle, not a type
//! parameter").
//!
//! A function that allocates takes `alloc: Alloc<'_>`, not `<A: Allocator>(alloc: A)`: it is
//! type-checked and compiled once, it can be a plain function pointer or a stored task, and its
//! signature says what it does. Collections stay generic underneath (`Vec<T, A>`), so a hot loop
//! that wants a bump allocation inlined still names `&Frame`; the aliases here ([`AVec`] and the
//! rest) name the collections over the handle.
//!
//! # Layout
//!
//! An allocator that hands out a handle is `#[repr(C)]` with a [`Header`] as its first field;
//! the header is one pointer to a [`Table`] of six function pointers (allocate, allocate zeroed,
//! deallocate, grow, grow zeroed, shrink), each taking the header's address. The handle is that
//! address and a lifetime that borrows the allocator. A call through it is one load of the
//! table, one load of the entry and one indirect call; the entry casts the header's address back
//! to the allocator and calls its method, which inlines there.
//!
//! Any allocator becomes one through [`Erased<A>`] (a header, then `A`). A type of its own can
//! carry the header itself (a frame, say) by implementing [`Headed`]: then `Alloc::of(&it)` is
//! the handle, with no wrapper beside it.
//!
//! # Invariants
//!
//! 1. A [`Headed`] type is `#[repr(C)]`, its first field a [`Header`] made by
//!    [`Header::of::<Self>`](Header::of): so the header's address is the value's, and its table's
//!    entries are the ones made for that type.
//! 2. A handle's address is derived from a reference to the whole allocator (never to the
//!    header field alone), so its provenance covers the allocator the entries reach through it.
//! 3. A handle's lifetime borrows that allocator: it stays at its address and alive (and is not
//!    mutably borrowed) while any copy of the handle exists. Copies are the same allocator, so a
//!    block made through one may be freed or resized through any other ([`AllocatorClone`]).
//!
//! # Threads
//!
//! `Alloc` is neither `Send` nor `Sync`: it may stand for an allocator one thread owns (a task's
//! frame, an [`Inline`](crate::Inline) buffer), and the one type cannot say which it is. A value
//! that crosses threads names a `Sync` allocator concretely (`Vec<T, Global>`), or takes a fresh
//! handle on the other side.

use core::{
    alloc::{AllocError, Allocator, AllocatorClone, Layout},
    fmt,
    marker::PhantomData,
    ptr::NonNull,
};

use crate::{Arc, BTreeMap, BTreeSet, Box, Rc, Soa, SortedMap, SortedSet, String, Vec, VecDeque};
#[cfg(test)]
use crate::plant::{self, Plant};

/// A block allocated or resized: the result of every allocating entry.
pub type Grant = Result<NonNull<[u8]>, AllocError>;

/// What an allocator does, as function pointers over its [`Header`].
///
/// Each entry's contract: `header` heads (invariant 1) a live allocator this table was made for,
/// with provenance over all of it (invariant 2); the rest is the contract of the [`Allocator`]
/// method of the same name.
pub struct Table {
    pub allocate: unsafe fn(header: NonNull<Header>, layout: Layout) -> Grant,
    pub allocate_zeroed: unsafe fn(header: NonNull<Header>, layout: Layout) -> Grant,
    pub deallocate: unsafe fn(header: NonNull<Header>, ptr: NonNull<u8>, layout: Layout),
    pub grow: unsafe fn(header: NonNull<Header>, ptr: NonNull<u8>, old: Layout, new: Layout) -> Grant,
    pub grow_zeroed: unsafe fn(header: NonNull<Header>, ptr: NonNull<u8>, old: Layout, new: Layout) -> Grant,
    pub shrink: unsafe fn(header: NonNull<Header>, ptr: NonNull<u8>, old: Layout, new: Layout) -> Grant,
}

/// The first field of an allocator that hands out [`Alloc`] handles: its table.
#[repr(C)]
pub struct Header {
    table: &'static Table,
}

impl Header {
    /// The header of a `T`, whose table's entries call `T`'s methods.
    pub const fn of<T: Headed>() -> Header {
        Header { table: <T as Entries>::TABLE }
    }
}

impl fmt::Debug for Header {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Header")
    }
}

/// An allocator that carries its own [`Header`], so a handle to it needs no wrapper.
///
/// # Safety
///
/// The type is `#[repr(C)]` and its first field is a [`Header`] made by
/// [`Header::of::<Self>()`](Header::of), never changed while a handle to the value exists.
pub unsafe trait Headed: Allocator {}

/// The table of a [`Headed`] type's entries (a private trait, so no type can name another).
trait Entries {
    const TABLE: &'static Table;
}

impl<T: Headed> Entries for T {
    const TABLE: &'static Table = &Table {
        allocate: allocate::<T>,
        allocate_zeroed: allocate_zeroed::<T>,
        deallocate: deallocate::<T>,
        grow: grow::<T>,
        grow_zeroed: grow_zeroed::<T>,
        shrink: shrink::<T>,
    };
}

/// The allocator a header heads.
///
/// # Safety
///
/// The entries' contract ([`Table`]) for a table made for `T`; the reference is used only for
/// the one call.
#[inline(always)]
unsafe fn this<'t, T: Headed>(header: NonNull<Header>) -> &'t T {
    // SAFETY: the header is the first field of a `#[repr(C)]` `T` (invariant 1), so its address
    // is the `T`'s; the pointer's provenance covers the whole `T` (invariant 2); the `T` is live
    // and not mutably borrowed while the handle exists (invariant 3).
    unsafe { header.cast::<T>().as_ref() }
}

unsafe fn allocate<T: Headed>(header: NonNull<Header>, layout: Layout) -> Grant {
    // SAFETY: the entries' contract, passed on.
    unsafe { this::<T>(header) }.allocate(layout)
}

unsafe fn allocate_zeroed<T: Headed>(header: NonNull<Header>, layout: Layout) -> Grant {
    // SAFETY: the entries' contract, passed on.
    unsafe { this::<T>(header) }.allocate_zeroed(layout)
}

unsafe fn deallocate<T: Headed>(header: NonNull<Header>, ptr: NonNull<u8>, layout: Layout) {
    // SAFETY: the entries' contract, passed on; `deallocate`'s own is the caller's.
    unsafe { this::<T>(header).deallocate(ptr, layout) }
}

unsafe fn grow<T: Headed>(header: NonNull<Header>, ptr: NonNull<u8>, old: Layout, new: Layout) -> Grant {
    // SAFETY: the entries' contract, passed on.
    let this = unsafe { this::<T>(header) };
    // The planted bug of the handle's control: growth as a fresh allocation, the old bytes lost
    // (and the old block never freed).
    #[cfg(test)]
    if plant::on(Plant::HandleGrowAllocates) {
        return this.allocate(new);
    }
    // SAFETY: `grow`'s contract is the caller's.
    unsafe { this.grow(ptr, old, new) }
}

unsafe fn grow_zeroed<T: Headed>(header: NonNull<Header>, ptr: NonNull<u8>, old: Layout, new: Layout) -> Grant {
    // SAFETY: the entries' contract, passed on; `grow_zeroed`'s own is the caller's.
    unsafe { this::<T>(header).grow_zeroed(ptr, old, new) }
}

unsafe fn shrink<T: Headed>(header: NonNull<Header>, ptr: NonNull<u8>, old: Layout, new: Layout) -> Grant {
    // SAFETY: the entries' contract, passed on; `shrink`'s own is the caller's.
    unsafe { this::<T>(header).shrink(ptr, old, new) }
}

/// The allocator as a value: one word, `Copy`, borrowing the allocator it stands for for `'a`
/// (see the module).
#[derive(Clone, Copy)]
pub struct Alloc<'a> {
    /// The allocator's address, which is its header's (invariants 1 and 2).
    header: NonNull<Header>,
    borrow: PhantomData<&'a Header>,
}

const _: () = assert!(size_of::<Alloc<'static>>() == size_of::<usize>());
const _: () = assert!(size_of::<Option<Alloc<'static>>>() == size_of::<usize>());

impl<'a> Alloc<'a> {
    /// The handle of an allocator that carries its own header.
    #[inline]
    pub fn of<T: Headed>(allocator: &'a T) -> Alloc<'a> {
        // SAFETY: `T: Headed` (invariant 1); the address is the whole `T`'s, from a reference
        // to it (invariant 2); the handle borrows it for `'a` (invariant 3).
        unsafe { Alloc::from_header(NonNull::from(allocator).cast()) }
    }

    /// A handle from a header's address.
    ///
    /// # Safety
    ///
    /// `header` heads a live allocator its table was made for (invariant 1), its provenance
    /// covers that whole allocator (invariant 2), and the allocator stays at its address, alive
    /// and not mutably borrowed for `'a` (invariant 3).
    #[inline]
    pub const unsafe fn from_header(header: NonNull<Header>) -> Alloc<'a> {
        Alloc { header, borrow: PhantomData }
    }

    /// Whether `self` and `other` are handles of one allocator value.
    #[inline]
    pub fn same(self, other: Alloc<'_>) -> bool {
        self.header == other.header
    }

    #[inline(always)]
    fn table(self) -> &'static Table {
        // SAFETY: the header is live and not mutably borrowed while the handle exists
        // (invariant 3).
        unsafe { self.header.as_ref() }.table
    }
}

#[cfg(any(test, feature = "std"))]
mod global {
    use std::alloc::Global;

    use super::{Alloc, Erased};

    /// The global allocator, erased once for the whole program.
    static GLOBAL: Erased<Global> = Erased::new(Global);

    impl Alloc<'static> {
        /// The global allocator's handle: named in tests and at a program's top only, which
        /// hands it (or a frame's) down (the `std` feature).
        #[inline]
        pub fn global() -> Alloc<'static> {
            GLOBAL.handle()
        }
    }
}

impl fmt::Debug for Alloc<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Alloc").field(&self.header).finish()
    }
}

// SAFETY: every method forwards to the entry of the allocator the handle stands for, whose
// contract (`Table`) the handle's invariants hold; the caller's part of each method's contract
// is passed on unchanged, and every copy of the handle is the same allocator.
unsafe impl Allocator for Alloc<'_> {
    #[inline]
    fn allocate(&self, layout: Layout) -> Grant {
        // SAFETY: the handle's invariants are the entry's contract.
        unsafe { (self.table().allocate)(self.header, layout) }
    }

    #[inline]
    fn allocate_zeroed(&self, layout: Layout) -> Grant {
        // SAFETY: the handle's invariants are the entry's contract.
        unsafe { (self.table().allocate_zeroed)(self.header, layout) }
    }

    #[inline]
    unsafe fn deallocate(&self, ptr: NonNull<u8>, layout: Layout) {
        // SAFETY: the handle's invariants, and the caller's contract for `deallocate`.
        unsafe { (self.table().deallocate)(self.header, ptr, layout) }
    }

    #[inline]
    unsafe fn grow(&self, ptr: NonNull<u8>, old: Layout, new: Layout) -> Grant {
        // SAFETY: the handle's invariants, and the caller's contract for `grow`.
        unsafe { (self.table().grow)(self.header, ptr, old, new) }
    }

    #[inline]
    unsafe fn grow_zeroed(&self, ptr: NonNull<u8>, old: Layout, new: Layout) -> Grant {
        // SAFETY: the handle's invariants, and the caller's contract for `grow_zeroed`.
        unsafe { (self.table().grow_zeroed)(self.header, ptr, old, new) }
    }

    #[inline]
    unsafe fn shrink(&self, ptr: NonNull<u8>, old: Layout, new: Layout) -> Grant {
        // SAFETY: the handle's invariants, and the caller's contract for `shrink`.
        unsafe { (self.table().shrink)(self.header, ptr, old, new) }
    }
}

// SAFETY: a copy is the same address, so the same allocator (invariant 3); copying or dropping
// a handle touches nothing of it.
unsafe impl AllocatorClone for Alloc<'_> {}

/// Any allocator with a [`Header`] before it, so it hands out [`Alloc`] handles:
/// `Erased::new(&frame).handle()`, `Erased::new(&counting).handle()`.
#[repr(C)]
pub struct Erased<A: Allocator> {
    /// First, so its address is the value's (invariant 1).
    header: Header,
    inner: A,
}

impl<A: Allocator> Erased<A> {
    pub const fn new(inner: A) -> Erased<A> {
        Erased { header: Header::of::<Erased<A>>(), inner }
    }

    /// The handle, borrowing `self`.
    #[inline]
    pub fn handle(&self) -> Alloc<'_> {
        Alloc::of(self)
    }

    pub fn inner(&self) -> &A {
        &self.inner
    }

    pub fn into_inner(self) -> A {
        self.inner
    }
}

impl<A: Allocator + fmt::Debug> fmt::Debug for Erased<A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Erased").field(&self.inner).finish()
    }
}

// SAFETY: every method is `inner`'s, passed through unchanged.
unsafe impl<A: Allocator> Allocator for Erased<A> {
    #[inline(always)]
    fn allocate(&self, layout: Layout) -> Grant {
        self.inner.allocate(layout)
    }

    #[inline(always)]
    fn allocate_zeroed(&self, layout: Layout) -> Grant {
        self.inner.allocate_zeroed(layout)
    }

    #[inline(always)]
    unsafe fn deallocate(&self, ptr: NonNull<u8>, layout: Layout) {
        // SAFETY: the caller's contract, passed on.
        unsafe { self.inner.deallocate(ptr, layout) }
    }

    #[inline(always)]
    unsafe fn grow(&self, ptr: NonNull<u8>, old: Layout, new: Layout) -> Grant {
        // SAFETY: the caller's contract, passed on.
        unsafe { self.inner.grow(ptr, old, new) }
    }

    #[inline(always)]
    unsafe fn grow_zeroed(&self, ptr: NonNull<u8>, old: Layout, new: Layout) -> Grant {
        // SAFETY: the caller's contract, passed on.
        unsafe { self.inner.grow_zeroed(ptr, old, new) }
    }

    #[inline(always)]
    unsafe fn shrink(&self, ptr: NonNull<u8>, old: Layout, new: Layout) -> Grant {
        // SAFETY: the caller's contract, passed on.
        unsafe { self.inner.shrink(ptr, old, new) }
    }
}

// SAFETY: `#[repr(C)]`, the header first, made by `Header::of::<Erased<A>>()` in `new`, the only
// constructor, and never written after (the field is private).
unsafe impl<A: Allocator> Headed for Erased<A> {}

/// A [`Vec`] over the handle.
pub type AVec<'a, T> = Vec<T, Alloc<'a>>;
/// A [`String`] over the handle.
pub type AString<'a> = String<Alloc<'a>>;
/// A [`VecDeque`] over the handle.
pub type ADeque<'a, T> = VecDeque<T, Alloc<'a>>;
/// A [`Box`] over the handle (`ABox<'a, str>`, `ABox<'a, [T]>` too).
pub type ABox<'a, T> = Box<T, Alloc<'a>>;
/// An [`Arc`] over the handle.
pub type AArc<'a, T> = Arc<T, Alloc<'a>>;
/// An [`Rc`] over the handle.
pub type ARc<'a, T> = Rc<T, Alloc<'a>>;
/// A [`SortedMap`] over the handle.
pub type ASortedMap<'a, K, V> = SortedMap<K, V, Alloc<'a>>;
/// A [`SortedSet`] over the handle.
pub type ASortedSet<'a, T> = SortedSet<T, Alloc<'a>>;
/// A [`BTreeMap`] over the handle.
pub type ABTreeMap<'a, K, V> = BTreeMap<K, V, Alloc<'a>>;
/// A [`BTreeSet`] over the handle.
pub type ABTreeSet<'a, T> = BTreeSet<T, Alloc<'a>>;
/// A [`Soa`] over the handle; a table made by `soa!` is `Rows<Alloc<'a>>`.
pub type ASoa<'a, C> = Soa<C, Alloc<'a>>;
