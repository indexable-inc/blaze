//! What the store knows of each record beyond its bytes, as bits: one bit a record and flag,
//! over the record's row. A row is its slot in the index table, or its row in a delta table
//! (`table.rs`, which keeps its rows' bits beside its slots).
//!
//! An index table's flags are blocked by row: each 64 rows have one 64-byte line of [`BLOCK`]
//! words, a word a flag, so every flag of a record is on one cache line. A hit that checks
//! [`Flag::Verified`] and sets [`Flag::Read`] touches that line alone, where columns a flag
//! apart cost it a line each. A sweep still tests 64 records a word.
//!
//! A read sets a flag with no lock (docs/v2/plan/design/concurrency.md "Reads"): each word is
//! atomic, and a flag already set is only loaded, so reading a record again writes no shared
//! cache line. The words sit on a region tagged `Tag::Store` (`words.rs`), committed whole when
//! the flags are made.

use std::fmt;

use crate::{
    sync::atomic::{AtomicU64, Ordering},
    words::Words,
};

/// A record's row in its tier's flags.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(crate) enum Row {
    /// Its slot in the mapped index table.
    Slot(u64),
    /// Its row in the delta table. A settle, the one user of rows, does not run while an index
    /// swap leaves a delta before it.
    Delta(u64),
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum Flag {
    /// Its bytes were hashed and found to be what was recorded. A mapping never changes the
    /// bytes at a place, so once is enough. A delta record keeps this on its spot instead,
    /// since its place changes when it moves from a segment to a pack.
    Verified,
    /// Read since the last rotation.
    Read,
    /// Read between the two rotations before.
    ReadBefore,
    /// Compressing it saved too little to keep: not tried again.
    Tried,
    /// Made (put, or moved by a settle) since the last rotation.
    New,
    /// Made between the two rotations before.
    NewBefore,
}

pub(crate) const FLAGS: usize = 6;

/// Words of one block of 64 rows: a word for each flag, padded to a 64-byte line.
const BLOCK: usize = 8;

const _: () = assert!(FLAGS <= BLOCK && BLOCK * size_of::<AtomicU64>() == 64);

/// A flag is a hint about bytes that never change at a place, so no ordering is needed.
const RELAXED: Ordering = Ordering::Relaxed;

/// The flags of a fixed number of rows (an index table's slots): 1 byte a row, 64 rows a line.
pub(crate) struct FlagColumns {
    /// `blocks` blocks of [`BLOCK`] words, one stretch.
    bits: Words<AtomicU64>,
    blocks: usize,
}

impl fmt::Debug for FlagColumns {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FlagColumns").field("rows", &(self.blocks * 64)).finish_non_exhaustive()
    }
}

impl FlagColumns {
    pub(crate) fn new(rows: u64) -> FlagColumns {
        let blocks = rows.div_ceil(64) as usize;
        let bits = Words::new(BLOCK * blocks);
        bits.make(0, 0, BLOCK * blocks);
        FlagColumns { bits, blocks }
    }

    fn words(&self) -> &[AtomicU64] {
        self.bits.get(0, 0, BLOCK * self.blocks)
    }

    /// The word of `flag` for `row` and the row's bit in it; `None` past the table's rows.
    #[inline]
    fn word(&self, row: u64, flag: Flag) -> Option<(&AtomicU64, u64)> {
        let block = usize::try_from(row / 64).ok().filter(|&block| block < self.blocks)?;
        Some((&self.words()[block * BLOCK + flag as usize], 1 << (row % 64)))
    }

    pub(crate) fn get(&self, row: u64, flag: Flag) -> bool {
        self.word(row, flag).is_some_and(|(word, bit)| word.load(RELAXED) & bit != 0)
    }

    /// Sets `flag` of `row`; a row past the table's is none of its records and is left.
    pub(crate) fn set(&self, row: u64, flag: Flag) {
        if let Some((word, bit)) = self.word(row, flag)
            && word.load(RELAXED) & bit == 0
        {
            word.fetch_or(bit, RELAXED);
        }
    }

    /// A new period of reading: what was read becomes read before, what was made made before.
    /// Only the store calls it; a flag set meanwhile lands in one period or the other.
    pub(crate) fn rotate(&self) {
        for block in self.words().chunks_exact(BLOCK) {
            for (from, to) in [(Flag::Read, Flag::ReadBefore), (Flag::New, Flag::NewBefore)] {
                block[to as usize].store(block[from as usize].swap(0, RELAXED), RELAXED);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_flag_is_set_per_row_and_reading_rotates() {
        let bits = FlagColumns::new(701);
        bits.set(3, Flag::Read);
        bits.set(700, Flag::Verified);
        bits.set(5, Flag::New);
        assert!(bits.get(3, Flag::Read));
        assert!(!bits.get(3, Flag::Verified));
        assert!(bits.get(700, Flag::Verified));
        assert!(!bits.get(1 << 40, Flag::Read), "past the end is clear");
        bits.set(1 << 40, Flag::Read);
        bits.rotate();
        assert!(!bits.get(3, Flag::Read));
        assert!(bits.get(3, Flag::ReadBefore));
        assert!(!bits.get(5, Flag::New) && bits.get(5, Flag::NewBefore));
        assert!(bits.get(700, Flag::Verified), "rotating keeps the rest");
        bits.rotate();
        assert!(!bits.get(3, Flag::ReadBefore));
    }

    /// Every flag of a row is on the row's one 64-byte line.
    #[test]
    #[cfg(not(blaze_check))]
    fn a_rows_flags_share_one_line() {
        let bits = FlagColumns::new(3 << 10);
        let flags = [Flag::Verified, Flag::Read, Flag::ReadBefore, Flag::Tried, Flag::New, Flag::NewBefore];
        for row in [0, 1, 63, 64, 1000, (3 << 10) - 1] {
            let lines: Vec<usize> = flags
                .iter()
                .map(|&flag| std::ptr::from_ref(bits.word(row, flag).unwrap().0).addr() / 64)
                .collect();
            assert!(lines.windows(2).all(|pair| pair[0] == pair[1]), "row {row}: lines {lines:?}");
        }
    }

    /// Flags of a table big enough to span many commit steps answer as a `Vec<bool>` a flag
    /// does, through sets, rotations and rows past the end.
    #[test]
    #[cfg(not(blaze_check))]
    fn the_columns_answer_as_vecs_of_bools_do() {
        let rows = 3 << 20;
        let bits = FlagColumns::new(rows);
        let flags = [Flag::Verified, Flag::Read, Flag::ReadBefore, Flag::Tried, Flag::New, Flag::NewBefore];
        let mut model = vec![vec![false; rows as usize]; FLAGS];
        let mut rng = blaze_ids::rng::SplitMix64::new(5);
        for step in 0..100_000u64 {
            let row = rng.next_u64() % (rows + 64);
            let flag = flags[(rng.next_u64() % FLAGS as u64) as usize];
            if step % 9 == 0 {
                bits.set(row, flag);
                if row < rows {
                    model[flag as usize][row as usize] = true;
                }
            } else if step % 10_007 == 0 {
                bits.rotate();
                for (from, to) in [(Flag::Read, Flag::ReadBefore), (Flag::New, Flag::NewBefore)] {
                    model[to as usize] = std::mem::replace(&mut model[from as usize], vec![false; rows as usize]);
                }
            } else {
                let want = row < rows && model[flag as usize][row as usize];
                assert_eq!(bits.get(row, flag), want, "step {step}: row {row} {flag:?}");
            }
        }
    }

    #[test]
    #[cfg(not(blaze_check))]
    fn flags_set_from_many_threads_at_once_all_land() {
        use std::{sync::Arc, thread};

        let bits = Arc::new(FlagColumns::new(64 * 64));
        let threads: Vec<_> = (0..8u64)
            .map(|t| {
                let bits = bits.clone();
                thread::spawn(move || {
                    for row in (t..64 * 64).step_by(8) {
                        bits.set(row, Flag::Read);
                    }
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        assert!((0..64 * 64).all(|row| bits.get(row, Flag::Read)));
    }
}
