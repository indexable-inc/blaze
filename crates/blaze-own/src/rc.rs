//! [`Rc`] and [`Weak`]: [`Arc`](crate::Arc)'s shape counted on one thread, with plain cells
//! for counts. Neither is `Send` or `Sync` (the `NonNull` inside makes them neither).
//!
//! Not here yet, as for `Arc`: `make_mut` for unsized values, `Weak::new` without a block.

use core::{
    alloc::{Allocator, AllocatorClone, Layout, StaticAllocator},
    any::type_name,
    borrow::Borrow,
    cell::Cell,
    cmp::Ordering,
    fmt,
    hash::{Hash, Hasher},
    marker::{PhantomData, Unsize},
    mem::{DropGuard, ManuallyDrop},
    ops::{CoerceUnsized, Deref},
    pin::Pin,
    ptr::{self, NonNull},
};

use crate::{
    error::{AllocFailure, MOST_COUNT, abort_count_overflow, fail},
    vec::Refused,
};

/// The block, as `Arc`'s: `strong` counts the `Rc`s, `weak` the `Weak`s plus one every `Rc`
/// holds together.
#[repr(C)]
struct Inner<T: ?Sized> {
    strong: Cell<usize>,
    weak: Cell<usize>,
    value: T,
}

fn slice_layout<T>(len: usize) -> Option<Layout> {
    let values = Layout::array::<T>(len).ok()?;
    let (layout, _offset) = Layout::new::<Inner<()>>().extend(values).ok()?;
    Some(layout.pad_to_align())
}

/// A shared `T` in `A`, counted on one thread: one block for value and counts; a clone is a plain increment.
///
/// # Invariants
///
/// As `Arc`'s: `ptr` is a block of `alloc` (or an equivalent clone) for `Layout::for_value`
/// of it, whose `strong` count includes this owner.
pub struct Rc<T: ?Sized, A: Allocator> {
    ptr: NonNull<Inner<T>>,
    alloc: A,
    _owns: PhantomData<Inner<T>>,
}

/// A reference that keeps an [`Rc`]'s block but not its value.
pub struct Weak<T: ?Sized, A: Allocator> {
    ptr: NonNull<Inner<T>>,
    alloc: A,
}

impl<T: ?Sized, A: Allocator> Unpin for Rc<T, A> {}

impl<T: ?Sized + Unsize<U>, U: ?Sized, A: Allocator> CoerceUnsized<Rc<U, A>> for Rc<T, A> {}
impl<T: ?Sized + Unsize<U>, U: ?Sized, A: Allocator> CoerceUnsized<Weak<U, A>> for Weak<T, A> {}

/// The counts of the block at `ptr`, read through raw places.
///
/// # Safety
///
/// The block at `ptr` is live.
unsafe fn counts<'a, T: ?Sized>(ptr: NonNull<Inner<T>>) -> (&'a Cell<usize>, &'a Cell<usize>) {
    let raw = ptr.as_ptr();
    // SAFETY: the block is live (the caller's promise); its counts are cells, shared by the
    // owners on this one thread.
    unsafe { (&*(&raw const (*raw).strong), &*(&raw const (*raw).weak)) }
}

/// One more owner on `count`, aborting before it could wrap (a `mem::forget` loop).
fn raise(count: &Cell<usize>, what: &'static str) {
    let now = count.get();
    if now > MOST_COUNT {
        abort_count_overflow(what);
    }
    count.set(now + 1);
}

impl<T, A: Allocator> Rc<T, A> {
    pub fn new_in(value: T, alloc: A) -> Rc<T, A> {
        match Rc::try_new_in(value, alloc) {
            Ok(rc) => rc,
            Err(refused) => fail(refused.why),
        }
    }

    pub fn try_new_in(value: T, alloc: A) -> Result<Rc<T, A>, Refused<T>> {
        let layout = Layout::new::<Inner<T>>();
        let ptr = match alloc.allocate(layout) {
            Ok(block) => block.cast::<Inner<T>>(),
            Err(_) => {
                let why = AllocFailure::Refused { what: type_name::<T>(), layout };
                return Err(Refused { value, why });
            }
        };
        let inner = Inner { strong: Cell::new(1), weak: Cell::new(1), value };
        // SAFETY: a fresh block of `Inner<T>`'s layout, empty.
        unsafe { ptr.as_ptr().write(inner) };
        Ok(Rc { ptr, alloc, _owns: PhantomData })
    }

    /// As [`Arc::new_cyclic_in`](crate::Arc::new_cyclic_in).
    pub fn new_cyclic_in(make: impl FnOnce(&Weak<T, A>) -> T, alloc: A) -> Rc<T, A> {
        let layout = Layout::new::<Inner<T>>();
        let Ok(block) = alloc.allocate(layout) else {
            fail(AllocFailure::Refused { what: type_name::<T>(), layout });
        };
        let ptr = block.cast::<Inner<T>>();
        // SAFETY: a fresh block of `Inner<T>`'s layout: counts written, value left for `make`.
        unsafe {
            (&raw mut (*ptr.as_ptr()).strong).write(Cell::new(0));
            (&raw mut (*ptr.as_ptr()).weak).write(Cell::new(1));
        }
        // The owners' shared weak reference; a panic in `make` drops it, freeing the block.
        let weak = Weak { ptr, alloc };
        let value = make(&weak);
        // SAFETY: the value's place is empty and nothing reaches it while strong is 0.
        unsafe { (&raw mut (*ptr.as_ptr()).value).write(value) };
        weak.strong().set(1);
        let weak = ManuallyDrop::new(weak);
        // SAFETY: `weak` is never dropped; its allocator moves out once.
        let alloc = unsafe { ptr::read(&weak.alloc) };
        Rc { ptr, alloc, _owns: PhantomData }
    }

    /// As [`Arc::pin_in`](crate::Arc::pin_in).
    pub fn pin_in(value: T, alloc: A) -> Pin<Rc<T, A>>
    where
        A: StaticAllocator,
    {
        // SAFETY: as `Arc::pin_in`: no `&mut T` through a `Pin<Rc>`, and the value stays in its
        // block until dropped.
        unsafe { Pin::new_unchecked(Rc::new_in(value, alloc)) }
    }

    /// The value, when this is the only `Rc`; else this `Rc` back.
    pub fn try_unwrap(this: Rc<T, A>) -> Result<T, Rc<T, A>> {
        if this.strong().get() != 1 {
            return Err(this);
        }
        this.strong().set(0);
        let this = ManuallyDrop::new(this);
        // SAFETY: the strong count went to 0 from 1: the value is ours alone and moves out once;
        // `this` is never dropped, so its allocator moves out once.
        let (value, alloc) = unsafe { (ptr::read(&(*this.ptr.as_ptr()).value), ptr::read(&this.alloc)) };
        drop(Weak { ptr: this.ptr, alloc });
        Ok(value)
    }

    /// The value, when this is the last `Rc`; else `None`, and this owner is gone.
    pub fn into_inner(this: Rc<T, A>) -> Option<T> {
        Rc::try_unwrap(this).ok()
    }

    /// As [`Arc::make_mut`](crate::Arc::make_mut).
    pub fn make_mut(this: &mut Rc<T, A>) -> &mut T
    where
        T: Clone,
        A: AllocatorClone,
    {
        if this.strong().get() != 1 {
            let copy = Rc::new_in((**this).clone(), this.alloc.clone());
            *this = copy;
        } else if this.weak().get() != 1 {
            // The only owner, with `Weak`s: the value moves to a new block, and the old one is
            // left to the `Weak`s with a strong count of 0.
            let alloc = this.alloc.clone();
            let layout = Layout::new::<Inner<T>>();
            let Ok(block) = alloc.allocate(layout) else {
                fail(AllocFailure::Refused { what: type_name::<T>(), layout });
            };
            let block = block.cast::<Inner<T>>();
            this.strong().set(0);
            // SAFETY: this is the only owner and its count is now 0, so the value moves out of
            // the old block once; the new block is fresh, of `Inner<T>`'s layout.
            unsafe {
                let value = ptr::read(&(*this.ptr.as_ptr()).value);
                block.as_ptr().write(Inner { strong: Cell::new(1), weak: Cell::new(1), value });
            }
            let old = Weak { ptr: this.ptr, alloc };
            this.ptr = block;
            drop(old);
        }
        // SAFETY: this owner is now the only one of either kind.
        unsafe { &mut (*this.ptr.as_ptr()).value }
    }
}

impl<T, A: Allocator> Rc<[T], A> {
    /// Clones of `values` shared, in one block in `alloc`.
    pub fn from_slice_in(values: &[T], alloc: A) -> Rc<[T], A>
    where
        T: Clone,
    {
        match Rc::try_from_slice_in(values, alloc) {
            Ok(rc) => rc,
            Err(failure) => fail(failure),
        }
    }

    /// [`Rc::from_slice_in`], or why there is no block; nothing is left allocated.
    pub fn try_from_slice_in(values: &[T], alloc: A) -> Result<Rc<[T], A>, AllocFailure>
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
        let inner = ptr::slice_from_raw_parts_mut(block.as_ptr().cast::<T>(), len) as *mut Inner<[T]>;
        // SAFETY: the fresh block laid out as `Inner<[T]>` of `len` (`slice_layout`).
        let first = unsafe {
            (&raw mut (*inner).strong).write(Cell::new(1));
            (&raw mut (*inner).weak).write(Cell::new(1));
            (&raw mut (*inner).value).cast::<T>()
        };
        let mut made = DropGuard::new(0usize, |made| {
            // SAFETY: the first `made` slots hold clones, dropped once; the block is freed once
            // and no `Rc` was made of it.
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
        // SAFETY: from a non-null block.
        let ptr = unsafe { NonNull::new_unchecked(inner) };
        Ok(Rc { ptr, alloc, _owns: PhantomData })
    }
}

impl<A: Allocator> Rc<str, A> {
    pub fn from_str_in(text: &str, alloc: A) -> Rc<str, A> {
        let bytes = ManuallyDrop::new(Rc::from_slice_in(text.as_bytes(), alloc));
        // SAFETY: `bytes` is never dropped, so the allocator moves out once.
        let alloc = unsafe { ptr::read(&bytes.alloc) };
        let raw = bytes.ptr.as_ptr() as *mut Inner<str>;
        // SAFETY: from a non-null pointer; `str` has `[u8]`'s layout and metadata, and the bytes
        // are UTF-8, copied from a `str`.
        let ptr = unsafe { NonNull::new_unchecked(raw) };
        Rc { ptr, alloc, _owns: PhantomData }
    }
}

impl<T: ?Sized, A: Allocator> Rc<T, A> {
    #[inline]
    fn strong(&self) -> &Cell<usize> {
        // SAFETY: an `Rc` keeps its block live.
        unsafe { counts(self.ptr) }.0
    }

    #[inline]
    fn weak(&self) -> &Cell<usize> {
        // SAFETY: as `strong`.
        unsafe { counts(self.ptr) }.1
    }

    /// The value to change, when no other `Rc` or `Weak` shares it.
    pub fn get_mut(this: &mut Rc<T, A>) -> Option<&mut T> {
        if this.strong().get() == 1 && this.weak().get() == 1 {
            // SAFETY: no other owner of either kind, and `&mut this` keeps it so.
            Some(unsafe { &mut (*this.ptr.as_ptr()).value })
        } else {
            None
        }
    }

    pub fn downgrade(this: &Rc<T, A>) -> Weak<T, A>
    where
        A: AllocatorClone,
    {
        let alloc = this.alloc.clone();
        raise(this.weak(), type_name::<T>());
        Weak { ptr: this.ptr, alloc }
    }

    pub fn strong_count(this: &Rc<T, A>) -> usize {
        this.strong().get()
    }

    pub fn weak_count(this: &Rc<T, A>) -> usize {
        this.weak().get() - 1
    }

    pub fn ptr_eq(this: &Rc<T, A>, other: &Rc<T, A>) -> bool {
        ptr::addr_eq(this.ptr.as_ptr(), other.ptr.as_ptr())
    }

    pub fn as_ptr(this: &Rc<T, A>) -> *const T {
        // SAFETY: the block is live; a place, no reference made.
        unsafe { &raw const (*this.ptr.as_ptr()).value }
    }

    pub fn allocator(this: &Rc<T, A>) -> &A {
        &this.alloc
    }

    /// As [`Arc::into_raw_with_allocator`](crate::Arc::into_raw_with_allocator).
    pub fn into_raw_with_allocator(this: Rc<T, A>) -> (*const T, A) {
        let this = ManuallyDrop::new(this);
        let value = Rc::as_ptr(&this);
        // SAFETY: `this` is never dropped, so its allocator moves out once.
        (value, unsafe { ptr::read(&this.alloc) })
    }

    /// An owner back from [`Rc::into_raw_with_allocator`].
    ///
    /// # Safety
    ///
    /// As [`Arc::from_raw_in`](crate::Arc::from_raw_in), for an `Rc`.
    pub unsafe fn from_raw_in(value: *const T, alloc: A) -> Rc<T, A> {
        // SAFETY: from a live block (the caller's promise).
        let ptr = unsafe { value_block(value) };
        Rc { ptr, alloc, _owns: PhantomData }
    }
}

/// The block of the value at `value`: the header's size rounded up to the value's alignment
/// before it, as `repr(C)` places the last field.
///
/// # Safety
///
/// `value` points to the value of a live block (dropped or not), with metadata valid for it.
unsafe fn value_block<T: ?Sized>(value: *const T) -> NonNull<Inner<T>> {
    // SAFETY: the metadata is valid (the caller's promise); only the alignment is read.
    let align = unsafe { Layout::for_value_raw(value) }.align();
    let offset = Layout::new::<Inner<()>>().size().next_multiple_of(align);
    // SAFETY: the block begins `offset` bytes before its value, in one allocation.
    let block = unsafe { value.byte_sub(offset) } as *mut Inner<T>;
    // SAFETY: inside a block, so not null.
    unsafe { NonNull::new_unchecked(block) }
}

impl<T: ?Sized, A: AllocatorClone> Clone for Rc<T, A> {
    fn clone(&self) -> Rc<T, A> {
        let alloc = self.alloc.clone();
        raise(self.strong(), type_name::<T>());
        Rc { ptr: self.ptr, alloc, _owns: PhantomData }
    }
}

// SAFETY (`may_dangle`): the drop drops the `T` it owns and reads nothing else of it.
unsafe impl<#[may_dangle] T: ?Sized, A: Allocator> Drop for Rc<T, A> {
    fn drop(&mut self) {
        let strong = self.strong().get() - 1;
        self.strong().set(strong);
        if strong != 0 {
            return;
        }
        // The owners' shared weak reference goes after the value, a panic in its drop among the
        // ways.
        let _weak = Weak { ptr: self.ptr, alloc: &self.alloc };
        // SAFETY: the strong count reached 0: no owner reaches the value again; dropped once.
        unsafe { ptr::drop_in_place(&raw mut (*self.ptr.as_ptr()).value) };
    }
}

impl<T: ?Sized, A: Allocator> Weak<T, A> {
    #[inline]
    fn strong(&self) -> &Cell<usize> {
        // SAFETY: a `Weak` keeps its block live.
        unsafe { counts(self.ptr) }.0
    }

    #[inline]
    fn weak(&self) -> &Cell<usize> {
        // SAFETY: as `strong`.
        unsafe { counts(self.ptr) }.1
    }

    pub fn upgrade(&self) -> Option<Rc<T, A>>
    where
        A: AllocatorClone,
    {
        if self.strong().get() == 0 {
            return None;
        }
        let alloc = self.alloc.clone();
        raise(self.strong(), type_name::<T>());
        Some(Rc { ptr: self.ptr, alloc, _owns: PhantomData })
    }

    pub fn strong_count(&self) -> usize {
        self.strong().get()
    }

    pub fn weak_count(&self) -> usize {
        if self.strong().get() == 0 { 0 } else { self.weak().get() - 1 }
    }

    pub fn ptr_eq(&self, other: &Weak<T, A>) -> bool {
        ptr::addr_eq(self.ptr.as_ptr(), other.ptr.as_ptr())
    }

    /// Where the value is, or was; as [`arc::Weak::as_ptr`](crate::arc::Weak::as_ptr).
    pub fn as_ptr(&self) -> *const T {
        // SAFETY: the block is live; a place, no reference made.
        unsafe { &raw const (*self.ptr.as_ptr()).value }
    }

    pub fn into_raw_with_allocator(self) -> (*const T, A) {
        let this = ManuallyDrop::new(self);
        let value = this.as_ptr();
        // SAFETY: `this` is never dropped, so its allocator moves out once.
        (value, unsafe { ptr::read(&this.alloc) })
    }

    /// # Safety
    ///
    /// As [`Rc::from_raw_in`], for a pointer a `Weak` gave.
    pub unsafe fn from_raw_in(value: *const T, alloc: A) -> Weak<T, A> {
        // SAFETY: the weak count kept the block live (the caller's promise).
        let ptr = unsafe { value_block(value) };
        Weak { ptr, alloc }
    }
}

impl<T: ?Sized, A: AllocatorClone> Clone for Weak<T, A> {
    fn clone(&self) -> Weak<T, A> {
        let alloc = self.alloc.clone();
        raise(self.weak(), type_name::<T>());
        Weak { ptr: self.ptr, alloc }
    }
}

impl<T: ?Sized, A: Allocator> Drop for Weak<T, A> {
    fn drop(&mut self) {
        let weak = self.weak().get() - 1;
        self.weak().set(weak);
        if weak != 0 {
            return;
        }
        // SAFETY: the block is live until freed below; its metadata gives its layout.
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

impl<T: ?Sized, A: Allocator> Deref for Rc<T, A> {
    type Target = T;

    #[inline]
    fn deref(&self) -> &T {
        // SAFETY: the strong count includes this owner, so the value is live.
        unsafe { &(*self.ptr.as_ptr()).value }
    }
}

impl<T: ?Sized, A: Allocator> AsRef<T> for Rc<T, A> {
    fn as_ref(&self) -> &T {
        self
    }
}

impl<T: ?Sized, A: Allocator> Borrow<T> for Rc<T, A> {
    fn borrow(&self) -> &T {
        self
    }
}

impl<T: ?Sized + fmt::Debug, A: Allocator> fmt::Debug for Rc<T, A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&**self, f)
    }
}

impl<T: ?Sized + fmt::Display, A: Allocator> fmt::Display for Rc<T, A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&**self, f)
    }
}

impl<T: ?Sized + PartialEq, A: Allocator> PartialEq for Rc<T, A> {
    fn eq(&self, other: &Rc<T, A>) -> bool {
        PartialEq::eq(&**self, &**other)
    }
}

impl<T: ?Sized + Eq, A: Allocator> Eq for Rc<T, A> {}

impl<T: ?Sized + PartialOrd, A: Allocator> PartialOrd for Rc<T, A> {
    fn partial_cmp(&self, other: &Rc<T, A>) -> Option<Ordering> {
        PartialOrd::partial_cmp(&**self, &**other)
    }
}

impl<T: ?Sized + Ord, A: Allocator> Ord for Rc<T, A> {
    fn cmp(&self, other: &Rc<T, A>) -> Ordering {
        Ord::cmp(&**self, &**other)
    }
}

impl<T: ?Sized + Hash, A: Allocator> Hash for Rc<T, A> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        (**self).hash(state);
    }
}
