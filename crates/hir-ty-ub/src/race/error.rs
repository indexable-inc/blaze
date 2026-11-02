//! What the race detector and the thread bookkeeping stop a run with, in Miri's words
//! (`src/tools/miri/src/diagnostics.rs`, `TerminationInfo::DataRace` and `GlobalDeadlock`;
//! `src/tools/miri/src/concurrency/thread.rs` for joins and detaches).
//!
//! These are built only when the run stops, so they may own strings.

use std::fmt;

use crate::ids::{AllocId, SiteId, ThreadId};

/// One side of a race: what the access was (Miri: `AccessType::description`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Action {
    Read,
    RetagRead,
    Allocate,
    Write,
    RetagWrite,
    Deallocate,
    AtomicLoad,
    AtomicStore,
    AtomicRmw,
}

impl Action {
    pub fn description(self) -> &'static str {
        match self {
            Action::Read => "non-atomic read",
            Action::RetagRead => "retag read",
            Action::Allocate => "creating a new allocation",
            Action::Write => "non-atomic write",
            Action::RetagWrite => "retag write",
            Action::Deallocate => "deallocation",
            Action::AtomicLoad => "atomic load",
            Action::AtomicStore => "atomic store",
            Action::AtomicRmw => "atomic read-modify-write",
        }
    }
}

/// An access in a race report (Miri: `RacingOp`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RacingOp {
    pub action: Action,
    /// The access size, for a race of mixed sizes. `Some(0)` stands for several atomic loads of
    /// different sizes, as in Miri.
    pub size: Option<u64>,
    pub thread: ThreadId,
    /// The thread's name for display: `main`, the name it was given, or `unnamed-N`.
    pub thread_name: String,
    pub site: SiteId,
}

impl fmt::Display for RacingOp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.size {
            Some(0) => f.write_str("multiple differently-sized atomic loads, including one load")?,
            Some(size) => write!(f, "{size}-byte {}", self.action.description())?,
            None => f.write_str(self.action.description())?,
        }
        write!(f, " on thread `{}`", self.thread_name)
    }
}

/// A data race, or a race of atomic accesses of different sizes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DataRace {
    /// One of the two is non-atomic: "Data race"; else "Race condition".
    pub involves_non_atomic: bool,
    pub alloc: AllocId,
    /// The offset in `alloc` of the first byte found racing.
    pub offset: u64,
    /// The earlier access.
    pub op1: RacingOp,
    /// The access that found the race.
    pub op2: RacingOp,
    /// The two are atomic and of different sizes.
    pub mixed_size: bool,
    /// One of the two is a retag's implicit access.
    pub retag: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RaceErrorKind {
    Race(DataRace),
    /// A weak atomic load read a store whose bytes were not initialized.
    UninitLoad { alloc: AllocId, offset: u64 },
    JoinDetached(ThreadId),
    JoinJoined(ThreadId),
    JoinSelf(ThreadId),
    /// `join` of a thread that has not exited: the evaluator joins only after it has.
    JoinRunning(ThreadId),
    DetachTwice(ThreadId),
}

/// Why the race detector stopped the run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RaceError {
    pub kind: RaceErrorKind,
}

impl RaceError {
    pub(super) fn new(kind: RaceErrorKind) -> RaceError {
        RaceError { kind }
    }

    /// The help lines Miri prints under the report, in its order.
    pub fn help(&self) -> Vec<&'static str> {
        let mut helps = Vec::new();
        if let RaceErrorKind::Race(race) = &self.kind {
            if race.mixed_size {
                helps.push("overlapping unsynchronized atomic accesses must use the same access size");
                helps.push("see https://doc.rust-lang.org/nightly/std/sync/atomic/index.html#memory-model-for-atomic-accesses for more information about the Rust memory model");
            }
            if race.retag {
                helps.push("retags occur on all (re)borrows and as well as when references are copied or moved");
                helps.push("retags permit optimizations that insert speculative reads or writes");
                helps.push("therefore from the perspective of data races, a retag has the same implications as a read or write");
            }
        }
        helps
    }

    /// The sites of the two racing accesses, earlier one first.
    pub fn sites(&self) -> Option<(SiteId, SiteId)> {
        match &self.kind {
            RaceErrorKind::Race(race) => Some((race.op1.site, race.op2.site)),
            _ => None,
        }
    }
}

impl fmt::Display for RaceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.kind {
            RaceErrorKind::Race(race) => {
                write!(
                    f,
                    "{} detected between (1) {} and (2) {} at {:?}",
                    if race.involves_non_atomic { "Data race" } else { "Race condition" },
                    race.op1,
                    race.op2,
                    race.alloc,
                )?;
                if race.offset != 0 {
                    write!(f, "+{:#x}", race.offset)?;
                }
                Ok(())
            }
            RaceErrorKind::UninitLoad { .. } => {
                f.write_str("using uninitialized data, but this operation requires initialized memory")
            }
            RaceErrorKind::JoinDetached(_) => f.write_str("trying to join a detached thread"),
            RaceErrorKind::JoinJoined(_) => f.write_str("trying to join an already joined thread"),
            RaceErrorKind::JoinSelf(_) => f.write_str("trying to join itself"),
            RaceErrorKind::JoinRunning(thread) => {
                write!(f, "joining thread {}, which has not terminated", thread.0)
            }
            RaceErrorKind::DetachTwice(_) => {
                f.write_str("trying to detach thread that was already detached or joined")
            }
        }
    }
}

/// Every thread left is blocked (Miri: `TerminationInfo::GlobalDeadlock`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Deadlock {
    /// The threads that have not terminated, all blocked.
    pub threads: Vec<ThreadId>,
}

impl fmt::Display for Deadlock {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("the evaluated program deadlocked")
    }
}
