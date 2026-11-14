//! The read recorder: what a piece of work read, at item grain, for its read list.
//!
//! Off unless the snapshot was made with [`Reads::Kept`]. On, each run gathers in its own
//! frame the rows it read: [`Read::Input`] for an input getter (and [`read`]), and for each
//! tracked query it asked, hit or run, either one [`Read::Query`] row (the query's kind is a
//! fact boundary, [`crate::Builder::mark_boundary`]: its own list stays behind, the firewall)
//! or that query's own list appended (every other kind is flattened into its asker). At the
//! run's end the list is sorted and deduplicated and kept in the entry, in the arena; it
//! depends on the entry alone, never on who asked first. Port decision 2 (2026-11-13).
//!
//! Cost (rule 67): one row per boundary call and per input read in the list of each entry
//! that reads it (inputs flattened through non-boundary queries are copied into each asker:
//! up to 88 bytes a row), an append per hit, a sort per run. No per-entry edge list beyond
//! that, and no walk afterwards. Off: one branch per fetch and an empty slice per entry.

use std::{
    alloc::{Allocator, Global},
    cmp::Ordering,
    fmt,
    hash::{Hash, Hasher},
    ptr,
};

use blaze_ids::ContentId;

use crate::{CycleKind, Fact, Gid, Kind, Memo, Reads, task};

/// A query's or an input's kind, as a list row names it: copied, compared and ordered by the
/// kind's name hash then its name, so a sorted list is in the same order in every process.
#[derive(Clone, Copy)]
pub struct KindId(&'static Kind);

impl KindId {
    pub const fn of(kind: &'static Kind) -> KindId {
        KindId(kind)
    }

    /// The kind's full name: `module::f`, `module::Type::method`, or a reserved name
    /// ([`AROUND_INPUTS`], [`UNTRACKED`], [`IMAGE_FACT`]).
    pub const fn name(self) -> &'static str {
        self.0.name()
    }

    pub const fn kind(self) -> &'static Kind {
        self.0
    }
}

impl PartialEq for KindId {
    /// By name, as [`Ord`] orders (a kind's name is its full path, one per kind): the pointer
    /// first, the common case.
    fn eq(&self, other: &KindId) -> bool {
        ptr::eq(self.0, other.0) || (self.0.id() == other.0.id() && self.0.name() == other.0.name())
    }
}

impl Eq for KindId {}

impl Ord for KindId {
    fn cmp(&self, other: &KindId) -> Ordering {
        (self.0.id(), self.0.name()).cmp(&(other.0.id(), other.0.name()))
    }
}

impl PartialOrd for KindId {
    fn partial_cmp(&self, other: &KindId) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Hash for KindId {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.0.id().hash(state);
    }
}

impl fmt::Debug for KindId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0.name())
    }
}

/// One row of a read list.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Read {
    /// A fact read directly: an input's (its kind the input's), one read around the inputs
    /// ([`read`], kind [`AROUND_INPUTS`]), an untracked read ([`read_untracked`]), or a fact a
    /// served image entry read ([`IMAGE_FACT`]).
    Input { kind: KindId, fact: Fact },
    /// A boundary query asked: its kind and its entry's id in this snapshot (the query's key,
    /// [`crate::Memo::boundary_key`] in the generated `key_of`). Valid in this snapshot only:
    /// a persisted list holds stable facts, translated before anything is filed.
    Query { kind: KindId, key: Gid },
}

/// The kind of a fact [`read`] lists without an input of its own: a record read from the
/// store, say.
pub static AROUND_INPUTS: Kind = Kind::new("blaze_memo::read", CycleKind::Panic);
/// The kind of an untracked read ([`read_untracked`]): a classifier refuses to store a list
/// holding one, so the reader is recomputed in each snapshot and never filed.
pub static UNTRACKED: Kind = Kind::new("blaze_memo::untracked", CycleKind::Panic);
/// The kind of the facts of an entry served from the crate's own image: the record's list,
/// each an input fact or a boundary query's stable fact as the image translated it.
pub static IMAGE_FACT: Kind = Kind::new("blaze_memo::image", CycleKind::Panic);

impl From<Fact> for Read {
    fn from(fact: Fact) -> Read {
        Read::Input { kind: KindId(&AROUND_INPUTS), fact }
    }
}

impl Read {
    pub const fn kind(self) -> KindId {
        match self {
            Read::Input { kind, .. } | Read::Query { kind, .. } => kind,
        }
    }

    /// Whether this row is an untracked read.
    pub fn is_untracked(self) -> bool {
        self.kind() == KindId(&UNTRACKED)
    }
}

/// The recorder is off in this snapshot ([`Reads::Off`]): nothing was listed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct RecorderOff;

impl std::fmt::Display for RecorderOff {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the read recorder is off in this snapshot (`Reads::Off`): make it with `Reads::Kept`")
    }
}

/// Lists `read` as read by the current run (for a read the memo does not see: a record read
/// from the store, say). A bare [`Fact`] is a [`Read::Input`] of kind [`AROUND_INPUTS`].
pub fn read(memo: &Memo, read: impl Into<Read>) {
    task::note_fact(memo, read.into());
}

/// Lists an untracked read, `what` naming it (`"the process environment"`): something read
/// that no input covers. Its digest is the snapshot's own, so it never matches another
/// snapshot's, and a classifier refuses to store a list holding it.
pub fn read_untracked(memo: &Memo, what: &'static str) {
    let fact = Fact {
        id: ContentId::of("blaze_memo untracked read", what.as_bytes()),
        digest: ContentId::of("blaze_memo untracked epoch", &memo.snapshot_epoch().get().to_le_bytes()),
    };
    task::note_fact(memo, Read::Input { kind: KindId(&UNTRACKED), fact });
}

/// Runs `work` with the recorder off for it: nothing it reads, asks or records reaches the
/// running query's list or any [`record`] open around it (a [`record`] inside `work` still
/// gives its own rows). For bookkeeping whose reads are no part of the running query's value:
/// a span's name, a cache filing a goal. That is the caller's promise; the memo cannot check
/// it, and a value that does depend on such a read is listed without it. Borrows of the arenas
/// `work` reads are still noted (they keep values sound, not lists right).
pub fn unrecorded<T>(memo: &Memo, work: impl FnOnce() -> T) -> T {
    if memo.reads() != Reads::Kept {
        return work();
    }
    let frame = task::Run::unrecorded(memo);
    let _pushed = task::push(&frame);
    work()
}

/// Runs `work` and gives the rows it read, sorted and deduplicated, in the global heap.
///
/// # Errors
///
/// [`RecorderOff`] when the snapshot keeps no read lists.
pub fn record<T>(memo: &Memo, work: impl FnOnce() -> T) -> Result<(T, Vec<Read, Global>), RecorderOff> {
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
) -> Result<(T, Vec<Read, A>), RecorderOff> {
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
