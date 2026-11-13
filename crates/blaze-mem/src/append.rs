//! [`AppendVec<T>`]: a column one writer appends to while any number of threads read it with
//! no lock (docs/v2/plan/design/concurrency.md "no lock and no shared write on any read
//! path"). The writer writes the value in place and publishes the new length with one release
//! store; a reader loads the length with acquire and indexes from a base that never moves.
//!
//! Unlike [`Column`](crate::Column), whose pushers claim indices at once and publish each value
//! by its own ready bit, the length here is the whole truth, so [`AppendVec::as_slice`] is the
//! published prefix in one load: for tables a scan reads whole (directory tables, path lists).
//!
//! Cost per append: the value's store and one release store of the length, plus, for an
//! [`AppendVec::append`] on its own, an uncontended swap and store of the writer flag; a
//! [`Writer`] takes the flag once for all its appends, and its bulk appends publish many values
//! with one store. A commit system call per quarter of growth. Per read: one acquire load and a
//! bounds compare.

use std::{
    fmt,
    marker::PhantomData,
    mem, slice,
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
};

use crate::{MemError, Region, Tag};

pub struct AppendVec<T> {
    region: Region,
    /// Elements published: every one below it is written and never written again.
    len: AtomicUsize,
    /// Set while an append runs, so a second writer at once stops instead of racing.
    writing: AtomicBool,
    max: usize,
    _owns: PhantomData<T>,
}

// SAFETY: an element is written once, by the one writer (`writing`), before the release store
// of a length that covers it, and read only below a length loaded with acquire; `T: Sync` lets
// every thread read it, `T: Send` lets the thread that drops the column drop it.
unsafe impl<T: Send + Sync> Sync for AppendVec<T> {}
// SAFETY: the column owns its values as a `Vec` does.
unsafe impl<T: Send> Send for AppendVec<T> {}

impl<T> AppendVec<T> {
    const SIZE: usize = {
        assert!(mem::size_of::<T>() != 0, "an AppendVec of a zero-sized type holds nothing");
        assert!(mem::align_of::<T>() <= 4096, "an AppendVec's values align to at most a page");
        mem::size_of::<T>()
    };

    /// A column of at most `max` values (at most `u32::MAX`) for `tag`, committing none.
    pub fn new(max: usize, tag: Tag) -> Result<AppendVec<T>, MemError> {
        let max = max.min(u32::MAX as usize);
        AppendVec::over(Region::reserve(AppendVec::<T>::bytes(max, tag)?, tag)?, max)
    }

    /// As [`AppendVec::new`], on huge pages where the kernel offers them.
    pub fn new_huge(max: usize, tag: Tag) -> Result<AppendVec<T>, MemError> {
        let max = max.min(u32::MAX as usize);
        AppendVec::over(Region::reserve_huge(AppendVec::<T>::bytes(max, tag)?, tag)?, max)
    }

    fn bytes(max: usize, tag: Tag) -> Result<usize, MemError> {
        max.checked_mul(Self::SIZE).ok_or(MemError::TooLarge {
            tag,
            what: "a column's length",
            wanted: max,
        })
    }

    fn over(region: Region, max: usize) -> Result<AppendVec<T>, MemError> {
        Ok(AppendVec {
            region,
            len: AtomicUsize::new(0),
            writing: AtomicBool::new(false),
            max,
            _owns: PhantomData,
        })
    }

    pub fn tag(&self) -> Tag {
        self.region.tag()
    }

    pub fn max_len(&self) -> usize {
        self.max
    }

    /// Elements published.
    #[inline]
    pub fn len(&self) -> u32 {
        self.len.load(Ordering::Acquire) as u32
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The column's one writer until it is dropped: appends through it take no atomic
    /// read-modify-write, only the value's store and the length's release store. Refused
    /// ([`MemError::Refused`]) while another writer or append is running: its appends must be
    /// serialized, and a second writer at once is refused rather than racing.
    pub fn writer(&self) -> Result<Writer<'_, T>, MemError> {
        if self.writing.swap(true, Ordering::Acquire) {
            return Err(MemError::Refused {
                tag: self.tag(),
                what: "a second writer of an AppendVec while one runs (rule: its appends are \
                       serialized, one writer at a time)",
            });
        }
        Ok(Writer { column: self })
    }

    /// Appends `value` and returns its index, readable by every thread from now on. One writer
    /// at a time: the caller serializes its appends (its own lock or a single owner); a second
    /// append while one runs is refused. Many appends in a row are cheaper through
    /// [`AppendVec::writer`].
    #[inline]
    pub fn append(&self, value: T) -> Result<u32, MemError> {
        self.writer()?.append(value)
    }

    /// Appends every value of `values` with one publish and returns the first one's index.
    pub fn extend_from_slice(&self, values: &[T]) -> Result<u32, MemError>
    where
        T: Copy,
    {
        self.writer()?.extend_from_slice(values)
    }

    /// The published elements.
    #[inline]
    pub fn as_slice(&self) -> &[T] {
        let len = self.len.load(Ordering::Acquire);
        // SAFETY: the first `len` slots were written before the release store of `len` this
        // load synchronized with, are never written again through `&self`, and stay committed
        // for the column's life; the base is not null and aligned.
        unsafe { slice::from_raw_parts(self.region.base().as_ptr().cast::<T>(), len) }
    }

    /// The element at `index`, once published.
    #[inline]
    pub fn get(&self, index: u32) -> Option<&T> {
        self.as_slice().get(index as usize)
    }

    pub fn iter(&self) -> slice::Iter<'_, T> {
        self.as_slice().iter()
    }

    /// The elements, to change in place: `&mut self` says no reader is left.
    pub fn as_mut_slice(&mut self) -> &mut [T] {
        let len = *self.len.get_mut();
        // SAFETY: as for `as_slice`, and `&mut self` makes the borrow unique.
        unsafe { slice::from_raw_parts_mut(self.region.base().as_ptr().cast::<T>(), len) }
    }
}

/// The one writer of an [`AppendVec`], from [`AppendVec::writer`] until dropped. It holds the
/// column's writer flag, so its appends need no atomic read-modify-write each.
pub struct Writer<'a, T> {
    column: &'a AppendVec<T>,
}

impl<T> Writer<'_, T> {
    /// Room for `count` more values past the published length, committed: its start.
    fn room(&self, count: usize) -> Result<usize, MemError> {
        let column = self.column;
        // Only the writer changes `len`, and the flag's acquire ordered this after the last
        // writer's store.
        let len = column.len.load(Ordering::Relaxed);
        let end = len.checked_add(count).filter(|&end| end <= column.max).ok_or_else(|| {
            MemError::Exhausted {
                tag: column.tag(),
                reserved: column.region.reserved(),
                wanted: len.saturating_add(count).saturating_mul(AppendVec::<T>::SIZE),
            }
        })?;
        column.region.ensure(end * AppendVec::<T>::SIZE)?;
        Ok(len)
    }

    /// Slot `index` of a room [`Writer::room`] gave.
    ///
    /// INVARIANT (no wrapped offset): every caller's `index` is in `[len, end]` of a room
    /// `room` gave, and `room` bounds `end <= max`, so `index <= max`; `max * SIZE` was checked
    /// not to overflow (`AppendVec::bytes`), so `index * SIZE` does not wrap, and `Region::at`
    /// holds it inside the reservation.
    fn slot(&self, index: usize) -> *mut T {
        debug_assert!(index <= self.column.max);
        self.column.region.at(index * AppendVec::<T>::SIZE).cast::<T>().as_ptr()
    }

    fn publish(&self, len: usize) {
        // Release: the values' bytes are seen by a reader that sees the length.
        self.column.len.store(len, Ordering::Release);
    }

    /// Appends `value` and returns its index, readable by every thread from now on; when the
    /// reservation is used up or memory cannot be had, the error names the column's tag and
    /// nothing is published.
    #[inline]
    pub fn append(&mut self, value: T) -> Result<u32, MemError> {
        let len = self.room(1)?;
        // SAFETY: the slot is committed (`room`), aligned (a page-aligned base plus a multiple
        // of `size_of::<T>()`), past the published length so no reader reaches it, and written
        // by this writer alone (the column's flag).
        unsafe { self.slot(len).write(value) };
        self.publish(len + 1);
        Ok(len as u32)
    }

    /// Appends every value of `values` with one copy and one publish; the first one's index.
    pub fn extend_from_slice(&mut self, values: &[T]) -> Result<u32, MemError>
    where
        T: Copy,
    {
        let len = self.room(values.len())?;
        // SAFETY: `[len, len + values.len())` is committed (`room`), aligned, past the published
        // length and this writer's alone, so it overlaps neither `values` (a live borrow, so not
        // unpublished room) nor anything a reader reaches.
        unsafe { std::ptr::copy_nonoverlapping(values.as_ptr(), self.slot(len), values.len()) };
        self.publish(len + values.len());
        Ok(len as u32)
    }

    /// Appends `make(0)`, `make(1)`, ... `make(count - 1)` with one publish; the first one's
    /// index. If `make` panics, none is published and those made so far are leaked (the next
    /// append writes over them).
    pub fn extend_with(
        &mut self,
        count: usize,
        mut make: impl FnMut(usize) -> T,
    ) -> Result<u32, MemError> {
        let len = self.room(count)?;
        for at in 0..count {
            // SAFETY: as in `append`, for each slot of the room `room` committed.
            unsafe { self.slot(len + at).write(make(at)) };
        }
        self.publish(len + count);
        Ok(len as u32)
    }
}

impl<T> Drop for Writer<'_, T> {
    fn drop(&mut self) {
        self.column.writing.store(false, Ordering::Release);
    }
}

impl<T> Drop for AppendVec<T> {
    fn drop(&mut self) {
        if mem::needs_drop::<T>() {
            let values: *mut [T] = self.as_mut_slice();
            // SAFETY: the published elements are initialized, owned by the column and dropped
            // once: it is going away.
            unsafe { values.drop_in_place() };
        }
    }
}

impl<T: fmt::Debug> fmt::Debug for AppendVec<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.as_slice()).finish()
    }
}
