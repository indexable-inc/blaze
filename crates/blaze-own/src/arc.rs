//! [`Arc`] and [`Weak`]: a value shared by owners on any threads, counted atomically, in an
//! allocator the caller names. The counting protocol is std's (`alloc::sync`), each ordering
//! with its reason beside it.
//!
//! The owners share one allocator: each holds a clone, and whichever is last frees the block
//! with its own, so cloning an `Arc` asks `A: AllocatorClone` (clones are equivalent: memory
//! from one may be freed by another), as std's does.
//!
//! Not here yet: `make_mut` and `from` for unsized values (they need `CloneToUninit`), and
//! `Weak::new` without a block.

use core::{
    alloc::{Allocator, AllocatorClone, Layout, StaticAllocator},
    any::type_name,
    borrow::Borrow,
    cmp::Ordering,
    fmt,
    hash::{Hash, Hasher},
    hint,
    marker::{PhantomData, Unsize},
    mem::{DropGuard, ManuallyDrop},
    ops::{CoerceUnsized, Deref},
    pin::Pin,
    ptr::{self, NonNull},
    sync::atomic::{
        self, AtomicUsize,
        Ordering::{Acquire, Relaxed, Release},
    },
};

#[cfg(test)]
use crate::plant::{self, Plant};
use crate::{
    error::{AllocFailure, MOST_COUNT, abort_count_overflow, fail},
    vec::Refused,
};

/// The block: the two counts, then the value. `repr(C)` so a slice value's block can be laid
/// out by hand ([`Arc::from_slice_in`]).
///
/// `strong` counts the `Arc`s. `weak` counts the `Weak`s plus one that every `Arc` holds
/// together, given up when the last `Arc` goes; the block is freed when `weak` reaches 0.
/// `weak` is `usize::MAX` while [`Arc::get_mut`] checks it is alone (`is_unique`).
#[repr(C)]
struct Inner<T: ?Sized> {
    strong: AtomicUsize,
    weak: AtomicUsize,
    value: T,
}

/// The layout of an `Inner<[T]>` of `len` values, as the compiler lays out a `repr(C)` struct
/// with a slice last.
fn slice_layout<T>(len: usize) -> Option<Layout> {
    let values = Layout::array::<T>(len).ok()?;
    let (layout, _offset) = Layout::new::<Inner<()>>().extend(values).ok()?;
    Some(layout.pad_to_align())
}

/// A shared `T` in `A`, counted atomically: one block for value and counts; every clone and drop writes the one shared count line.
///
/// # Invariants
///
/// `ptr` points to a block `alloc` (or an equivalent clone) returned for
/// `Layout::for_value(&*ptr)`, whose `strong` count includes this owner, so `value` is live.
pub struct Arc<T: ?Sized, A: Allocator> {
    ptr: NonNull<Inner<T>>,
    alloc: A,
    /// Owns an `Inner<T>`, for drop check (the `#[may_dangle]` on the drop relies on it).
    _owns: PhantomData<Inner<T>>,
}

/// A reference that keeps an [`Arc`]'s block but not its value: [`Weak::upgrade`] gives an
/// `Arc` while one is left.
///
/// # Invariants
///
/// `ptr` points to a block as `Arc`'s whose `weak` count includes this reference, so the
/// block, though maybe not the value, is live.
pub struct Weak<T: ?Sized, A: Allocator> {
    ptr: NonNull<Inner<T>>,
    alloc: A,
}

// SAFETY: as std's: an `Arc` on another thread reads the value (`T: Sync`) and may be the last
// owner, which drops it there (`T: Send`); the allocator is shared and may free there.
unsafe impl<T: ?Sized + Sync + Send, A: Allocator + Send + Sync> Send for Arc<T, A> {}
// SAFETY: as `Send`: a `&Arc` clones into an `Arc` on the other thread.
unsafe impl<T: ?Sized + Sync + Send, A: Allocator + Send + Sync> Sync for Arc<T, A> {}
// SAFETY: as `Arc`'s: a `Weak` upgrades into an `Arc` wherever it is.
unsafe impl<T: ?Sized + Sync + Send, A: Allocator + Send + Sync> Send for Weak<T, A> {}
// SAFETY: as `Send`.
unsafe impl<T: ?Sized + Sync + Send, A: Allocator + Send + Sync> Sync for Weak<T, A> {}

impl<T: ?Sized, A: Allocator> Unpin for Arc<T, A> {}

impl<T: ?Sized + Unsize<U>, U: ?Sized, A: Allocator> CoerceUnsized<Arc<U, A>> for Arc<T, A> {}
impl<T: ?Sized + Unsize<U>, U: ?Sized, A: Allocator> CoerceUnsized<Weak<U, A>> for Weak<T, A> {}

/// The counts of the block at `ptr`, read through raw places, so no reference to the value is
/// made (the value may already be dropped when a `Weak` reads them).
///
/// # Safety
///
/// The block at `ptr` is live.
unsafe fn counts<'a, T: ?Sized>(ptr: NonNull<Inner<T>>) -> (&'a AtomicUsize, &'a AtomicUsize) {
    let raw = ptr.as_ptr();
    // SAFETY: the block is live (the caller's promise), so its counts are; they are atomics,
    // shared by every owner.
    unsafe { (&*(&raw const (*raw).strong), &*(&raw const (*raw).weak)) }
}

impl<T, A: Allocator> Arc<T, A> {
    /// `value` shared, in `alloc`; panics with the [`AllocFailure`] when there is no memory.
    pub fn new_in(value: T, alloc: A) -> Arc<T, A> {
        match Arc::try_new_in(value, alloc) {
            Ok(arc) => arc,
            Err(refused) => fail(refused.why),
        }
    }

    /// `value` shared, in `alloc`, or `value` back with the [`AllocFailure`].
    pub fn try_new_in(value: T, alloc: A) -> Result<Arc<T, A>, Refused<T>> {
        let layout = Layout::new::<Inner<T>>();
        let ptr = match alloc.allocate(layout) {
            Ok(block) => block.cast::<Inner<T>>(),
            Err(_) => {
                let why = AllocFailure::Refused { what: type_name::<T>(), layout };
                return Err(Refused { value, why });
            }
        };
        let inner = Inner { strong: AtomicUsize::new(1), weak: AtomicUsize::new(1), value };
        // SAFETY: a fresh block of `Inner<T>`'s layout, empty.
        unsafe { ptr.as_ptr().write(inner) };
        Ok(Arc { ptr, alloc, _owns: PhantomData })
    }

    /// A value made by `make` from a [`Weak`] to its own block, for a value that refers to
    /// itself. The `Weak` (and its clones) upgrade to nothing until `make` returns. A panic in
    /// `make` frees the block, which holds no value then.
    pub fn new_cyclic_in(make: impl FnOnce(&Weak<T, A>) -> T, alloc: A) -> Arc<T, A> {
        let layout = Layout::new::<Inner<T>>();
        let Ok(block) = alloc.allocate(layout) else {
            fail(AllocFailure::Refused { what: type_name::<T>(), layout });
        };
        let ptr = block.cast::<Inner<T>>();
        // SAFETY: a fresh block of `Inner<T>`'s layout: its counts are written, its value left
        // for `make`; strong 0 says there is no value to reach.
        unsafe {
            (&raw mut (*ptr.as_ptr()).strong).write(AtomicUsize::new(0));
            (&raw mut (*ptr.as_ptr()).weak).write(AtomicUsize::new(1));
        }
        // The owners' shared weak reference, lent to `make`; dropped by a panic in it, which
        // frees the block without touching the value.
        let weak = Weak { ptr, alloc };
        let value = make(&weak);
        // SAFETY: the value's place is empty and no owner reaches it while strong is 0.
        unsafe { (&raw mut (*ptr.as_ptr()).value).write(value) };
        // Release: the value's write happens before any `upgrade` that sees the count (its
        // exchange acquires).
        weak.strong().store(1, Release);
        let weak = ManuallyDrop::new(weak);
        // SAFETY: `weak` is never dropped: its weak reference becomes the owners' shared one,
        // and its allocator moves out once.
        let alloc = unsafe { ptr::read(&weak.alloc) };
        Arc { ptr, alloc, _owns: PhantomData }
    }

    /// `value` shared and pinned: the block never moves and a `StaticAllocator` never reuses it
    /// before it is freed, which is after the value is dropped.
    pub fn pin_in(value: T, alloc: A) -> Pin<Arc<T, A>>
    where
        A: StaticAllocator,
    {
        // SAFETY: an `Arc` gives no `&mut T` through `Pin` (`get_mut` takes `&mut Arc`, which a
        // `Pin<Arc>` never lends), and the value stays in its block until dropped.
        unsafe { Pin::new_unchecked(Arc::new_in(value, alloc)) }
    }

    /// The value, when this is the only `Arc`; else this `Arc` back.
    pub fn try_unwrap(this: Arc<T, A>) -> Result<T, Arc<T, A>> {
        // Relaxed: the fence below orders this after the other owners' release decrements.
        if this.strong().compare_exchange(1, 0, Relaxed, Relaxed).is_err() {
            return Err(this);
        }
        // Acquire: synchronizes with every other owner's release decrement, so their uses of the
        // value happen before it moves out.
        atomic::fence(Acquire);
        let this = ManuallyDrop::new(this);
        // SAFETY: the strong count went from 1 to 0, so the value is ours alone and moves out
        // once; `this` is never dropped, so its allocator moves out once.
        let (value, alloc) = unsafe { (ptr::read(&(*this.ptr.as_ptr()).value), ptr::read(&this.alloc)) };
        // The weak reference the owners held together: frees the block when no `Weak` is left.
        drop(Weak { ptr: this.ptr, alloc });
        Ok(value)
    }

    /// The value, when this is the last `Arc`; else `None`, and this owner is gone. Of several
    /// owners each calling it, exactly one gets the value.
    pub fn into_inner(this: Arc<T, A>) -> Option<T> {
        let this = ManuallyDrop::new(this);
        // SAFETY: `this` is never dropped, so its allocator moves out once.
        let alloc = unsafe { ptr::read(&this.alloc) };
        // Release and Acquire: as in the drop.
        if this.strong().fetch_sub(1, Release) != 1 {
            return None;
        }
        atomic::fence(Acquire);
        // SAFETY: the strong count reached 0 here: the value is ours alone and moves out once.
        let value = unsafe { ptr::read(&(*this.ptr.as_ptr()).value) };
        drop(Weak { ptr: this.ptr, alloc });
        Some(value)
    }

    /// The value to change, cloned into a block of its own first when other `Arc`s share it,
    /// or moved to one when only `Weak`s do (they then upgrade to nothing).
    ///
    /// When the move's allocation fails it panics with the [`AllocFailure`] and the value
    /// stays; a `Weak` upgrading in that moment sees `None`.
    pub fn make_mut(this: &mut Arc<T, A>) -> &mut T
    where
        T: Clone,
        A: AllocatorClone,
    {
        // Acquire: synchronizes with the release decrements of owners that went away, so their
        // uses of the value happen before this owner writes to it.
        if this.strong().compare_exchange(1, 0, Acquire, Relaxed).is_err() {
            // Other owners: take a copy of our own.
            let copy = Arc::new_in((**this).clone(), this.alloc.clone());
            *this = copy;
        } else if this.weak().load(Relaxed) != 1 {
            // The only owner, with `Weak`s: the value moves to a new block, and the old one is
            // left to the `Weak`s with a strong count of 0, so none upgrades.
            let alloc = this.alloc.clone();
            let layout = Layout::new::<Inner<T>>();
            let block = match alloc.allocate(layout) {
                Ok(block) => block.cast::<Inner<T>>(),
                Err(_) => {
                    // Release: as the store below; the value never left.
                    this.strong().store(1, Release);
                    fail(AllocFailure::Refused { what: type_name::<T>(), layout });
                }
            };
            // SAFETY: the strong count is 0 and this is its only owner, so the value moves out
            // of the old block once; the new block is fresh, of `Inner<T>`'s layout.
            unsafe {
                let value = ptr::read(&(*this.ptr.as_ptr()).value);
                let inner = Inner { strong: AtomicUsize::new(1), weak: AtomicUsize::new(1), value };
                block.as_ptr().write(inner);
            }
            // The owners' shared weak reference to the old block, given up after the switch.
            let old = Weak { ptr: this.ptr, alloc };
            this.ptr = block;
            drop(old);
        } else {
            // The only owner of either kind. Release: synchronizes with a later `upgrade`'s
            // acquire, though no `Weak` exists now to make one.
            this.strong().store(1, Release);
        }
        // SAFETY: this owner is now the only one of either kind, so no other access exists.
        unsafe { &mut (*this.ptr.as_ptr()).value }
    }
}

impl<T, A: Allocator> Arc<[T], A> {
    /// Clones of `values` shared, in one block in `alloc`.
    pub fn from_slice_in(values: &[T], alloc: A) -> Arc<[T], A>
    where
        T: Clone,
    {
        match Arc::try_from_slice_in(values, alloc) {
            Ok(arc) => arc,
            Err(failure) => fail(failure),
        }
    }

    pub fn try_from_slice_in(values: &[T], alloc: A) -> Result<Arc<[T], A>, AllocFailure>
    where
        T: Clone,
    {
        let len = values.len();
        let Some(layout) = slice_layout::<T>(len) else {
            let what = type_name::<T>();
            return Err(AllocFailure::CapacityOverflow { what, len: 0, additional: len });
        };
        let block = alloc
            .allocate(layout)
            .map_err(|_| AllocFailure::Refused { what: type_name::<[T]>(), layout })?
            .cast::<u8>();
        // A slice's metadata is its length, and a struct with a slice last takes the slice's.
        let inner = ptr::slice_from_raw_parts_mut(block.as_ptr().cast::<T>(), len) as *mut Inner<[T]>;
        // SAFETY: `inner` is the fresh block, laid out as `Inner<[T]>` of `len` (`slice_layout`);
        // its counts are written before anything reads them.
        let first = unsafe {
            (&raw mut (*inner).strong).write(AtomicUsize::new(1));
            (&raw mut (*inner).weak).write(AtomicUsize::new(1));
            (&raw mut (*inner).value).cast::<T>()
        };
        // A panicking clone drops the clones made and frees the block.
        let mut made = DropGuard::new(0usize, |made| {
            // SAFETY: the first `made` slots hold clones, dropped once; the block is `alloc`'s,
            // freed once, and no `Arc` was made of it.
            unsafe {
                ptr::drop_in_place(ptr::slice_from_raw_parts_mut(first, made));
                alloc.deallocate(block, layout);
            }
        });
        for value in values {
            let clone = value.clone();
            // SAFETY: slot `made < len` is in the block and empty.
            unsafe { first.add(*made).write(clone) };
            *made += 1;
        }
        DropGuard::dismiss(made);
        // SAFETY: `inner` came from a non-null block.
        let ptr = unsafe { NonNull::new_unchecked(inner) };
        Ok(Arc { ptr, alloc, _owns: PhantomData })
    }
}

impl<A: Allocator> Arc<str, A> {
    /// A copy of `text` shared, in `alloc`.
    pub fn from_str_in(text: &str, alloc: A) -> Arc<str, A> {
        let bytes = Arc::from_slice_in(text.as_bytes(), alloc);
        let bytes = ManuallyDrop::new(bytes);
        // SAFETY: `bytes` is never dropped, so the allocator moves out once.
        let alloc = unsafe { ptr::read(&bytes.alloc) };
        // `str` has `[u8]`'s layout and metadata; the bytes are UTF-8, copied from a `str`.
        let raw = bytes.ptr.as_ptr() as *mut Inner<str>;
        // SAFETY: from a non-null pointer.
        let ptr = unsafe { NonNull::new_unchecked(raw) };
        Arc { ptr, alloc, _owns: PhantomData }
    }
}

impl<T: ?Sized, A: Allocator> Arc<T, A> {
    #[inline]
    fn strong(&self) -> &AtomicUsize {
        // SAFETY: an `Arc` keeps its block live (the invariant).
        unsafe { counts(self.ptr) }.0
    }

    #[inline]
    fn weak(&self) -> &AtomicUsize {
        // SAFETY: as `strong`.
        unsafe { counts(self.ptr) }.1
    }

    /// The value to change, when no other `Arc` or `Weak` shares it.
    pub fn get_mut(this: &mut Arc<T, A>) -> Option<&mut T> {
        if Arc::is_unique(this) {
            // SAFETY: no other owner of either kind exists, and `&mut this` keeps it so.
            Some(unsafe { &mut (*this.ptr.as_ptr()).value })
        } else {
            None
        }
    }

    /// Whether this is the only owner of either kind.
    fn is_unique(this: &Arc<T, A>) -> bool {
        // Lock the weak count (to `usize::MAX`) when it holds only the owners' shared reference,
        // so no `downgrade` can make a `Weak` while the strong count is read. Acquire:
        // synchronizes with the release decrement of a `Weak` that went away.
        if this.weak().compare_exchange(1, usize::MAX, Acquire, Relaxed).is_err() {
            return false;
        }
        // Acquire: synchronizes with the release decrements of `Arc`s that went away, so their
        // uses of the value happen before the caller's writes.
        let unique = this.strong().load(Acquire) == 1;
        // Release: unlocks, and synchronizes with `downgrade`'s acquire, so the read above
        // happens before any `Weak` made after it.
        this.weak().store(1, Release);
        unique
    }

    /// A [`Weak`] to the same block.
    pub fn downgrade(this: &Arc<T, A>) -> Weak<T, A>
    where
        A: AllocatorClone,
    {
        let alloc = this.alloc.clone();
        let mut seen = this.weak().load(Relaxed);
        // Lock-free: a failed exchange means another thread changed the count, so some thread
        // always progresses. The lock is held by `is_unique` for two instructions.
        loop {
            if seen == usize::MAX {
                hint::spin_loop();
                seen = this.weak().load(Relaxed);
                continue;
            }
            if seen > MOST_COUNT {
                abort_count_overflow(type_name::<T>());
            }
            // Acquire: synchronizes with `is_unique`'s release unlock.
            match this.weak().compare_exchange_weak(seen, seen + 1, Acquire, Relaxed) {
                Ok(_) => return Weak { ptr: this.ptr, alloc },
                Err(now) => seen = now,
            }
        }
    }

    /// How many `Arc`s share the value; another thread may change it at once.
    pub fn strong_count(this: &Arc<T, A>) -> usize {
        this.strong().load(Relaxed)
    }

    /// How many `Weak`s refer to the block; another thread may change it at once.
    pub fn weak_count(this: &Arc<T, A>) -> usize {
        match this.weak().load(Relaxed) {
            // Locked by `is_unique`, which locks only a count of no `Weak`s.
            usize::MAX => 0,
            count => count - 1,
        }
    }

    /// Whether the two share one block.
    pub fn ptr_eq(this: &Arc<T, A>, other: &Arc<T, A>) -> bool {
        ptr::addr_eq(this.ptr.as_ptr(), other.ptr.as_ptr())
    }

    pub fn as_ptr(this: &Arc<T, A>) -> *const T {
        // SAFETY: the block is live (the invariant); a place, no reference made.
        unsafe { &raw const (*this.ptr.as_ptr()).value }
    }

    /// The allocator this owner holds. An associated function, so it never hides a method of
    /// `T`.
    pub fn allocator(this: &Arc<T, A>) -> &A {
        &this.alloc
    }

    /// This owner as a pointer to the value and its allocator; [`Arc::from_raw_in`] takes them
    /// back. The count is kept: until then the value is leaked.
    pub fn into_raw_with_allocator(this: Arc<T, A>) -> (*const T, A) {
        let this = ManuallyDrop::new(this);
        let value = Arc::as_ptr(&this);
        // SAFETY: `this` is never dropped, so its allocator moves out once.
        (value, unsafe { ptr::read(&this.alloc) })
    }

    /// An owner back from [`Arc::into_raw_with_allocator`].
    ///
    /// # Safety
    ///
    /// `value` came from `Arc::into_raw_with_allocator` of an `Arc<T, A>` (an unsizing cast of
    /// the pointer between, as `*const [U; N]` to `*const [U]`, keeps the layout and is fine),
    /// its owner is not taken back twice, and `alloc` is that allocator or an equivalent one.
    pub unsafe fn from_raw_in(value: *const T, alloc: A) -> Arc<T, A> {
        // SAFETY: from a live block (the caller's promise), as `value_block` needs.
        let ptr = unsafe { value_block(value) };
        Arc { ptr, alloc, _owns: PhantomData }
    }
}

/// The offset of a value aligned to `align` in a block: the header's size rounded up to it, as
/// `repr(C)` places the last field.
fn value_offset(align: usize) -> usize {
    let header = Layout::new::<Inner<()>>().size();
    header.next_multiple_of(align)
}

/// The block of the value at `value`.
///
/// # Safety
///
/// `value` points to the value of a live block (its value dropped or not), with metadata
/// valid for it.
unsafe fn value_block<T: ?Sized>(value: *const T) -> NonNull<Inner<T>> {
    // SAFETY: the metadata is valid (the caller's promise); only the alignment is read.
    let align = unsafe { Layout::for_value_raw(value) }.align();
    // SAFETY: the block begins `value_offset(align)` bytes before its value, in one
    // allocation; the metadata carries over.
    let block = unsafe { value.byte_sub(value_offset(align)) } as *mut Inner<T>;
    // SAFETY: inside a block, so not null.
    unsafe { NonNull::new_unchecked(block) }
}

impl<T: ?Sized, A: AllocatorClone> Clone for Arc<T, A> {
    fn clone(&self) -> Arc<T, A> {
        // Cloned first: an allocator clone does not unwind (`AllocatorClone`), but if it did,
        // no count would be raised for it.
        let alloc = self.alloc.clone();
        #[cfg(test)]
        if plant::on(Plant::ArcCloneSkipsCount) {
            return Arc { ptr: self.ptr, alloc, _owns: PhantomData };
        }
        // Relaxed: a new owner is made from one that keeps the value alive, so nothing else
        // needs ordering; the drop's release and acquire order the uses (std's reasoning).
        let old = self.strong().fetch_add(1, Relaxed);
        if old > MOST_COUNT {
            abort_count_overflow(type_name::<T>());
        }
        Arc { ptr: self.ptr, alloc, _owns: PhantomData }
    }
}

// SAFETY (`may_dangle`): the drop drops the `T` it owns (`PhantomData<Inner<T>>` tells drop check
// so) and reads nothing else of it.
unsafe impl<#[may_dangle] T: ?Sized, A: Allocator> Drop for Arc<T, A> {
    fn drop(&mut self) {
        // Release: this owner's uses of the value happen before the decrement, so the last
        // owner's acquire fence sees them.
        if self.strong().fetch_sub(1, Release) != 1 {
            return;
        }
        // Acquire: synchronizes with every other owner's release decrement, so their uses
        // happen before the value is dropped.
        atomic::fence(Acquire);
        // The owners' shared weak reference goes after the value, a panic in its drop among the
        // ways, and frees the block when no `Weak` is left.
        let _weak = Weak { ptr: self.ptr, alloc: &self.alloc };
        // SAFETY: the strong count reached 0: no owner can reach the value again, and it is
        // dropped once.
        unsafe { ptr::drop_in_place(&raw mut (*self.ptr.as_ptr()).value) };
    }
}

impl<T: ?Sized, A: Allocator> Weak<T, A> {
    #[inline]
    fn strong(&self) -> &AtomicUsize {
        // SAFETY: a `Weak` keeps its block live (the invariant).
        unsafe { counts(self.ptr) }.0
    }

    #[inline]
    fn weak(&self) -> &AtomicUsize {
        // SAFETY: as `strong`.
        unsafe { counts(self.ptr) }.1
    }

    /// An [`Arc`], while one is left.
    pub fn upgrade(&self) -> Option<Arc<T, A>>
    where
        A: AllocatorClone,
    {
        let alloc = self.alloc.clone();
        let mut seen = self.strong().load(Relaxed);
        // Lock-free, as `downgrade`.
        loop {
            if seen == 0 {
                return None;
            }
            if seen > MOST_COUNT {
                abort_count_overflow(type_name::<T>());
            }
            // Acquire on success: synchronizes with the release store of `make_mut` and the
            // release decrements, so the value this owner reads is the one they left. Relaxed on
            // failure: the loop reads again.
            match self.strong().compare_exchange_weak(seen, seen + 1, Acquire, Relaxed) {
                Ok(_) => return Some(Arc { ptr: self.ptr, alloc, _owns: PhantomData }),
                Err(now) => seen = now,
            }
        }
    }

    pub fn strong_count(&self) -> usize {
        self.strong().load(Relaxed)
    }

    /// How many `Weak`s refer to the block; 0 once no `Arc` is left, as std's.
    pub fn weak_count(&self) -> usize {
        let weak = self.weak().load(Relaxed);
        let strong = self.strong().load(Relaxed);
        if strong == 0 { 0 } else { weak.saturating_sub(1) }
    }

    pub fn ptr_eq(&self, other: &Weak<T, A>) -> bool {
        ptr::addr_eq(self.ptr.as_ptr(), other.ptr.as_ptr())
    }

    /// Where the value is, or was: a pointer only to compare or to hand to
    /// [`Weak::from_raw_in`], never to read through unless an `Arc` is known to live.
    pub fn as_ptr(&self) -> *const T {
        // SAFETY: the block is live (the invariant); a place, no reference made, so a dropped
        // value is fine.
        unsafe { &raw const (*self.ptr.as_ptr()).value }
    }

    /// This weak reference as a pointer and its allocator; the weak count is kept.
    pub fn into_raw_with_allocator(self) -> (*const T, A) {
        let this = ManuallyDrop::new(self);
        let value = this.as_ptr();
        // SAFETY: `this` is never dropped, so its allocator moves out once.
        (value, unsafe { ptr::read(&this.alloc) })
    }

    /// A weak reference back from [`Weak::into_raw_with_allocator`].
    ///
    /// # Safety
    ///
    /// As [`Arc::from_raw_in`], for a pointer a `Weak` gave.
    pub unsafe fn from_raw_in(value: *const T, alloc: A) -> Weak<T, A> {
        // SAFETY: the weak count kept the block live (the caller's promise).
        let ptr = unsafe { value_block(value) };
        Weak { ptr, alloc }
    }
}

impl<T: ?Sized, A: AllocatorClone> Clone for Weak<T, A> {
    fn clone(&self) -> Weak<T, A> {
        let alloc = self.alloc.clone();
        // Relaxed: as `Arc::clone`. The count cannot be locked: `is_unique` locks only a count
        // with no `Weak`, and this is one.
        let old = self.weak().fetch_add(1, Relaxed);
        if old > MOST_COUNT {
            abort_count_overflow(type_name::<T>());
        }
        Weak { ptr: self.ptr, alloc }
    }
}

impl<T: ?Sized, A: Allocator> Drop for Weak<T, A> {
    fn drop(&mut self) {
        // Release: this reference's reads of the counts happen before the decrement.
        if self.weak().fetch_sub(1, Release) != 1 {
            return;
        }
        // Acquire: synchronizes with every other release decrement of `weak` (the last `Arc`'s
        // comes after the value's drop), so the block is unused when it is freed.
        atomic::fence(Acquire);
        // SAFETY: the block is live until freed just below; its metadata gives its layout,
        // whether or not the value in it was dropped.
        let layout = unsafe { Layout::for_value_raw(self.ptr.as_ptr()) };
        // SAFETY: the block came from an allocator equivalent to this one for `layout`, and the
        // weak count reached 0, so it is freed once.
        unsafe { self.alloc.deallocate(self.ptr.cast(), layout) };
    }
}

impl<T: ?Sized, A: Allocator> fmt::Debug for Weak<T, A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("(Weak)")
    }
}

impl<T: ?Sized, A: Allocator> Deref for Arc<T, A> {
    type Target = T;

    #[inline]
    fn deref(&self) -> &T {
        // SAFETY: the strong count includes this owner, so the value is live (the invariant).
        unsafe { &(*self.ptr.as_ptr()).value }
    }
}

impl<T: ?Sized, A: Allocator> AsRef<T> for Arc<T, A> {
    fn as_ref(&self) -> &T {
        self
    }
}

impl<T: ?Sized, A: Allocator> Borrow<T> for Arc<T, A> {
    fn borrow(&self) -> &T {
        self
    }
}

impl<T: ?Sized + fmt::Debug, A: Allocator> fmt::Debug for Arc<T, A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&**self, f)
    }
}

impl<T: ?Sized + fmt::Display, A: Allocator> fmt::Display for Arc<T, A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&**self, f)
    }
}

impl<T: ?Sized + PartialEq, A: Allocator> PartialEq for Arc<T, A> {
    #[inline]
    fn eq(&self, other: &Arc<T, A>) -> bool {
        PartialEq::eq(&**self, &**other)
    }
}

impl<T: ?Sized + Eq, A: Allocator> Eq for Arc<T, A> {}

impl<T: ?Sized + PartialOrd, A: Allocator> PartialOrd for Arc<T, A> {
    fn partial_cmp(&self, other: &Arc<T, A>) -> Option<Ordering> {
        PartialOrd::partial_cmp(&**self, &**other)
    }
}

impl<T: ?Sized + Ord, A: Allocator> Ord for Arc<T, A> {
    fn cmp(&self, other: &Arc<T, A>) -> Ordering {
        Ord::cmp(&**self, &**other)
    }
}

impl<T: ?Sized + Hash, A: Allocator> Hash for Arc<T, A> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        (**self).hash(state);
    }
}
