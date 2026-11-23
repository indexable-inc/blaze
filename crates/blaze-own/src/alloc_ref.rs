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
//! frame, an [`Inline`](crate::Inline) buffer). [`SharedAlloc`] is the same word made only from a
//! `Sync` allocator (`Erased::shared`, `SharedAlloc::global()`), and is `Send + Sync` (invariant
//! 4); a value that crosses threads holds its collections over it, and passes
//! `shared.local()` to the functions it calls.

use core::{
    alloc::{AllocError, Allocator, AllocatorClone, Layout},
    fmt,
    marker::PhantomData,
    ptr::NonNull,
};

use crate::{
    Arc, BTreeMap, BTreeSet, Box, Lists, Paths, Rc, Soa, SortedMap, SortedSet, String, Texts, ThinArc, ThinArcSlice,
    ThinStr, ThinVec, Vec, VecDeque,
};
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

/// A bump/arena allocator whose allocations are reclaimed in bulk when the arena itself drops or
/// resets (such as `&Frame`, `&Inline<N>`, or `ArenaLocal<'_>`).
///
/// Containers backed by a [`BumpAlloc`] can freeze their backing storage into an arena-lifetime
/// slice or reference ([`crate::Vec::into_bump_slice`], [`crate::String::into_bump_str`],
/// [`crate::Box::into_bump_ref`]) without leaking memory. General-purpose and global allocators
/// (`Global`, `Heap`, `Alloc`, `Root`) must **never** implement this trait.
///
/// # Safety
///
/// Allocations handed out by `Self` stay live for as long as `Self` can be named (`Self: 'a`),
/// and forgetting a container handle backed by `Self` does not leak memory beyond the arena's own
/// lifetime (the arena reclaims its storage in bulk on drop/reset).
pub unsafe trait BumpAlloc: Allocator {}

// SAFETY: delegating through a shared reference preserves the arena's bulk-reclamation contract.
unsafe impl<A: BumpAlloc + ?Sized> BumpAlloc for &A {}

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

    use super::{Alloc, Erased, SharedAlloc};
    use crate::Vec;

    /// The global allocator, erased once for the whole program.
    static GLOBAL: Erased<Global> = Erased::new(Global);

    impl Erased<Global> {
        /// `vec`, made over this allocator's handle, as the same vector over `Global`, its
        /// buffer kept where it is (nothing copied or allocated): a value built through an API
        /// that takes `Alloc<'_>` can then be owned by a type that names `Global` and outlives
        /// the handle's borrow (a task's record in blaze-cli). A vector over any other
        /// allocator is given back unchanged, even one over another `Erased<Global>`: only this
        /// value's handle is known here to free through `Global`.
        pub fn adopt<'v, T>(&self, vec: Vec<T, Alloc<'v>>) -> Result<Vec<T, Global>, Vec<T, Alloc<'v>>> {
            if !vec.allocator().same(self.handle()) {
                return Err(vec);
            }
            let (ptr, len, cap, _this) = vec.into_raw_parts_with_alloc();
            // SAFETY: the buffer was allocated, and is resized and freed, through this value's
            // handle, whose every entry is `Global`'s method passed through unchanged (`Erased`'s
            // `Allocator`), so `Global` is equivalent to that allocator for it
            // (`RawBuf::from_raw_parts_in`); `ptr`, `len` and `cap` are the vector's own, moved
            // out once by `into_raw_parts_with_alloc`.
            Ok(unsafe { Vec::from_raw_parts_in(ptr, len, cap, Global) })
        }
    }

    impl<'a, T> Vec<T, Alloc<'a>> {
        /// The same vector over `Global`, its buffer kept where it is, when its handle is the
        /// heap's ([`Heap::alloc`], or any other handle of the one heap); else given back
        /// unchanged ([`Erased::adopt`] of the one heap).
        pub fn into_global(self) -> Result<Vec<T, Global>, Vec<T, Alloc<'a>>> {
            GLOBAL.adopt(self)
        }
    }

    impl Alloc<'static> {
        /// The global allocator's handle: named in tests and at a program's top only, which
        /// hands it (or a frame's) down (the `std` feature). The kind every function takes; a
        /// value that crosses threads is made over [`SharedAlloc::global`] instead.
        #[inline]
        pub fn global() -> Alloc<'static> {
            GLOBAL.handle()
        }
    }

    impl SharedAlloc<'static> {
        /// The global allocator's handle that may cross threads (the `std` feature).
        #[inline]
        pub fn global() -> SharedAlloc<'static> {
            GLOBAL.shared()
        }
    }

    /// The process's heap as a capability (decision D102, rule 75 "nothing ambient"): the
    /// global allocator, reached only through a `&Heap` its holder was handed. Holding one is
    /// the proof: the field is private, so no code makes a `Heap`; the one value is [`HEAP`],
    /// and only two functions hand out a reference to it: [`Heap::process`], which the
    /// program's entry calls, and [`crate::testing::heap`], a test's entry (the `testing`
    /// feature). Every handle is borrowed from it ([`Heap::alloc`], [`Heap::shared`]); they are
    /// handles of the one global allocator, so a block made through one may be freed through
    /// any other (and through `Alloc::global()`'s, while that still exists).
    pub struct Heap {
        _only_here: (),
    }

    /// The one heap.
    static HEAP: Heap = Heap { _only_here: () };

    impl Heap {
        /// The process's heap, for the program's entry only: `rt::run`, `rt::launch` and
        /// `guest::start`'s caller in bstd, a plain binary's `fn main`. A library never calls
        /// it; it takes `&Heap` or an `Alloc<'_>` from its caller. Which code may call it is
        /// held by the ratchet (tools/ratchets/alloc-global.txt counts every call, per crate),
        /// not by a flag at run time: the hosted start runs no atomic (guest.rs), and taking it
        /// costs nothing.
        #[inline]
        pub const fn process() -> &'static Heap {
            &HEAP
        }

        /// The heap for a test: [`crate::testing::heap`] is the one way to it.
        #[cfg(any(test, feature = "testing"))]
        pub(crate) fn for_tests() -> &'static Heap {
            &HEAP
        }

        /// The heap's handle, borrowed from it: the kind every function takes.
        #[inline]
        pub fn alloc(&self) -> Alloc<'_> {
            GLOBAL.handle()
        }

        /// The heap's handle that may cross threads, borrowed from it.
        #[inline]
        pub fn shared(&self) -> SharedAlloc<'_> {
            GLOBAL.shared()
        }
    }

    impl core::fmt::Debug for Heap {
        fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
            f.write_str("Heap")
        }
    }
}

#[cfg(any(test, feature = "std"))]
pub use global::Heap;

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

/// An [`Alloc`] that may cross threads: made only from an allocator that is `Sync` (the global
/// heap, an `Erased<A>` with `A: Sync`), never from a task's frame or an `Inline` buffer. A
/// collection over it is `Send` when its elements are. A function still takes `Alloc<'_>`:
/// [`SharedAlloc::local`] (or `.into()`) is the same handle, one word, for passing down.
///
/// Invariant 4: the allocator a `SharedAlloc` stands for is `Sync`, so its methods may be called
/// through a shared reference from any thread at once, and its header is never written after
/// it is made; that is all sending or sharing the handle does with it.
#[derive(Clone, Copy)]
pub struct SharedAlloc<'a> {
    alloc: Alloc<'a>,
}

// SAFETY: invariant 4: sending the handle sends a `&T` of a `T: Sync` (`of`'s bound, the only
// constructor), which is `Send`.
unsafe impl Send for SharedAlloc<'_> {}
// SAFETY: invariant 4: a `&SharedAlloc` lets other threads call the allocator through a shared
// reference, which `T: Sync` allows.
unsafe impl Sync for SharedAlloc<'_> {}

impl<'a> SharedAlloc<'a> {
    /// The handle of a `Sync` allocator that carries its own header.
    #[inline]
    pub fn of<T: Headed + Sync>(allocator: &'a T) -> SharedAlloc<'a> {
        SharedAlloc { alloc: Alloc::of(allocator) }
    }

    /// The same handle as the kind every function takes.
    #[inline]
    pub fn local(self) -> Alloc<'a> {
        self.alloc
    }
}

impl<'a> From<SharedAlloc<'a>> for Alloc<'a> {
    #[inline]
    fn from(shared: SharedAlloc<'a>) -> Alloc<'a> {
        shared.alloc
    }
}

impl fmt::Debug for SharedAlloc<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("SharedAlloc").field(&self.alloc.header).finish()
    }
}

// SAFETY: every method is the inner handle's, whose contract it keeps.
unsafe impl Allocator for SharedAlloc<'_> {
    #[inline]
    fn allocate(&self, layout: Layout) -> Grant {
        self.alloc.allocate(layout)
    }

    #[inline]
    fn allocate_zeroed(&self, layout: Layout) -> Grant {
        self.alloc.allocate_zeroed(layout)
    }

    #[inline]
    unsafe fn deallocate(&self, ptr: NonNull<u8>, layout: Layout) {
        // SAFETY: the caller's contract, passed on.
        unsafe { self.alloc.deallocate(ptr, layout) }
    }

    #[inline]
    unsafe fn grow(&self, ptr: NonNull<u8>, old: Layout, new: Layout) -> Grant {
        // SAFETY: the caller's contract, passed on.
        unsafe { self.alloc.grow(ptr, old, new) }
    }

    #[inline]
    unsafe fn grow_zeroed(&self, ptr: NonNull<u8>, old: Layout, new: Layout) -> Grant {
        // SAFETY: the caller's contract, passed on.
        unsafe { self.alloc.grow_zeroed(ptr, old, new) }
    }

    #[inline]
    unsafe fn shrink(&self, ptr: NonNull<u8>, old: Layout, new: Layout) -> Grant {
        // SAFETY: the caller's contract, passed on.
        unsafe { self.alloc.shrink(ptr, old, new) }
    }
}

// SAFETY: as `Alloc`'s: a copy is the same allocator.
unsafe impl AllocatorClone for SharedAlloc<'_> {}

/// One-word handle to a [`BumpAlloc`] arena (`&Frame`, `&Inline`, `ArenaLocal`): the bump-arena
/// counterpart to [`SharedAlloc`].
///
/// Constructible only from a [`BumpAlloc`] (`Bump::of` or `Erased::new(frame).bump()`), never
/// from `Global` or `Heap`. Containers backed by `Bump<'a>` can freeze into arena-lifetime
/// slices/references via [`crate::Vec::into_bump_slice`], [`crate::String::into_bump_str`], and
/// [`crate::Box::into_bump_ref`] with compile-time proof that memory is reclaimed in bulk when the
/// arena drops.
#[derive(Clone, Copy)]
pub struct Bump<'a> {
    alloc: Alloc<'a>,
}

impl<'a> Bump<'a> {
    /// The handle of a [`BumpAlloc`] that carries its own header.
    #[inline]
    pub fn of<T: Headed + BumpAlloc>(allocator: &'a T) -> Bump<'a> {
        Bump { alloc: Alloc::of(allocator) }
    }

    /// Upcasts to a general [`Alloc<'a>`] for functions that do not require arena freezing.
    #[inline]
    pub fn alloc(self) -> Alloc<'a> {
        self.alloc
    }

    /// Moves `value` into this bump arena, returning a `&'a mut T` reclaimed when the arena drops.
    #[inline]
    pub fn alloc_val<T>(self, value: T) -> &'a mut T {
        crate::Box::into_bump_ref(crate::Box::new_in(value, self))
    }

    /// Copies `values` into this bump arena, returning a slice reclaimed when the arena drops.
    #[inline]
    pub fn alloc_slice_copy<T: Copy>(self, values: &[T]) -> &'a mut [T] {
        crate::Vec::from_slice_in(values, self).into_bump_slice()
    }

    /// Copies `text` into this bump arena, returning a `&'a mut str` reclaimed when the arena drops.
    #[inline]
    pub fn alloc_str(self, text: &str) -> &'a mut str {
        crate::String::from_str_in(text, self).into_bump_str()
    }
}

impl<'a> From<Bump<'a>> for Alloc<'a> {
    #[inline]
    fn from(bump: Bump<'a>) -> Alloc<'a> {
        bump.alloc
    }
}

impl fmt::Debug for Bump<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Bump").field(&self.alloc.header).finish()
    }
}

// SAFETY: every method forwards to the inner `Alloc`'s handle, whose contract it preserves.
unsafe impl Allocator for Bump<'_> {
    #[inline]
    fn allocate(&self, layout: Layout) -> Grant {
        self.alloc.allocate(layout)
    }

    #[inline]
    fn allocate_zeroed(&self, layout: Layout) -> Grant {
        self.alloc.allocate_zeroed(layout)
    }

    #[inline]
    unsafe fn deallocate(&self, ptr: NonNull<u8>, layout: Layout) {
        // SAFETY: the caller's contract, passed on.
        unsafe { self.alloc.deallocate(ptr, layout) }
    }

    #[inline]
    unsafe fn grow(&self, ptr: NonNull<u8>, old: Layout, new: Layout) -> Grant {
        // SAFETY: the caller's contract, passed on.
        unsafe { self.alloc.grow(ptr, old, new) }
    }

    #[inline]
    unsafe fn grow_zeroed(&self, ptr: NonNull<u8>, old: Layout, new: Layout) -> Grant {
        // SAFETY: the caller's contract, passed on.
        unsafe { self.alloc.grow_zeroed(ptr, old, new) }
    }

    #[inline]
    unsafe fn shrink(&self, ptr: NonNull<u8>, old: Layout, new: Layout) -> Grant {
        // SAFETY: the caller's contract, passed on.
        unsafe { self.alloc.shrink(ptr, old, new) }
    }
}

// SAFETY: as `Alloc`'s: a copy is the same allocator.
unsafe impl AllocatorClone for Bump<'_> {}

// SAFETY: `Bump` is only constructed from a `T: BumpAlloc` (`Bump::of`), so its backing arena
// reclaims all allocations in bulk on drop/reset.
unsafe impl BumpAlloc for Bump<'_> {}

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

    /// The handle that may cross threads, borrowing `self`: only of a `Sync` allocator.
    ///
    /// A collection over it crosses a thread:
    ///
    /// ```
    /// #![feature(allocator_ext)]
    /// let erased = blaze_own::Erased::new(std::alloc::Global);
    /// let mut v = blaze_own::Vec::new_in(erased.shared());
    /// v.push(1u32);
    /// std::thread::scope(|s| {
    ///     s.spawn(move || assert_eq!(v.as_slice(), [1]));
    /// });
    /// ```
    ///
    /// One over a thread's own allocator (an `Inline` buffer, a task's frame) cannot be shared:
    ///
    /// ```compile_fail,E0277
    /// let inline = blaze_own::Inline::<256>::new();
    /// let erased = blaze_own::Erased::new(&inline);
    /// let _ = erased.shared();
    /// ```
    ///
    /// and over its plain handle it cannot cross a thread:
    ///
    /// ```compile_fail,E0277
    /// let inline = blaze_own::Inline::<256>::new();
    /// let erased = blaze_own::Erased::new(&inline);
    /// let mut v = blaze_own::Vec::new_in(erased.handle());
    /// v.push(1u32);
    /// std::thread::scope(|s| {
    ///     s.spawn(move || drop(v));
    /// });
    /// ```
    #[inline]
    pub fn shared(&self) -> SharedAlloc<'_>
    where
        A: Sync,
    {
        SharedAlloc::of(self)
    }

    /// A [`Bump`] handle when `A` is a bulk-reclaimed [`BumpAlloc`] arena (`&Frame`, `&Inline`,
    /// `ArenaLocal`); does not compile for `Global` or `Heap`:
    ///
    /// ```compile_fail,E0277
    /// #![feature(allocator_api)]
    /// let erased = blaze_own::Erased::new(std::alloc::Global);
    /// let _ = erased.bump();
    /// ```
    #[inline]
    pub fn bump(&self) -> Bump<'_>
    where
        A: BumpAlloc,
    {
        Bump::of(self)
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

// SAFETY: `Erased<A>` delegates every allocation directly to `A`; when `A: BumpAlloc`, bulk
// reclamation holds identically.
unsafe impl<A: BumpAlloc> BumpAlloc for Erased<A> {}

/// A [`Vec`] over a [`Bump`] arena handle.
pub type BVec<'a, T> = Vec<T, Bump<'a>>;
/// A [`String`] over a [`Bump`] arena handle.
pub type BString<'a> = String<Bump<'a>>;
/// A [`Box`] over a [`Bump`] arena handle.
pub type BBox<'a, T> = Box<T, Bump<'a>>;

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
/// A [`Lists`] over the handle: rows of `T` in one item array and one array of row ends.
pub type ALists<'a, T> = Lists<T, Alloc<'a>>;
/// A [`Texts`] over the handle: short strings in one byte buffer.
pub type ATexts<'a> = Texts<Alloc<'a>>;
/// A [`Paths`] over the handle: directories and files as rows, each name once a directory.
pub type APaths<'a> = Paths<Alloc<'a>>;
/// A [`ThinArc`] over the handle (still one pointer, 8 bytes: the handle lives in the heap header).
pub type AThinArc<'a, H, T> = ThinArc<H, T, Alloc<'a>>;
/// A [`ThinArcSlice`] over the handle (one pointer, 8 bytes).
pub type AThinArcSlice<'a, T> = ThinArcSlice<T, Alloc<'a>>;
/// A [`ThinStr`] over the handle (one pointer, 8 bytes).
pub type AThinStr<'a> = ThinStr<Alloc<'a>>;
/// A [`ThinVec`] over the handle.
pub type AThinVec<'a, T> = ThinVec<T, Alloc<'a>>;
