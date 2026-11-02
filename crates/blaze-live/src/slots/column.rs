//! A column of rows that grows in place: address space for its most rows is reserved when it
//! is made, and committed [`STEP`] at a time as it grows, so growing never moves a row nor
//! calls the allocator. A column past its most rows is an error that names it, never a
//! fallback to the heap.
//!
//! The platform reserves and commits ([`os::Reserved`]: `mmap` with `PROT_NONE` then
//! `mprotect`, `VirtualAlloc` with `MEM_RESERVE` then `MEM_COMMIT`), and [`image::rows`] views
//! the committed bytes as rows; this module has no unsafe. Not blaze-mem's `RegionVec`: this
//! crate links into the program with no dependencies, and reserves only what a long session
//! needs (docs/v2/plan/lanes/link/speed-of-light.md, "The live runtime").

use std::{
    marker::PhantomData,
    ops::{Deref, DerefMut, Range},
};

use super::{
    image::{self, Plain},
    os,
};

/// How much a column commits at once: 256 KiB, so a column of 8-byte rows calls `mprotect`
/// once per 32,768 rows. A multiple of [`os::GRAIN`].
pub(super) const STEP: usize = 256 << 10;

/// The smallest page of the platforms: [`Column::ahead`] writes once per this many bytes.
const FAULT: usize = 4096;

const _: () = assert!(STEP.is_multiple_of(os::GRAIN));
const _: () = assert!(size_of::<Column<u64>>() == 32 + size_of::<os::Reserved>());

/// At most `limit` rows of `T`, in address space reserved for all of them. 56 bytes (the
/// reservation is 24); a row is `size_of::<T>()` bytes, and an index is a compare against
/// the committed bytes, a bounds check and one load (a loop takes the slice once).
pub(super) struct Column<T: Plain> {
    /// Which column, for the error when it is full.
    name: &'static str,
    limit: usize,
    len: usize,
    memory: os::Reserved,
    rows: PhantomData<T>,
}

impl<T: Plain> Column<T> {
    /// A column for at most `limit` rows, with none committed.
    pub(super) fn new(name: &'static str, limit: usize) -> Result<Column<T>, String> {
        let bytes = limit.checked_mul(size_of::<T>());
        let Some(bytes) = bytes.and_then(|it| it.checked_next_multiple_of(STEP)) else {
            // alloc: error path, once at start
            return Err(format!("{name}: {limit} rows are more than the address space"));
        };
        let memory = os::Reserved::new(bytes.max(STEP))
            // alloc: error path, once at start
            .map_err(|it| format!("reserving {name} for {limit} rows: {it}"))?;
        Ok(Column { name, limit, len: 0, memory, rows: PhantomData })
    }

    /// Makes room for `more` rows past the last: an error naming the column if that is past
    /// its limit, else the pages committed for them.
    pub(super) fn room(&mut self, more: usize) -> Result<(), String> {
        let rows = self.len.checked_add(more).filter(|it| *it <= self.limit);
        let Some(rows) = rows else {
            // alloc: error path
            return Err(format!("{} is full: it holds {} rows at most", self.name, self.limit));
        };
        let bytes = rows * size_of::<T>();
        if bytes <= self.memory.committed() {
            return Ok(());
        }
        let upto = bytes.next_multiple_of(STEP).min(self.memory.len());
        // alloc: error path
        self.memory.commit(upto).map_err(|it| format!("growing {}: {it}", self.name))
    }

    pub(super) fn push(&mut self, value: T) -> Result<(), String> {
        self.room(1)?;
        let at = self.len;
        image::rows_mut(&mut self.memory, at + 1)[at] = value;
        self.len = at + 1;
        Ok(())
    }

    pub(super) fn extend_from_slice(&mut self, values: &[T]) -> Result<(), String> {
        self.room(values.len())?;
        let at = self.len;
        image::rows_mut(&mut self.memory, at + values.len())[at..].copy_from_slice(values);
        self.len = at + values.len();
        Ok(())
    }

    /// Commits the pages the next `more` rows go on (up to the limit) and writes each page
    /// once, so the pushes that fill them neither call `mprotect` nor fault: for after a
    /// patch's switch, off its latency path. What it can't commit is left to
    /// [`Column::room`], which commits it when a row needs it, or refuses the patch saying why.
    pub(super) fn ahead(&mut self, more: usize) {
        let rows = self.len.saturating_add(more).min(self.limit);
        if self.room(rows - self.len).is_err() {
            return;
        }
        let per_page = (FAULT / size_of::<T>()).max(1);
        let pages = image::rows_mut::<T>(&mut self.memory, rows);
        let mut at = self.len;
        while at < rows {
            // Past the last row: a row's bytes stored back, which the compiler can't drop.
            pages[at] = std::hint::black_box(pages[at]);
            at = (at / per_page + 1) * per_page;
        }
    }

    /// Keeps the first `len` rows; what was committed stays committed.
    pub(super) fn truncate(&mut self, len: usize) {
        self.len = self.len.min(len);
    }

    /// Puts `value` in place of the rows `range`, moving the rows after it in place, as
    /// `Vec::splice(range, [value])` does: an insertion when `range` is empty.
    pub(super) fn splice(&mut self, range: Range<usize>, value: T) -> Result<(), String> {
        let (start, end, len) = (range.start, range.end, self.len);
        assert!(start <= end && end <= len, "rows {start}..{end} of {len}");
        if start == end {
            self.room(1)?;
            let rows = image::rows_mut(&mut self.memory, len + 1);
            rows.copy_within(start..len, start + 1);
            rows[start] = value;
            self.len = len + 1;
        } else {
            let rows = image::rows_mut(&mut self.memory, len);
            rows[start] = value;
            rows.copy_within(end..len, start + 1);
            self.truncate(len - (end - start - 1));
        }
        Ok(())
    }

    /// Where the reservation is, and how long.
    #[cfg(test)]
    pub(super) fn reserved(&self) -> (u64, usize) {
        (self.memory.base(), self.memory.len())
    }

    /// How many bytes are committed.
    #[cfg(test)]
    pub(super) fn committed(&self) -> usize {
        self.memory.committed()
    }
}

impl<T: Plain> Deref for Column<T> {
    type Target = [T];

    fn deref(&self) -> &[T] {
        image::rows(&self.memory, self.len)
    }
}

impl<T: Plain> DerefMut for Column<T> {
    fn deref_mut(&mut self) -> &mut [T] {
        image::rows_mut(&mut self.memory, self.len)
    }
}

#[cfg(test)]
mod tests {
    use blaze_ids::rng::SplitMix64;

    use super::*;

    /// A column against a `Vec` doing the same, over seeded pushes, splices (insertions and
    /// merges of runs) and truncations, past several commit steps and up to its limit: the
    /// same rows after each, the commit always whole steps covering the rows and never a step
    /// more, and a push past the limit refused with nothing changed.
    fn model<T: Plain + PartialEq + std::fmt::Debug>(seed: u64, of: impl Fn(u64) -> T) {
        let rows_per_step = STEP / size_of::<T>();
        let limit = 5 * rows_per_step + 77;
        let mut column = Column::<T>::new("the model column", limit).expect("reserved");
        let mut model: Vec<T> = Vec::new();
        let mut rng = SplitMix64::new(seed);
        let mut full = 0;
        for step in 0..40_000 {
            let value = of(rng.next_u64());
            match rng.below(16) {
                0 => {
                    let start = rng.index(model.len() + 1);
                    let end = start + rng.index((model.len() - start).min(8) + 1);
                    match column.splice(start..end, value) {
                        Ok(()) => drop(model.splice(start..end, [value])),
                        Err(why) => {
                            assert!(start == end && model.len() == limit, "{why}");
                            full += 1;
                        }
                    }
                }
                1 if rng.one_in(64) => {
                    let len = model.len() - rng.index(model.len().min(rows_per_step / 8) + 1);
                    column.truncate(len);
                    model.truncate(len);
                }
                2 => {
                    let count = rng.index(rows_per_step / 64);
                    let values: Vec<T> = (0..count).map(|_| of(rng.next_u64())).collect();
                    match column.extend_from_slice(&values) {
                        Ok(()) => model.extend_from_slice(&values),
                        Err(why) => {
                            assert!(model.len() + values.len() > limit, "{why}");
                            full += 1;
                        }
                    }
                }
                _ => match column.push(value) {
                    Ok(()) => model.push(value),
                    Err(why) => {
                        assert_eq!(model.len(), limit, "{why}");
                        assert!(
                            why.contains("the model column") && why.contains(&limit.to_string())
                        );
                        full += 1;
                    }
                },
            }
            assert_eq!(column.len(), model.len());
            if step % 97 == 0 {
                assert_eq!(&column[..], &model[..], "at step {step}");
            }
            let committed = column.committed();
            assert!(committed.is_multiple_of(STEP));
            assert!(committed >= model.len() * size_of::<T>());
        }
        assert_eq!(&column[..], &model[..]);
        assert!(full > 0, "the column reached its limit");
        assert!(column.committed() >= 5 * STEP, "commits crossed several steps");
    }

    #[test]
    fn a_column_holds_what_a_vec_would() {
        model(0x5eed_0001, |it| it);
        model(0x5eed_0002, |it| it as u32);
        model(0x5eed_0003, |it| it as u8);
    }

    /// Commits grow a step at a time, only as rows need them.
    #[test]
    fn a_column_commits_a_step_at_a_time() {
        let mut column = Column::<u64>::new("a column", 3 * STEP / 8).expect("reserved");
        assert_eq!(column.committed(), 0);
        column.push(1).expect("room");
        assert_eq!(column.committed(), STEP);
        column.room(STEP / 8 - 1).expect("room");
        assert_eq!(column.committed(), STEP, "the rows fit the first step");
        column.room(STEP / 8).expect("room");
        assert_eq!(column.committed(), 2 * STEP);
        column.room(3 * STEP / 8 - 1).expect("room");
        assert_eq!(column.committed(), 3 * STEP);
        let full = column.room(3 * STEP / 8).expect_err("past the limit");
        assert_eq!(full, format!("a column is full: it holds {} rows at most", 3 * STEP / 8));
    }

    /// Readying rows ahead commits them and changes no row; past the limit it readies up to it.
    #[test]
    fn rows_readied_ahead_are_committed_and_unchanged() {
        let mut column = Column::<u64>::new("a column", STEP / 8 + 10).expect("reserved");
        column.extend_from_slice(&[1, 2, 3]).expect("room");
        column.truncate(1);
        column.ahead(STEP / 8);
        assert_eq!(column.committed(), 2 * STEP);
        assert_eq!(&column[..], &[1]);
        column.ahead(usize::MAX);
        column.push(4).expect("room");
        assert_eq!(&column[..], &[1, 4]);
    }

    /// Rows stay where they are as the column grows: growing never moves data.
    #[test]
    fn rows_never_move() {
        let mut column = Column::<u32>::new("a column", 1 << 20).expect("reserved");
        column.push(5).expect("room");
        let first = column.as_ptr();
        for it in 0..200_000 {
            column.push(it).expect("room");
        }
        assert_eq!(column.as_ptr(), first);
        assert_eq!(column[0], 5);
    }
}
