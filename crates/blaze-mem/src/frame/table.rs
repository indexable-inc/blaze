//! [`FrameTable`]: ONE growing table of a frame, outside the frame's bump, for a table that
//! grows far while other allocations of the same frame interleave (so it is never the frame's
//! last block, and a `Vec<T, &Frame>` would leave every outgrown copy behind:
//! [`super::grow`]).
//!
//! Its storage is its own reservation of address space ([`RegionVec`], over
//! [`crate::Region`]: the crate's one mapping wrapper), committed as it grows, so a push never
//! copies, never leaves a hole in the frame, and an index is a stable `u32` for the table's
//! life. Reserved address space is not memory: the footprint is what was pushed, rounded up by
//! the region's geometric commit (at most a quarter over). Nothing admits or throttles by it.
//!
//! Lifetime: the table borrows its frame, so it is dropped (its pages and its reservation given
//! back to the kernel) before the frame can be reset, rolled back or dropped, each of which
//! takes `&mut Frame` or the frame itself. No value in it outlives the frame's task.
//!
//! Every growing method is fallible and typed ([`TableRefused`], which names the frame, the
//! bytes asked, the table's peak and its reserve: the fields of parallel.md's `FrameFull` that
//! this layer knows; the job seam adds the task). There is no `push` that panics.

use core::{fmt, mem, ops::Deref};

use super::Frame;
use crate::{MemError, RegionVec, Tag};

/// Why a [`FrameTable`] did not grow.
#[derive(Debug)]
pub enum TableRefused {
    /// The table holds as many rows as it was made for: its reservation never moves.
    Full {
        /// The frame's tag: the subsystem whose table it is.
        tag: Tag,
        /// The id of the frame the table belongs to.
        frame: u32,
        /// Bytes the table would hold had the growth been granted.
        asked: usize,
        /// The most bytes its rows took so far.
        peak: usize,
        /// Bytes of address space it reserved when it was made.
        reserve: usize,
    },
    /// The kernel refused the memory (or the address space) for it.
    Memory {
        /// The id of the frame the table belongs to.
        frame: u32,
        /// Bytes the table would hold had the growth been granted.
        asked: usize,
        /// The most bytes its rows took so far.
        peak: usize,
        /// What the kernel refused, naming the tag and the bytes.
        error: MemError,
    },
}

impl fmt::Display for TableRefused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TableRefused::Full { tag, frame, asked, peak, reserve } => write!(
                f,
                "{tag}: a table of frame {frame} is full: {asked} bytes asked, {peak} at its \
                 peak, {reserve} reserved; a table never moves, so make it for more rows"
            ),
            TableRefused::Memory { frame, asked, peak, error } => write!(
                f,
                "a table of frame {frame} could not grow to {asked} bytes ({peak} at its peak): \
                 {error}"
            ),
        }
    }
}

impl core::error::Error for TableRefused {
    // dyn: `Error::source`'s own signature, on an error path.
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            TableRefused::Full { .. } => None,
            TableRefused::Memory { error, .. } => Some(error),
        }
    }
}

/// A growing table of one frame, in its own reservation: see the module's text.
pub struct FrameTable<'f, T> {
    rows: RegionVec<T>,
    frame: &'f Frame,
    /// The most rows it may ever hold: what [`Frame::table`] was asked for.
    most: usize,
    /// The most rows it held at once.
    peak: usize,
}

impl Frame {
    /// A table of at most `most` rows of this frame, outside its bump: address space for
    /// `most` rows is reserved now and no memory is taken until rows are pushed. `most` is at
    /// most `u32::MAX`, so every index is a `u32`.
    ///
    /// # Errors
    ///
    /// [`MemError::TooLarge`] when `most` passes `u32::MAX` or its bytes overflow;
    /// [`MemError::Reserve`] when the kernel refuses the address space.
    pub fn table<T>(&self, most: usize) -> Result<FrameTable<'_, T>, MemError> {
        let tag = self.tag();
        if u32::try_from(most).is_err() {
            return Err(MemError::TooLarge { tag, what: "a frame table's rows", wanted: most });
        }
        Ok(FrameTable { rows: RegionVec::new(most, tag)?, frame: self, most, peak: 0 })
    }
}

impl<'f, T> FrameTable<'f, T> {
    /// The frame it belongs to.
    pub fn frame(&self) -> &'f Frame {
        self.frame
    }

    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// The most rows it may ever hold.
    pub fn most(&self) -> usize {
        self.most
    }

    /// Bytes of memory it holds now: its committed rows, used or not.
    pub fn committed(&self) -> usize {
        self.rows.capacity() * mem::size_of::<T>()
    }

    /// The most bytes its rows took so far.
    pub fn peak(&self) -> usize {
        self.peak * mem::size_of::<T>()
    }

    /// Bytes of address space it reserved.
    pub fn reserve(&self) -> usize {
        self.most * mem::size_of::<T>()
    }

    /// The refusal of a growth to `rows` rows that `error` (if any) stopped.
    #[cold]
    fn refused(&self, rows: usize, error: Option<MemError>) -> TableRefused {
        let asked = rows.saturating_mul(mem::size_of::<T>());
        match error {
            None | Some(MemError::Exhausted { .. }) => TableRefused::Full {
                tag: self.rows.tag(),
                frame: self.frame.id,
                asked,
                peak: self.peak(),
                reserve: self.reserve(),
            },
            Some(error) => {
                TableRefused::Memory { frame: self.frame.id, asked, peak: self.peak(), error }
            }
        }
    }

    /// Appends `value` and returns its index, which names it for the table's life. Nothing is
    /// copied and no row moves.
    ///
    /// # Errors
    ///
    /// [`TableRefused`], `value` dropped: the table is full, or the kernel refused the memory.
    #[inline]
    pub fn try_push(&mut self, value: T) -> Result<u32, TableRefused> {
        let index = self.rows.len();
        if index >= self.most {
            return Err(self.refused(index.saturating_add(1), None));
        }
        match self.rows.try_push(value) {
            Ok(index) => {
                self.peak = self.peak.max(index + 1);
                // `index < most <= u32::MAX` (checked by `Frame::table`): the cast keeps it.
                Ok(index as u32)
            }
            Err(error) => Err(self.refused(index + 1, Some(error))),
        }
    }

    /// Room for `additional` more rows committed now, so the pushes that follow make no system
    /// call.
    ///
    /// # Errors
    ///
    /// [`TableRefused`]: past the rows it was made for, or the kernel refused the memory.
    pub fn try_reserve(&mut self, additional: usize) -> Result<(), TableRefused> {
        let rows = self.rows.len().saturating_add(additional);
        if rows > self.most {
            return Err(self.refused(rows, None));
        }
        self.rows.reserve(additional).map_err(|error| self.refused(rows, Some(error)))
    }

    /// Drops the rows from `len` on; their memory stays committed for the pushes that follow.
    pub fn truncate(&mut self, len: usize) {
        self.rows.truncate(len);
    }

    /// Drops every row; the memory stays committed.
    pub fn clear(&mut self) {
        self.rows.clear();
    }

    pub fn as_slice(&self) -> &[T] {
        self.rows.as_slice()
    }

    pub fn as_mut_slice(&mut self) -> &mut [T] {
        self.rows.as_mut_slice()
    }

    /// The row `index` names.
    pub fn get(&self, index: u32) -> Option<&T> {
        self.rows.get(index as usize)
    }

    /// The row `index` names, to change.
    pub fn get_mut(&mut self, index: u32) -> Option<&mut T> {
        self.rows.get_mut(index as usize)
    }
}

impl<T: Copy> FrameTable<'_, T> {
    /// Appends `values`, all or none.
    ///
    /// # Errors
    ///
    /// [`TableRefused`], nothing appended.
    pub fn try_extend_from_slice(&mut self, values: &[T]) -> Result<(), TableRefused> {
        let rows = self.rows.len().saturating_add(values.len());
        if rows > self.most {
            return Err(self.refused(rows, None));
        }
        match self.rows.try_extend_from_slice(values) {
            Ok(()) => {
                self.peak = self.peak.max(rows);
                Ok(())
            }
            Err(error) => Err(self.refused(rows, Some(error))),
        }
    }
}

impl<T> Deref for FrameTable<'_, T> {
    type Target = [T];

    fn deref(&self) -> &[T] {
        self.rows.as_slice()
    }
}

impl<T: fmt::Debug> fmt::Debug for FrameTable<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.rows.as_slice()).finish()
    }
}
