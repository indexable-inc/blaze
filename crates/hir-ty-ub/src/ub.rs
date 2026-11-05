//! What the checker found: one undefined behavior, where, and on which thread.
//!
//! Each part of the model names its own errors ([`memory::MemError`], [`borrows::AliasError`],
//! [`race::RaceError`], [`check::ValueError`]); [`Ub`] is the one a run stops with. The first
//! is the only one: like Miri, the run ends at it, since what follows UB means nothing.

use std::fmt;

use crate::{
    borrows, check,
    ids::{SiteId, ThreadId},
    memory, race,
};

/// An undefined behavior, or a leak or deadlock the run ended with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Ub {
    /// Bounds, use after free, alignment, uninitialized bytes, provenance, frees.
    Memory(memory::MemError),
    /// Tree Borrows refused an access, a retag or a free.
    Aliasing(borrows::AliasError),
    /// A data race, or a mixed-size or non-atomic access racing an atomic one.
    Race(race::RaceError),
    /// A value invalid at its type (a `bool` of 2, a dangling reference, an uninhabited type).
    Value(check::ValueError),
    /// `ptr_offset_from` of pointers it is undefined for (other allocations, out of bounds).
    OffsetFrom(check::OffsetFromError),
    /// The main thread returned with memory never freed and reachable from no static.
    Leak(memory::Leaks),
    /// Every thread left is blocked.
    Deadlock(race::Deadlock),
    /// The main thread returned while other threads still ran, which Miri fails before its
    /// leak check (`src/tools/miri/src/eval.rs`).
    ThreadsLeft,
}

/// A [`Ub`] with where the program was: the thread and the statement it was running.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Found {
    pub ub: Ub,
    pub thread: ThreadId,
    pub at: SiteId,
}

impl fmt::Display for Ub {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Ub::Memory(it) => write!(f, "Undefined Behavior: {it}"),
            Ub::Aliasing(it) => write!(f, "Undefined Behavior: {it}"),
            Ub::Race(it) => write!(f, "Undefined Behavior: {it}"),
            Ub::Value(it) => write!(f, "Undefined Behavior: {it}"),
            Ub::OffsetFrom(it) => write!(f, "Undefined Behavior: {it}"),
            Ub::Leak(it) => write!(f, "memory leaked: {it}"),
            Ub::Deadlock(it) => write!(f, "deadlock: {it}"),
            Ub::ThreadsLeft => {
                f.write_str("the main thread terminated without waiting for all remaining threads")
            }
        }
    }
}

impl From<memory::MemError> for Ub {
    fn from(it: memory::MemError) -> Ub {
        Ub::Memory(it)
    }
}

impl From<borrows::AliasError> for Ub {
    fn from(it: borrows::AliasError) -> Ub {
        Ub::Aliasing(it)
    }
}

impl From<race::RaceError> for Ub {
    fn from(it: race::RaceError) -> Ub {
        Ub::Race(it)
    }
}

impl From<check::ValueError> for Ub {
    fn from(it: check::ValueError) -> Ub {
        Ub::Value(it)
    }
}
