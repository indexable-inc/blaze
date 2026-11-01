//! [`RegionVec<T>`]: a column over a reserved region (docs/v2/plan/design/data.md). It
//! grows in place by committing pages, so pushing never reallocates or copies, a `&T` taken
//! before a push could stay valid after it (the borrow checker still asks for `&mut` to push),
//! and an index is a stable `u32` ID for the life of the column.
//!
//! Cost: one bounds compare and one store per push; a commit system call every
//! `max(committed / 4, 64 KB)` bytes of growth, up to 64 MB per call.

use std::{
    fmt,
    marker::PhantomData,
    mem::{self, MaybeUninit},
    ops::{Deref, DerefMut},
    ptr::{self, NonNull},
    slice,
};

use crate::{MemError, Region, Tag, error::fail};

/// A growable array over a reserved region: `Vec<T>`'s API for what a column needs, with no
/// move on growth. `max_len` is fixed at creation; pushing past it is an error naming the tag.
pub struct RegionVec<T> {
    region: Region,
    len: usize,
    /// Elements committed: pushing below this touches no system call.
    capacity: usize,
    max_len: usize,
    _owns: PhantomData<T>,
}

// SAFETY: a `RegionVec<T>` owns its `T`s as a `Vec<T>` does; the region is an address range.
unsafe impl<T: Send> Send for RegionVec<T> {}
// SAFETY: `&RegionVec<T>` gives only `&T`.
unsafe impl<T: Sync> Sync for RegionVec<T> {}

impl<T> RegionVec<T> {
    const SIZE: usize = {
        assert!(mem::size_of::<T>() != 0, "a RegionVec of a zero-sized type holds nothing");
        assert!(
            mem::align_of::<T>() <= 4096,
            "a RegionVec's base is page-aligned, so its elements align to at most a page"
        );
        mem::size_of::<T>()
    };

    /// Reserves room for `max_len` elements, committing none.
    pub fn new(max_len: usize, tag: Tag) -> Result<RegionVec<T>, MemError> {
        let bytes = max_len.checked_mul(Self::SIZE).ok_or(MemError::TooLarge {
            tag,
            what: "a column's length",
            wanted: max_len,
        })?;
        Ok(RegionVec::over(Region::reserve(bytes, tag)?))
    }

    /// As [`RegionVec::new`], on huge pages where the kernel offers them.
    pub fn new_huge(max_len: usize, tag: Tag) -> Result<RegionVec<T>, MemError> {
        let bytes = max_len.checked_mul(Self::SIZE).ok_or(MemError::TooLarge {
            tag,
            what: "a column's length",
            wanted: max_len,
        })?;
        Ok(RegionVec::over(Region::reserve_huge(bytes, tag)?))
    }

    fn over(region: Region) -> RegionVec<T> {
        let max_len = region.reserved() / Self::SIZE;
        RegionVec { region, len: 0, capacity: 0, max_len, _owns: PhantomData }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The most elements it can ever hold.
    pub fn max_len(&self) -> usize {
        self.max_len
    }

    /// Elements it holds room for without committing more.
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    pub fn tag(&self) -> Tag {
        self.region.tag()
    }

    fn ptr(&self) -> *mut T {
        self.region.base().as_ptr().cast::<T>()
    }

    /// Makes room for `additional` more elements.
    pub fn reserve(&mut self, additional: usize) -> Result<(), MemError> {
        let want = self.len.checked_add(additional).ok_or(MemError::TooLarge {
            tag: self.tag(),
            what: "a column's length",
            wanted: additional,
        })?;
        if want <= self.capacity {
            return Ok(());
        }
        self.grow(want)
    }

    #[cold]
    fn grow(&mut self, want: usize) -> Result<(), MemError> {
        if want > self.max_len {
            return Err(MemError::Exhausted {
                tag: self.tag(),
                reserved: self.region.reserved(),
                wanted: want * Self::SIZE,
            });
        }
        self.region.ensure(want * Self::SIZE)?;
        self.capacity = (self.region.committed() / Self::SIZE).min(self.max_len);
        Ok(())
    }

    /// Appends `value` and returns its index.
    #[inline]
    pub fn try_push(&mut self, value: T) -> Result<usize, MemError> {
        if self.len == self.capacity {
            self.grow(self.len + 1)?;
        }
        let index = self.len;
        // SAFETY: `index < capacity`, so the slot is committed, inside the reservation and
        // aligned (a page-aligned base plus a multiple of `size_of::<T>()`); it holds no value.
        unsafe { self.ptr().wrapping_add(index).write(value) };
        self.len = index + 1;
        Ok(index)
    }

    /// Appends `value` and returns its index; stops with the error when the column's
    /// reservation is used up or memory cannot be had.
    #[inline]
    #[track_caller]
    pub fn push(&mut self, value: T) -> usize {
        match self.try_push(value) {
            Ok(index) => index,
            Err(error) => fail(&error),
        }
    }

    /// Appends `value` and returns its index as a `u32` ID; stops when the column holds
    /// `u32::MAX` elements.
    #[inline]
    #[track_caller]
    pub fn push_id(&mut self, value: T) -> u32 {
        let index = self.push(value);
        match u32::try_from(index) {
            Ok(id) => id,
            Err(_) => fail(&MemError::TooLarge { tag: self.tag(), what: "a u32 ID", wanted: index }),
        }
    }

    pub fn pop(&mut self) -> Option<T> {
        if self.len == 0 {
            return None;
        }
        self.len -= 1;
        // SAFETY: the slot at the old last index holds a value, which `len` no longer counts,
        // so it is read out once.
        Some(unsafe { self.ptr().wrapping_add(self.len).read() })
    }

    /// Drops the elements from `len` on.
    pub fn truncate(&mut self, len: usize) {
        if len >= self.len {
            return;
        }
        let tail = ptr::slice_from_raw_parts_mut(self.ptr().wrapping_add(len), self.len - len);
        self.len = len;
        // SAFETY: `tail` is the initialized elements past the new length, which nothing counts
        // any more, dropped once (the length went down first, so a panicking drop leaks rather
        // than drops twice).
        unsafe { ptr::drop_in_place(tail) };
    }

    pub fn clear(&mut self) {
        self.truncate(0);
    }

    /// Gives the pages past what it holds back to the kernel (keeping `keep_len` elements of
    /// room): after a burst, so resident memory returns to the working set.
    pub fn shrink_to(&mut self, keep_len: usize) {
        let keep = keep_len.max(self.len).saturating_mul(Self::SIZE);
        if self.region.decommit_after(keep).is_ok() {
            self.capacity = (self.region.committed() / Self::SIZE).min(self.max_len);
        }
    }

    pub fn as_slice(&self) -> &[T] {
        // SAFETY: the first `len` slots are initialized, committed and aligned; the base is not
        // null.
        unsafe { slice::from_raw_parts(self.ptr(), self.len) }
    }

    pub fn as_mut_slice(&mut self) -> &mut [T] {
        // SAFETY: as for `as_slice`, and `&mut self` makes the borrow unique.
        unsafe { slice::from_raw_parts_mut(self.ptr(), self.len) }
    }

    /// The spare committed room past the length, to fill before [`RegionVec::set_len`].
    pub fn spare_capacity_mut(&mut self) -> &mut [MaybeUninit<T>] {
        let spare = self.ptr().wrapping_add(self.len).cast::<MaybeUninit<T>>();
        // SAFETY: `[len, capacity)` is committed, aligned and holds no value; `MaybeUninit`
        // needs no initialization.
        unsafe { slice::from_raw_parts_mut(spare, self.capacity - self.len) }
    }

    /// Sets the length after the caller filled the spare room.
    ///
    /// # Safety
    ///
    /// `len <= capacity()`, and every element below `len` is initialized.
    pub unsafe fn set_len(&mut self, len: usize) {
        debug_assert!(len <= self.capacity);
        self.len = len;
    }

    /// The address of element `index`, stable for the life of the column (`index < len`).
    pub fn as_ptr_at(&self, index: usize) -> Option<NonNull<T>> {
        (index < self.len).then(|| self.region.at(index * Self::SIZE).cast::<T>())
    }

    pub fn get(&self, index: usize) -> Option<&T> {
        self.as_slice().get(index)
    }

    pub fn get_mut(&mut self, index: usize) -> Option<&mut T> {
        self.as_mut_slice().get_mut(index)
    }

    pub fn last(&self) -> Option<&T> {
        self.as_slice().last()
    }
}

impl<T: Copy> RegionVec<T> {
    pub fn try_extend_from_slice(&mut self, values: &[T]) -> Result<(), MemError> {
        self.reserve(values.len())?;
        let spare = self.spare_capacity_mut();
        for (slot, value) in spare.iter_mut().zip(values) {
            slot.write(*value);
        }
        self.len += values.len();
        Ok(())
    }

    #[track_caller]
    pub fn extend_from_slice(&mut self, values: &[T]) {
        if let Err(error) = self.try_extend_from_slice(values) {
            fail(&error)
        }
    }
}

impl<T> Drop for RegionVec<T> {
    fn drop(&mut self) {
        if mem::needs_drop::<T>() {
            self.clear();
        }
    }
}

impl<T> Deref for RegionVec<T> {
    type Target = [T];

    fn deref(&self) -> &[T] {
        self.as_slice()
    }
}

impl<T> DerefMut for RegionVec<T> {
    fn deref_mut(&mut self) -> &mut [T] {
        self.as_mut_slice()
    }
}

impl<T> Extend<T> for RegionVec<T> {
    #[track_caller]
    fn extend<I: IntoIterator<Item = T>>(&mut self, iter: I) {
        for value in iter {
            self.push(value);
        }
    }
}

impl<'a, T> IntoIterator for &'a RegionVec<T> {
    type Item = &'a T;
    type IntoIter = slice::Iter<'a, T>;

    fn into_iter(self) -> slice::Iter<'a, T> {
        self.as_slice().iter()
    }
}

impl<T: fmt::Debug> fmt::Debug for RegionVec<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.as_slice()).finish()
    }
}
