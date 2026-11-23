//! [`Box`]: one value, sized or not, in an allocator the caller names.
//!
//! The compiler's own box moves its value out with `*b`; this one moves it out with
//! [`Box::into_inner`] (docs/v2/plan/design/bottom.md, "Known cost"). It unsizes as std's does
//! (`Box<T, A>` to `Box<dyn Trait, A>`, `Box<[T; N], A>` to `Box<[T], A>`, and pinned,
//! `Pin<Box<T, A>>` to `Pin<Box<dyn Trait, A>>`, when `A` is a `StaticAllocator`), and a `Box<dyn Fn()>`
//! is called as `b()` through `Deref`.
//!
//! A boxed closure called once: std's `Box<dyn FnOnce()>` moves the unsized closure out by the
//! compiler's `*b`, which no library box has. The primitive here is [`CallOnce`], a trait every
//! sized `FnOnce` has, whose call takes the closure by `&mut` and leaves its place moved-from;
//! a `Box<F, A>` is `FnOnce` (and `FnMut`, `Fn` when `F` is) for every `F: CallOnce`, so
//! `Box<dyn CallOnce<(u32,), Output = R>, A>` is the boxed one-shot closure, called as `b(7)`.
//! Not yet: `self: Box<Self, A>` receivers (`dispatch_from_dyn` admits only an allocator sized
//! 0).

use core::{
    alloc::{Allocator, Layout, StaticAllocator},
    any::type_name,
    borrow::{Borrow, BorrowMut},
    cmp::Ordering,
    fmt,
    hash::{Hash, Hasher},
    marker::{PhantomData, Tuple, Unsize},
    mem::{DropGuard, ManuallyDrop, size_of},
    ops::{CoerceUnsized, Deref, DerefMut},
    pin::{Pin, PinSafePointer},
    ptr::{self, NonNull},
};

use crate::{
    error::{AllocFailure, fail},
    make::CloneIn,
    vec::{Refused, Vec},
};

/// One `T` in `A`: one block (none for a zero-sized `T`), one pointer word (two for a slice or `dyn`).
///
/// # Invariants
///
/// `ptr` points to a live `T`. When `Layout::for_value` of it has a size, the block is one
/// `alloc` returned for that layout; when the size is 0 nothing is allocated and `ptr` is
/// dangling and aligned.
pub struct Box<T: ?Sized, A: Allocator> {
    ptr: NonNull<T>,
    alloc: A,
    /// Owns a `T`, for drop check (the `#[may_dangle]` on the drop relies on it).
    _owns: PhantomData<T>,
}

// SAFETY: the box owns its value uniquely, as std's does.
unsafe impl<T: ?Sized + Send, A: Allocator + Send> Send for Box<T, A> {}
// SAFETY: a `&Box` gives only `&T` and `&A`.
unsafe impl<T: ?Sized + Sync, A: Allocator + Sync> Sync for Box<T, A> {}

// The value never moves when the box does.
impl<T: ?Sized, A: Allocator> Unpin for Box<T, A> {}

impl<T: ?Sized + Unsize<U>, U: ?Sized, A: Allocator> CoerceUnsized<Box<U, A>> for Box<T, A> {}

// SAFETY: as std's `Box` (library/alloc/src/boxed.rs, the same bound). `deref` and `deref_mut`
// always give the one value the box owns, at an address that does not change when the box
// moves or is unsized, and neither they nor the drop move the value out. `&Box` is never taken
// as evidence the value is not pinned. `Clone` makes a new value in a clone of the allocator,
// which `Pin::clone` pins unchecked: sound because a `StaticAllocator` never invalidates that
// block before the new box frees it, after the value's drop. `Debug` and `Display` read `&T`.
unsafe impl<T: ?Sized, A: StaticAllocator> PinSafePointer for Box<T, A> {}

impl<T, A: Allocator> Box<T, A> {
    /// `value` in `alloc`; panics with the [`AllocFailure`] when there is no memory.
    #[inline]
    pub fn new_in(value: T, alloc: A) -> Box<T, A> {
        match Box::try_new_in(value, alloc) {
            Ok(b) => b,
            Err(refused) => fail(refused.why),
        }
    }

    /// `value` in `alloc`, or `value` back with the [`AllocFailure`].
    pub fn try_new_in(value: T, alloc: A) -> Result<Box<T, A>, Refused<T>> {
        let layout = Layout::new::<T>();
        let ptr = if size_of::<T>() == 0 {
            NonNull::dangling()
        } else {
            match alloc.allocate(layout) {
                Ok(block) => block.cast::<T>(),
                Err(_) => {
                    let why = AllocFailure::Refused { what: type_name::<T>(), layout };
                    return Err(Refused { value, why });
                }
            }
        };
        // SAFETY: `ptr` is a fresh block of `T`'s layout (or dangling and aligned for a `T` of
        // size 0), with no value in it yet.
        unsafe { ptr::write(ptr.as_ptr(), value) };
        Ok(Box { ptr, alloc, _owns: PhantomData })
    }

    /// `value` in `alloc`, pinned: it is never moved or its memory reused before it is dropped,
    /// which a `StaticAllocator` promises for its blocks.
    pub fn pin_in(value: T, alloc: A) -> Pin<Box<T, A>>
    where
        A: StaticAllocator,
    {
        Box::into_pin(Box::new_in(value, alloc))
    }

    /// The value, moved out; the block is freed. The replacement for `*b`.
    pub fn into_inner(b: Box<T, A>) -> T {
        let (ptr, alloc) = Box::into_non_null_with_allocator(b);
        // SAFETY: the box's live value, read out once; the box is gone.
        let value = unsafe { ptr::read(ptr.as_ptr()) };
        let layout = Layout::new::<T>();
        if layout.size() != 0 {
            // SAFETY: the block is one `alloc` returned for `T`'s layout (the invariant), freed
            // once, after the value moved out of it.
            unsafe { alloc.deallocate(ptr.cast(), layout) };
        }
        value
    }
}

impl<T: ?Sized, A: Allocator> Box<T, A> {
    /// Takes over a value from a pointer and its allocator.
    ///
    /// # Safety
    ///
    /// `ptr` points to a live `T` in a block `alloc` returned for `Layout::for_value` of it, or
    /// that layout has size 0 and `ptr` is dangling and aligned; nothing else owns the value.
    #[inline]
    pub unsafe fn from_non_null_in(ptr: NonNull<T>, alloc: A) -> Box<T, A> {
        Box { ptr, alloc, _owns: PhantomData }
    }

    /// As [`Box::from_non_null_in`].
    ///
    /// # Safety
    ///
    /// As [`Box::from_non_null_in`], and `raw` is not null.
    #[inline]
    pub unsafe fn from_raw_in(raw: *mut T, alloc: A) -> Box<T, A> {
        // SAFETY: the caller promises `raw` is not null.
        let ptr = unsafe { NonNull::new_unchecked(raw) };
        Box { ptr, alloc, _owns: PhantomData }
    }

    /// The pointer and the allocator; nothing is dropped or freed.
    pub fn into_non_null_with_allocator(b: Box<T, A>) -> (NonNull<T>, A) {
        let b = ManuallyDrop::new(b);
        // SAFETY: `b` is never used or dropped again, so the allocator moves out once.
        let alloc = unsafe { ptr::read(&b.alloc) };
        (b.ptr, alloc)
    }

    pub fn into_raw_with_allocator(b: Box<T, A>) -> (*mut T, A) {
        let (ptr, alloc) = Box::into_non_null_with_allocator(b);
        (ptr.as_ptr(), alloc)
    }

    /// Pins it in place ([`Box::pin_in`]).
    pub fn into_pin(b: Box<T, A>) -> Pin<Box<T, A>>
    where
        A: StaticAllocator,
    {
        // SAFETY: the value never moves when the box does, and a `StaticAllocator` never reuses
        // the block before the box frees it, which happens only after the value is dropped.
        unsafe { Pin::new_unchecked(b) }
    }

    /// The allocator its block is in. An associated function, so it never hides a method of
    /// `T`.
    #[inline]
    pub fn allocator(b: &Box<T, A>) -> &A {
        &b.alloc
    }

    #[inline]
    pub fn as_ptr(b: &Box<T, A>) -> *const T {
        b.ptr.as_ptr()
    }

    /// Gives up the value for as long as the allocator lives; nothing is dropped or freed.
    /// Freezes a bump-allocated box into a mutable reference tied to the arena's lifetime `'a`.
    ///
    /// Only compiles for bulk-reclaimed [`BumpAlloc`](crate::BumpAlloc) arenas (`&Frame`,
    /// `ArenaLocal`, `&Inline`, `Bump<'a>`) — never `Global` or `Heap`.
    #[inline]
    pub fn into_bump_ref<'a>(b: Box<T, A>) -> &'a mut T
    where
        A: crate::BumpAlloc + 'a,
    {
        let (mut ptr, alloc) = Box::into_non_null_with_allocator(b);
        core::mem::forget(alloc);
        // SAFETY: `A: BumpAlloc + 'a` guarantees the value lives for `'a` and its backing block is
        // reclaimed in bulk when the arena drops; nothing else aliases `ptr`.
        unsafe { ptr.as_mut() }
    }

    /// Permanently leaks a `'static`-backed box for the remainder of the process.
    ///
    /// Only valid for intentional process-lifetime OS hooks (such as an installed unwind
    /// registry). For scoped arenas, use [`Box::into_bump_ref`] instead.
    pub fn leak_static(b: Box<T, A>) -> &'static mut T
    where
        A: 'static,
    {
        let (mut ptr, alloc) = Box::into_non_null_with_allocator(b);
        core::mem::forget(alloc);
        // SAFETY: the box and its allocator are forgotten, so the value lives for `'static`.
        unsafe { ptr.as_mut() }
    }
}

impl<T, A: Allocator> Box<[T], A> {
    /// Clones of `values` in `alloc`.
    pub fn from_slice_in(values: &[T], alloc: A) -> Box<[T], A>
    where
        T: Clone,
    {
        Vec::from_slice_in(values, alloc).into_boxed_slice()
    }

    /// The values as a vector over the same block, its capacity their number.
    pub fn into_vec(b: Box<[T], A>) -> Vec<T, A> {
        let (ptr, alloc) = Box::into_non_null_with_allocator(b);
        let len = ptr.len();
        // SAFETY: the block holds `len` values of `T` in the layout of `[T]` of that length,
        // which is `RawBuf`'s for capacity `len`; for a length of 0 or a `T` sized 0 nothing is
        // allocated.
        unsafe { Vec::from_raw_parts_in(ptr.cast(), len, len, alloc) }
    }
}

// SAFETY (`may_dangle`): the drop drops the `T` it owns (`PhantomData<T>` tells drop check so) and
// reads nothing else of it.
unsafe impl<#[may_dangle] T: ?Sized, A: Allocator> Drop for Box<T, A> {
    fn drop(&mut self) {
        // SAFETY: `ptr` points to a live `T` (the invariant), whose size and alignment its
        // pointer's metadata gives.
        let layout = unsafe { Layout::for_value_raw(self.ptr.as_ptr()) };
        let block = self.ptr.cast::<u8>();
        // The block is freed after the value's drop, a panic in it among the ways.
        let _free = DropGuard::new(&self.alloc, move |alloc: &A| {
            if layout.size() != 0 {
                // SAFETY: the block is one `alloc` returned for `layout` (the invariant), freed
                // once, after the value in it was dropped.
                unsafe { alloc.deallocate(block, layout) };
            }
        });
        // SAFETY: the value is live and dropped once; the box is going away.
        unsafe { ptr::drop_in_place(self.ptr.as_ptr()) };
    }
}

impl<T: ?Sized, A: Allocator> Deref for Box<T, A> {
    type Target = T;

    #[inline]
    fn deref(&self) -> &T {
        // SAFETY: `ptr` points to a live `T` the box owns (the invariant).
        unsafe { self.ptr.as_ref() }
    }
}

impl<T: ?Sized, A: Allocator> DerefMut for Box<T, A> {
    #[inline]
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: as `deref`, with `&mut self` the only access.
        unsafe { self.ptr.as_mut() }
    }
}

impl<T: Clone, A: Allocator + Clone> Clone for Box<T, A> {
    /// Allocates from the collection's own allocator: prefer `clone_in`.
    fn clone(&self) -> Box<T, A> {
        Box::new_in((**self).clone(), self.alloc.clone())
    }
}

impl<T: Clone, A: Allocator + Clone> Clone for Box<[T], A> {
    /// Allocates from the collection's own allocator: prefer `clone_in`.
    fn clone(&self) -> Box<[T], A> {
        Box::from_slice_in(self, self.alloc.clone())
    }
}

// The copies into another allocator are associated functions (`Box::clone_in(&b, alloc)`), as
// `Box::allocator` is: a method would hide the boxed value's own `clone_in`. One impl over
// `T: ?Sized` (two, for `T` and `[T]`, would make `Box::clone_in` ambiguous), the work in
// `CloneIn`'s impls for `Box<T, A>` and `Box<[T], A>` (crate::make).
impl<T: ?Sized, A: Allocator> Box<T, A> {
    /// A deep copy in another allocator: the value copied with [`CloneIn`] into `alloc`, and a
    /// block for it there (for a slice, of exactly its length). A refusal drops what was copied.
    pub fn try_clone_in<B: Allocator>(b: &Box<T, A>, alloc: B) -> Result<<Box<T, A> as CloneIn<B>>::In, AllocFailure>
    where
        Box<T, A>: CloneIn<B>,
    {
        CloneIn::try_clone_in(b, alloc)
    }

    /// [`Box::try_clone_in`], panicking with the [`AllocFailure`].
    #[track_caller]
    pub fn clone_in<B: Allocator>(b: &Box<T, A>, alloc: B) -> <Box<T, A> as CloneIn<B>>::In
    where
        Box<T, A>: CloneIn<B>,
    {
        match CloneIn::try_clone_in(b, alloc) {
            Ok(copy) => copy,
            Err(failure) => fail(failure),
        }
    }
}

impl<T: ?Sized + fmt::Debug, A: Allocator> fmt::Debug for Box<T, A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&**self, f)
    }
}

impl<T: ?Sized + fmt::Display, A: Allocator> fmt::Display for Box<T, A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&**self, f)
    }
}

impl<T: ?Sized + PartialEq, A: Allocator> PartialEq for Box<T, A> {
    #[inline]
    fn eq(&self, other: &Box<T, A>) -> bool {
        PartialEq::eq(&**self, &**other)
    }
}

impl<T: ?Sized + Eq, A: Allocator> Eq for Box<T, A> {}

impl<T: ?Sized + PartialOrd, A: Allocator> PartialOrd for Box<T, A> {
    #[inline]
    fn partial_cmp(&self, other: &Box<T, A>) -> Option<Ordering> {
        PartialOrd::partial_cmp(&**self, &**other)
    }
}

impl<T: ?Sized + Ord, A: Allocator> Ord for Box<T, A> {
    #[inline]
    fn cmp(&self, other: &Box<T, A>) -> Ordering {
        Ord::cmp(&**self, &**other)
    }
}

impl<T: ?Sized + Hash, A: Allocator> Hash for Box<T, A> {
    #[inline]
    fn hash<H: Hasher>(&self, state: &mut H) {
        (**self).hash(state);
    }
}

impl<T: ?Sized, A: Allocator> AsRef<T> for Box<T, A> {
    fn as_ref(&self) -> &T {
        self
    }
}

impl<T: ?Sized, A: Allocator> AsMut<T> for Box<T, A> {
    fn as_mut(&mut self) -> &mut T {
        self
    }
}

impl<T: ?Sized, A: Allocator> Borrow<T> for Box<T, A> {
    fn borrow(&self) -> &T {
        self
    }
}

impl<T: ?Sized, A: Allocator> BorrowMut<T> for Box<T, A> {
    fn borrow_mut(&mut self) -> &mut T {
        self
    }
}

/// A closure that can be called once in place: the primitive under a boxed one-shot closure
/// (see the module). Every sized `FnOnce` has it; `dyn CallOnce<Args, Output = R>` is the
/// unsized form a [`Box`] holds.
pub trait CallOnce<Args: Tuple>: FnOnce<Args> {
    /// Calls the closure, moving it out of its place.
    ///
    /// # Safety
    ///
    /// `*self` is moved-from after: never used or dropped again (its memory may be freed).
    unsafe fn call_once_in_place(&mut self, args: Args) -> Self::Output;
}

impl<Args: Tuple, F: FnOnce<Args>> CallOnce<Args> for F {
    unsafe fn call_once_in_place(&mut self, args: Args) -> F::Output {
        // SAFETY: the place is moved-from after (the caller's promise), so this is its one move.
        let closure = unsafe { ptr::read(self) };
        closure.call_once(args)
    }
}

impl<Args: Tuple, F: ?Sized + CallOnce<Args>, A: Allocator> FnOnce<Args> for Box<F, A> {
    type Output = F::Output;

    extern "rust-call" fn call_once(self, args: Args) -> F::Output {
        let (ptr, alloc) = Box::into_non_null_with_allocator(self);
        // SAFETY: the box's value is live, so its metadata is valid.
        let layout = unsafe { Layout::for_value_raw(ptr.as_ptr()) };
        // The block is freed after the call, a panic in it among the ways, without dropping the
        // moved-from closure.
        let _free = DropGuard::new(alloc, move |alloc| {
            if layout.size() != 0 {
                // SAFETY: the box's block, from `alloc` for `layout` (the box's invariant),
                // freed once.
                unsafe { alloc.deallocate(ptr.cast(), layout) };
            }
        });
        // SAFETY: the closure is live and owned here; it is moved-from after, and only its
        // block is freed.
        unsafe { (*ptr.as_ptr()).call_once_in_place(args) }
    }
}

impl<Args: Tuple, F: ?Sized + FnMut<Args> + CallOnce<Args>, A: Allocator> FnMut<Args> for Box<F, A> {
    extern "rust-call" fn call_mut(&mut self, args: Args) -> <F as FnOnce<Args>>::Output {
        (**self).call_mut(args)
    }
}

impl<Args: Tuple, F: ?Sized + Fn<Args> + CallOnce<Args>, A: Allocator> Fn<Args> for Box<F, A> {
    extern "rust-call" fn call(&self, args: Args) -> <F as FnOnce<Args>>::Output {
        (**self).call(args)
    }
}

impl<I: Iterator + ?Sized, A: Allocator> Iterator for Box<I, A> {
    type Item = I::Item;

    fn next(&mut self) -> Option<I::Item> {
        (**self).next()
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (**self).size_hint()
    }
}

impl<T: ?Sized + core::error::Error, A: Allocator> core::error::Error for Box<T, A> {}
