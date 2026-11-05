//! [`Column<T>`]: an append-only column any number of threads push to at once, over a
//! reserved region (docs/v2/plan/design/concurrency.md "Tables are append-only", "Values are
//! immutable once published"). The column for interners and shared tables: a push claims an
//! index with one `fetch_add`, writes the value in place and publishes it with one release
//! store of its ready byte; a read is a bounds check against the committed length and one
//! acquire load of the ready byte, and writes nothing.
//!
//! It never moves, so `&T` from [`Column::get`] lives as long as the column, and an index is a
//! stable `u32` ID. Values are never changed or removed once published, but through
//! [`Column::get_mut`] by the column's only user.
//!
//! Cost per push: one `fetch_add` (once a block through [`Column::claim`]), a store of the
//! value and one release store of its byte, no read-modify-write: a byte per value is what
//! lets two pushers publish neighbours without sharing a word, for 1 byte of reserved address
//! space per value. A commit system call per quarter of growth. Per read: a bounds check and
//! one acquire load of the ready byte.

use std::{
    fmt,
    marker::PhantomData,
    mem,
    sync::atomic::{AtomicU8, AtomicU64, AtomicUsize, Ordering},
};

use crate::{MemError, Region, Tag};

/// Column numbers, so a [`Block`] fills only the column that claimed it; never reused.
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

pub struct Column<T> {
    values: Region,
    ready: Region,
    claimed: AtomicUsize,
    max: usize,
    id: u64,
    _owns: PhantomData<T>,
}

// SAFETY: a value is written once by the pusher that claimed its index, before its ready byte
// is set with release, and read only after the byte is seen with acquire; `T: Send` lets a value
// made on one thread be dropped on another, `T: Sync` lets every thread read it.
unsafe impl<T: Send + Sync> Sync for Column<T> {}
// SAFETY: the column owns its values as a `Vec` does.
unsafe impl<T: Send> Send for Column<T> {}

/// A ready byte's value once its value is published; committed memory starts zeroed.
const READY: u8 = 1;

/// Indices of one [`Column`] claimed at once by [`Column::claim`], published in order by
/// [`Column::fill`]. Not `Clone`: each index is written by the one holder of its block.
#[derive(Debug)]
pub struct Block {
    column: u64,
    next: u32,
    end: u32,
}

impl Block {
    /// A block with no index left: what a writer holds before its first claim.
    pub const EMPTY: Block = Block { column: 0, next: 0, end: 0 };

    pub fn is_spent(&self) -> bool {
        self.next == self.end
    }

    /// Indices left to fill.
    pub fn remaining(&self) -> u32 {
        self.end - self.next
    }
}

impl<T> Column<T> {
    const SIZE: usize = {
        assert!(mem::size_of::<T>() != 0, "a column of a zero-sized type holds nothing");
        assert!(mem::align_of::<T>() <= 4096, "a column's values align to at most a page");
        mem::size_of::<T>()
    };

    /// A column of at most `max` values for `tag` (at most `u32::MAX`), on huge pages where
    /// the kernel offers them.
    pub fn new(max: usize, tag: Tag) -> Result<Column<T>, MemError> {
        let max = max.min(u32::MAX as usize);
        let bytes = max.checked_mul(Self::SIZE).ok_or(MemError::TooLarge {
            tag,
            what: "a column's length",
            wanted: max,
        })?;
        Ok(Column {
            values: Region::reserve_huge(bytes, tag)?,
            ready: Region::reserve(max.max(1), tag)?,
            claimed: AtomicUsize::new(0),
            max,
            id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
            _owns: PhantomData,
        })
    }

    pub fn max_len(&self) -> usize {
        self.max
    }

    /// Indices claimed so far; a pusher between its claim and its publish has one of them, not
    /// yet readable. Indices of claimed blocks count whether filled or not.
    pub fn len(&self) -> usize {
        self.claimed.load(Ordering::Relaxed).min(self.max)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn ready(&self, index: usize) -> Option<&AtomicU8> {
        if index >= self.ready.committed() {
            return None;
        }
        // SAFETY: the byte is committed (checked just now), and committed memory is zeroed by
        // the kernel, a valid `AtomicU8`; it lives as long as `self`.
        Some(unsafe { self.ready.at(index).cast::<AtomicU8>().as_ref() })
    }

    /// Appends `value`; returns its index, readable by every thread from now on.
    pub fn push(&self, value: T) -> Result<u32, MemError> {
        let index = self.claimed.fetch_add(1, Ordering::Relaxed);
        if index >= self.max {
            self.claimed.fetch_min(self.max, Ordering::Relaxed);
            return Err(self.exhausted(index + 1));
        }
        self.publish(index, value)
    }

    /// Claims `count` consecutive indices with one `fetch_add`, for the caller to publish one at
    /// a time with [`Column::fill`]: writers that take indices in blocks share the counter once a
    /// block, not once a value. An index of a block never filled stays unreadable.
    pub fn claim(&self, count: u32) -> Result<Block, MemError> {
        let count = count as usize;
        let start = self.claimed.fetch_add(count, Ordering::Relaxed);
        let Some(end) = start.checked_add(count).filter(|&end| end <= self.max) else {
            self.claimed.fetch_min(self.max, Ordering::Relaxed);
            return Err(self.exhausted(start.saturating_add(count)));
        };
        Ok(Block { column: self.id, next: start as u32, end: end as u32 })
    }

    /// Publishes `value` at `block`'s next index, readable by every thread from now on, and
    /// returns that index. Stops when `block` is spent or another column's.
    #[track_caller]
    pub fn fill(&self, block: &mut Block, value: T) -> Result<u32, MemError> {
        assert!(block.column == self.id, "a block fills only the column that claimed it");
        assert!(!block.is_spent(), "a spent block fills nothing");
        let index = block.next as usize;
        block.next += 1;
        self.publish(index, value)
    }

    fn exhausted(&self, wanted: usize) -> MemError {
        MemError::Exhausted {
            tag: self.values.tag(),
            reserved: self.values.reserved(),
            wanted: wanted.saturating_mul(Self::SIZE),
        }
    }

    /// Writes `value` at `index`, claimed by the caller alone, and sets its ready byte.
    fn publish(&self, index: usize, value: T) -> Result<u32, MemError> {
        self.values.ensure((index + 1) * Self::SIZE)?;
        self.ready.ensure(index + 1)?;
        let slot = self.values.at(index * Self::SIZE).cast::<T>();
        // SAFETY: the slot is committed, aligned, and claimed by this thread alone (`push`'s
        // `fetch_add`, or the block of `fill`, which no other holder has: `Block` is not
        // `Clone` and names its column); no reader reaches it before its ready byte is set below.
        unsafe { slot.write(value) };
        let Some(ready) = self.ready(index) else {
            unreachable!("the ready byte was committed above")
        };
        // Release: the value's bytes are seen by a reader that sees the byte. The byte is this
        // pusher's alone, so a plain store publishes it.
        ready.store(READY, Ordering::Release);
        Ok(index as u32)
    }

    /// The value at `index` once it is published.
    #[inline]
    pub fn get(&self, index: u32) -> Option<&T> {
        let index = index as usize;
        if self.ready(index)?.load(Ordering::Acquire) != READY {
            return None;
        }
        // SAFETY: the ready byte is set, so the value was written whole before it (release,
        // acquire above) and is never written again; it lives as long as `self`.
        Some(unsafe { self.values.at(index * Self::SIZE).cast::<T>().as_ref() })
    }

    /// The value at `index` once it is published, for the column's only user.
    #[inline]
    pub fn get_mut(&mut self, index: u32) -> Option<&mut T> {
        let index = index as usize;
        if self.ready(index)?.load(Ordering::Relaxed) != READY {
            return None;
        }
        // SAFETY: the ready byte is set, so the value was written whole; `&mut self` excludes
        // every other reader and writer, and the value lives as long as `self`.
        Some(unsafe { self.values.at(index * Self::SIZE).cast::<T>().as_mut() })
    }

    /// Every published value with its index, in index order.
    pub fn iter(&self) -> impl Iterator<Item = (u32, &T)> {
        (0..self.len() as u32).filter_map(|index| Some((index, self.get(index)?)))
    }
}

impl<T> Drop for Column<T> {
    fn drop(&mut self) {
        if !mem::needs_drop::<T>() {
            return;
        }
        for index in 0..self.len() {
            if self.get(index as u32).is_some() {
                let slot = self.values.at(index * Self::SIZE).cast::<T>();
                // SAFETY: `&mut self`: no reader is left, and a published value is dropped once.
                unsafe { slot.drop_in_place() };
            }
        }
    }
}

impl<T: fmt::Debug> fmt::Debug for Column<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_map().entries(self.iter()).finish()
    }
}
