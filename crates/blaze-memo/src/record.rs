//! The read recorder: which facts a piece of work read, at fact grain, for its read list.
//!
//! Off unless the snapshot was made with [`Reads::Kept`]. On, each run gathers in its own
//! frame the facts it read directly (input getters, [`read`]) and the lists of the entries it
//! hit or ran; at its end the list is sorted and deduplicated and kept in the entry, in the
//! arena. A hit appends the entry's list to the asker's frame, so a list is the same whether
//! the result was computed or found. The cost on: 32+32 bytes per fact per entry that read it
//! (shared facts are copied into each entry that reads them, a cost against the floor of one
//! list per recorded item, said in the report), an append per hit, a sort per run. Off: one
//! branch per fetch and an empty slice per entry.

use std::alloc::{Allocator, Global};

use crate::{Fact, Memo, Reads, task};

/// The recorder is off in this snapshot ([`Reads::Off`]): nothing was listed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct RecorderOff;

impl std::fmt::Display for RecorderOff {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the read recorder is off in this snapshot (`Reads::Off`): make it with `Reads::Kept`")
    }
}

/// Lists `fact` as read by the current run (for a read the memo does not see: a record read
/// from the store, say).
pub fn read(memo: &Memo, fact: Fact) {
    task::note_fact(memo, fact);
}

/// Runs `work` and gives the facts it read, sorted and deduplicated, in the global heap.
///
/// # Errors
///
/// [`RecorderOff`] when the snapshot keeps no read lists.
pub fn record<T>(memo: &Memo, work: impl FnOnce() -> T) -> Result<(T, Vec<Fact, Global>), RecorderOff> {
    record_in(memo, Global, work)
}

/// [`record`], the list in `alloc` (the task's frame, typically).
///
/// # Errors
///
/// As [`record`].
pub fn record_in<T, A: Allocator>(
    memo: &Memo,
    alloc: A,
    work: impl FnOnce() -> T,
) -> Result<(T, Vec<Fact, A>), RecorderOff> {
    if memo.reads() != Reads::Kept {
        return Err(RecorderOff);
    }
    let frame = task::Run::recorder(memo);
    let pushed = task::push(&frame);
    let value = work();
    let reads = frame.take_reads();
    drop(pushed);
    // An outer run read what this work read.
    task::replay_list(&reads);
    let mut out = Vec::with_capacity_in(reads.len(), alloc);
    out.extend_from_slice(&reads);
    Ok((value, out))
}
