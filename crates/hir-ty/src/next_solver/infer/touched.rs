//! The log of the rows a union-find table wrote, for readers that keep work waiting on a
//! variable and wake it when the variable's row changes (`fulfill::watch`).
//!
//! Every write of a table goes through `UnionFind::set`, which appends the row's index. A
//! snapshot marks the log's length and a rollback truncates the log to it, as the rollback puts
//! back every row written since. Outside every snapshot (on a write, or when the outermost
//! snapshot closes) the log is cleared once it holds [`CAP`] rows, and its epoch moves.
//!
//! A reader holds a [`TouchedAt`]: where it stopped reading. It may go on from there while the
//! epoch is the same, the log is at least as long and the row before the cursor is the one it
//! read (rows carry a sequence number that never repeats within an epoch). Otherwise rows it
//! read were rolled back or cleared and the reader must treat every variable as written.

use super::InferCtxtInner;

/// Rows kept outside snapshots before the log is cleared: 8 KiB, so a context that writes
/// often stays in L1 and readers that fall this far behind rescan, which costs what one round
/// of rustc's loop costs.
const CAP: usize = 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TouchedRow {
    pub(crate) index: u32,
    seq: u32,
}

const _: () = assert!(size_of::<TouchedRow>() == 8);

/// Where a reader stopped reading a log.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TouchedAt {
    epoch: u64,
    len: u32,
    /// The sequence number of the row before `len`, or zero at the log's start.
    last: u32,
}

#[derive(Debug, Default)]
pub(crate) struct Touched {
    rows: Vec<TouchedRow>,
    /// The last sequence number given out; zero before the first row of an epoch.
    seq: u32,
    epoch: u64,
    /// Snapshots open on the table.
    open: u32,
}

impl Clone for Touched {
    fn clone(&self) -> Self {
        Touched {
            // clone: only `InferCtxt::fork` copies a table.
            rows: self.rows.clone(),
            seq: self.seq,
            epoch: self.epoch,
            open: self.open,
        }
    }

    /// Into `self`'s log, keeping its capacity: a warm fork copies without allocating.
    fn clone_from(&mut self, source: &Self) {
        self.rows.clone_from(&source.rows);
        self.seq = source.seq;
        self.epoch = source.epoch;
        self.open = source.open;
    }
}

impl Touched {
    /// Empties the log for another context. The epoch moves, so no cursor into this one reads
    /// the next.
    pub(crate) fn empty(&mut self) {
        self.rows.clear();
        self.open = 0;
        self.epoch += 1;
    }

    /// Bytes the log holds room for.
    pub(crate) fn held(&self) -> usize {
        self.rows.capacity() * size_of::<TouchedRow>()
    }

    #[inline(always)]
    pub(crate) fn push(&mut self, index: u32) {
        self.trim();
        self.seq = match self.seq.checked_add(1) {
            Some(seq) => seq,
            None => {
                self.epoch += 1;
                1
            }
        };
        // alloc: grows to its high-water mark once per context, at most `CAP` rows outside
        // snapshots.
        self.rows.push(TouchedRow { index, seq: self.seq });
    }

    pub(crate) fn mark(&mut self) -> u32 {
        self.open += 1;
        self.rows.len() as u32
    }

    pub(crate) fn rollback_to(&mut self, mark: u32) {
        self.open -= 1;
        self.rows.truncate(mark as usize);
        self.trim();
    }

    pub(crate) fn commit(&mut self) {
        self.open -= 1;
        self.trim();
    }

    /// Most writes are inside a snapshot, so the log is also trimmed when the outermost one
    /// closes.
    #[inline(always)]
    fn trim(&mut self) {
        if self.open == 0 && self.rows.len() >= CAP {
            self.rows.clear();
            self.epoch += 1;
        }
    }

    #[inline]
    pub(crate) fn now(&self) -> TouchedAt {
        TouchedAt {
            epoch: self.epoch,
            len: self.rows.len() as u32,
            last: self.rows.last().map_or(0, |row| row.seq),
        }
    }

    /// The rows written since `at`, or `None` when some may have been lost (`at` is `None`,
    /// or rows it read were rolled back or cleared).
    #[inline]
    pub(crate) fn since(&self, at: Option<TouchedAt>) -> Option<&[TouchedRow]> {
        let at = at?;
        let len = at.len as usize;
        if at.epoch != self.epoch || len > self.rows.len() {
            return None;
        }
        let last = if len == 0 { 0 } else { self.rows[len - 1].seq };
        (last == at.last).then(|| &self.rows[len..])
    }
}

/// The union-find tables whose rows a stalled goal reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum VarTable {
    /// Type variables' equality table: `stalled_vars`' type variables.
    Ty,
    /// Type variables' sub-unification table: `sub_roots`.
    Sub,
    Int,
    Float,
    Const,
}

impl VarTable {
    pub(crate) const ALL: [VarTable; 5] =
        [VarTable::Ty, VarTable::Sub, VarTable::Int, VarTable::Float, VarTable::Const];
}

impl InferCtxtInner<'_> {
    #[inline]
    pub(crate) fn touched(&self, table: VarTable) -> &Touched {
        match table {
            VarTable::Ty => self.type_variable_storage.touched_eq(),
            VarTable::Sub => self.type_variable_storage.touched_sub(),
            VarTable::Int => self.int_unification_storage.touched(),
            VarTable::Float => self.float_unification_storage.touched(),
            VarTable::Const => self.const_unification_storage.touched(),
        }
    }
}

#[cfg(test)]
mod tests;
