//! The solver's read lists and nested-goal lists as one append-only log of dense ids in nested
//! segments (solve.md (e)).
//!
//! A frame of the stack owns the log's suffix from where it started (`Frame::reads_lo`): what it
//! read, what its finished subgoals read, and what the cached results it used read. Each
//! segment is a SET: an id is appended only if the open segment does not hold it yet, which
//! is one load (`last[id]`, the id's latest position, compared with the segment's start).
//! When a frame finishes, its segment, which is the result's read list, is folded into the
//! parent's ([`SegLog::close`]): the ids the parent's own part already holds are dropped,
//! found through `prev` (each position's previous occurrence of its id), so the parent's
//! segment stays a set without sorting or hashing. Truncation (a rerun, an unwind) puts
//! `last` back from `prev`, so `last` is always the latest live occurrence.
//!
//! The cost is one load and compare per id appended and one pass over a finished segment: no
//! per-goal sort (the profile of tempo's check on 2026-11-17 had 22% of the busy thread in the
//! sort this replaces). The order of a segment is the order of first reads, a function of the
//! evaluation alone (rule 25); a writer that wants another order sorts once where it writes.

use blaze_hash::{FixedState, Map, MapOpts};
use blaze_own::{Alloc, Vec};

use crate::error::SolveError;
use crate::facts::{Log, Read};

const NONE: u32 = u32::MAX;

/// A log of dense ids in nested segments, each a set (the module doc).
pub struct SegLog<'a> {
    log: Vec<u32, Alloc<'a>>,
    /// Per position: the previous live position of its id, or `NONE`.
    prev: Vec<u32, Alloc<'a>>,
    /// Per id: its latest live position, or `NONE`.
    last: Vec<u32, Alloc<'a>>,
}

impl<'a> SegLog<'a> {
    pub fn new_in(alloc: Alloc<'a>) -> SegLog<'a> {
        SegLog { log: Vec::new_in(alloc), prev: Vec::new_in(alloc), last: Vec::new_in(alloc) }
    }

    pub fn len(&self) -> usize {
        self.log.len()
    }

    pub fn is_empty(&self) -> bool {
        self.log.is_empty()
    }

    /// The ids from position `lo` on.
    pub fn from(&self, lo: usize) -> &[u32] {
        &self.log[lo..]
    }

    /// Appends `id` to the segment that starts at `lo`, unless it holds it already.
    pub fn push(&mut self, lo: u32, id: u32) -> Result<(), SolveError> {
        let i = id as usize;
        if i >= self.last.len() {
            self.last.resize(i + 1, NONE);
        }
        let p = self.last[i];
        if p != NONE && p >= lo {
            return Ok(());
        }
        let at = crate::error::row(self.log.len())?;
        self.log.push(id);
        self.prev.push(p);
        self.last[i] = at;
        Ok(())
    }

    /// Appends each of `ids` as [`SegLog::push`] does.
    pub fn extend(&mut self, lo: u32, ids: &[u32]) -> Result<(), SolveError> {
        for &id in ids {
            self.push(lo, id)?;
        }
        Ok(())
    }

    /// Drops every position from `to` on, putting `last` back.
    pub fn truncate(&mut self, to: usize) {
        while self.log.len() > to {
            let (Some(id), Some(p)) = (self.log.pop(), self.prev.pop()) else { break };
            self.last[id as usize] = p;
        }
    }

    /// Folds the finished segment from `child_lo` into the open one from `parent_lo`
    /// (`parent_lo <= child_lo`): an id of the child's segment whose previous occurrence is in
    /// the parent's own part is dropped. Both segments are sets before, the parent's is after.
    pub fn close(&mut self, parent_lo: u32, child_lo: u32) {
        let mut w = child_lo as usize;
        for r in child_lo as usize..self.log.len() {
            let id = self.log[r];
            let p = self.prev[r];
            if p != NONE && p >= parent_lo {
                // The parent holds it: its latest occurrence is that one again.
                self.last[id as usize] = p;
                continue;
            }
            self.log[w] = id;
            self.prev[w] = p;
            self.last[id as usize] = w as u32;
            w += 1;
        }
        self.log.truncate(w);
        self.prev.truncate(w);
    }
}

/// The solver's read log: each [`Read`] interned to a dense id once per solver, the ids in a
/// [`SegLog`]. `lo` is the start of the open segment, the top frame's (set before a rule runs).
pub struct ReadLog<'a> {
    ids: Map<Read, u32, FixedState, Alloc<'a>>,
    rows: Vec<Read, Alloc<'a>>,
    pub(crate) seg: SegLog<'a>,
    pub(crate) lo: u32,
}

impl<'a> ReadLog<'a> {
    pub fn new_in(alloc: Alloc<'a>) -> ReadLog<'a> {
        ReadLog {
            ids: Map::new(MapOpts { hasher: FixedState, alloc, capacity: None }),
            rows: Vec::new_in(alloc),
            seg: SegLog::new_in(alloc),
            lo: 0,
        }
    }

    /// The read with id `id`.
    pub fn read(&self, id: u32) -> Read {
        self.rows[id as usize]
    }

    /// The id of `r`, interning it.
    pub fn intern(&mut self, r: Read) -> Result<u32, SolveError> {
        if let Some(&id) = self.ids.get(&r) {
            return Ok(id);
        }
        let id = crate::error::row(self.rows.len())?;
        self.rows.push(r);
        self.ids.insert(r, id);
        Ok(id)
    }

    /// Appends the reads of the open segment from `lo` to `out`, as reads.
    pub fn export(&self, lo: usize, out: &mut Vec<Read, Alloc<'_>>) {
        for &id in self.seg.from(lo) {
            out.push(self.rows[id as usize]);
        }
    }
}

impl Log for ReadLog<'_> {
    fn note(&mut self, r: Read) -> Result<(), SolveError> {
        let id = self.intern(r)?;
        self.seg.push(self.lo, id)
    }
}
