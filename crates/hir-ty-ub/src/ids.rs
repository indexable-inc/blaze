//! The small types every part of the checker shares: what names an allocation, a pointer's tag,
//! a thread and a place in the program, and the kinds of access.
//!
//! Each is a `u32` or `u64` newtype, so the per-byte and per-node tables that hold them stay
//! flat. None of them goes into a hash, a key or an output: they are dense ids of one run.

use std::{fmt, num::NonZeroU32, ops::Range};

/// An address the program sees: the evaluator's `usize` of its target, widened.
pub type Addr = u64;

/// One allocation of a run, in the order they were made, from 1. Ids are never reused, so a
/// pointer into freed memory still names what it pointed to (use after free is reported by it).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AllocId(NonZeroU32);

const _: () = assert!(size_of::<AllocId>() == 4 && size_of::<Option<AllocId>>() == 4);

impl AllocId {
    /// The `n`th allocation, counting from 1.
    pub fn new(n: u32) -> Option<AllocId> {
        NonZeroU32::new(n).map(AllocId)
    }

    pub fn get(self) -> u32 {
        self.0.get()
    }

    /// Its place in a table of every allocation, from 0.
    pub fn index(self) -> usize {
        self.0.get() as usize - 1
    }
}

impl fmt::Debug for AllocId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "alloc{}", self.0)
    }
}

/// A pointer's tag in the aliasing model: Tree Borrows' `BorTag` (Miri,
/// `src/tools/miri/src/borrow_tracker/mod.rs`). Tag 0 is never made; each allocation's root
/// pointer gets a fresh one, and every retag makes another.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BorTag(u64);

const _: () = assert!(size_of::<BorTag>() == 8);

impl BorTag {
    pub const fn new(n: u64) -> BorTag {
        BorTag(n)
    }

    pub const fn get(self) -> u64 {
        self.0
    }

    /// The tag after this one, as Miri's `BorTag::succ`.
    pub fn succ(self) -> Option<BorTag> {
        self.0.checked_add(1).map(BorTag)
    }
}

impl fmt::Debug for BorTag {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<{}>", self.0)
    }
}

/// What a pointer carries besides its address (Miri's `machine::Provenance`): the allocation
/// and tag it was made with, or `Wildcard` for one made from an integer whose address was
/// exposed (permissive provenance: it may be any exposed tag of the allocation it points into).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Prov {
    Concrete { alloc: AllocId, tag: BorTag },
    Wildcard,
}

const _: () = assert!(size_of::<Prov>() == 16);

/// A thread of the run. The main thread is 0; spawned threads count up and are never reused.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct ThreadId(pub u32);

impl ThreadId {
    pub const MAIN: ThreadId = ThreadId(0);

    pub fn index(self) -> usize {
        self.0 as usize
    }
}

/// A place in the program a report can point at: the checker's dense id for the evaluator's
/// `(body, MIR span)` of the statement or terminator being run (`check::Sites` interns them).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub struct SiteId(pub u32);

impl SiteId {
    /// Where nothing better is known: the start of the run.
    pub const START: SiteId = SiteId(0);
}

/// A read or a write, as the aliasing model and the race detector tell them apart.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum AccessKind {
    Read,
    Write,
}

/// The memory orderings of `core::sync::atomic::Ordering`: the evaluator's, which hands them to
/// the checker as they are.
pub use hir_ty_mir::mir::AtomicOrd;

/// A range of bytes of one allocation, as offsets from its start.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Span {
    pub start: u64,
    pub len: u64,
}

impl Span {
    pub fn new(start: u64, len: u64) -> Span {
        Span { start, len }
    }

    pub fn end(self) -> u64 {
        self.start + self.len
    }

    pub fn range(self) -> Range<u64> {
        self.start..self.end()
    }

    pub fn is_empty(self) -> bool {
        self.len == 0
    }
}

#[cfg(test)]
mod tests {
    use super::AtomicOrd;

    /// The checker's own orderings before it took the evaluator's: which acquire, which release.
    fn old(ord: AtomicOrd) -> (bool, bool) {
        match ord {
            AtomicOrd::Relaxed => (false, false),
            AtomicOrd::Acquire => (true, false),
            AtomicOrd::Release => (false, true),
            AtomicOrd::AcqRel | AtomicOrd::SeqCst => (true, true),
        }
    }

    #[test]
    fn the_evaluators_orderings_acquire_and_release_as_the_checkers_did() {
        use AtomicOrd::*;
        for ord in [Relaxed, Acquire, Release, AcqRel, SeqCst] {
            assert_eq!((ord.acquires(), ord.releases()), old(ord), "{ord:?}");
        }
    }
}
