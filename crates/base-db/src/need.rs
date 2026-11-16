//! [`Need`]: the typed stop of a compiler task that lacks a record (docs/v2/plan/design/bottom.md,
//! "How a task waits: it does not"; docs/v2/plan/design/reconnect.md, "Need").
//!
//! A task (check this body, lower this signature) never parks and never makes another item's
//! record inline. Where a facade function of hir-def finds that a record it reads is not there
//! for a crate whose text the task does not hold, it throws a [`Need`] naming the missing key.
//! The stop unwinds the task through the memo engine the way [`blaze_memo::Cancelled`] does
//! (`resume_unwind`, never the panic hook), and the driver catches it at the task's top
//! ([`Need::catch`]), makes the record the stop names, and enters the task again from the top.
//! What the first entry computed is not lost: what it read is records, which the second entry
//! reads again in place, and the memo's entries it filled stay in the snapshot.
//!
//! The set of stops of one run of a task is its need list: the driver may make all of them
//! before the next entry, so a task is entered at most once per depth of what it needs.

use std::alloc::Global;
use std::any::Any;
use std::fmt;

use blaze_surface::{ContentId, CrateItemKey, ItemKey, SurfaceError};

/// A record the running task needs and the snapshot lacks: the key the driver makes it by.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Needed {
    /// A record of one item: `fact` of `item`.
    Item { fact: &'static str, item: ItemKey },
    /// A record of one file (its `FileItems`, or its text for the crate the task compiles).
    File { fact: &'static str, file: ContentId },
    /// A record of one crate (its root, lang items, graph, impls).
    Crate { fact: &'static str, krate: CrateItemKey },
}

impl fmt::Display for Needed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Needed::Item { fact, item } => write!(f, "{fact} of the item {item:x}"),
            Needed::File { fact, file } => write!(f, "{fact} of the file {file:x}"),
            Needed::Crate { fact, krate } => write!(f, "{fact} of the crate {krate:x}"),
        }
    }
}

/// Why a task stopped: the question the surface could not answer, as the surface said it. A
/// stop that names a record the driver can make is a [`Needed`] ([`Need::needed`]); the rest
/// (a question the layer cannot answer yet, a proof of another crate, a key of no item) are
/// errors of the task, reported with the item they name.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Need {
    pub why: SurfaceError,
}

impl Need {
    /// Unwinds the running task with this stop. Never the panic hook: a stop is not a bug.
    pub fn throw(self) -> ! {
        std::panic::resume_unwind(Box::new_in(self, Global))
    }

    /// Runs `f`, a [`Need`] it throws as `Err`; any other panic goes on unwinding.
    pub fn catch<T>(f: impl FnOnce() -> T) -> Result<T, Need> {
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
            Ok(value) => Ok(value),
            Err(payload) => match payload.downcast::<Need>() {
                Ok(need) => Err(*need),
                Err(other) => std::panic::resume_unwind(other),
            },
        }
    }

    /// The `Need` a caught panic's payload is, if it is one.
    pub fn of_payload(payload: &(dyn Any + Send)) -> Option<&Need> {
        payload.downcast_ref::<Need>()
    }

    /// The record the driver makes to answer this stop, or `None` where no record answers it
    /// (the stop is the task's error).
    pub fn needed(&self) -> Option<Needed> {
        match self.why {
            SurfaceError::NoRecord { fact, item } => Some(Needed::Item { fact, item }),
            SurfaceError::NoFileRecord { fact, file } => Some(Needed::File { fact, file }),
            SurfaceError::NoCrateRecord { fact, krate } => Some(Needed::Crate { fact, krate }),
            SurfaceError::NotBuilt(_)
            | SurfaceError::NoItem(_)
            | SurfaceError::WrongCrate { .. }
            | SurfaceError::NoLocal { .. }
            | SurfaceError::LocalsFull => None,
        }
    }
}

impl From<SurfaceError> for Need {
    fn from(why: SurfaceError) -> Need {
        Need { why }
    }
}

impl fmt::Display for Need {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.needed() {
            Some(needed) => write!(f, "this task needs {needed}"),
            None => fmt::Display::fmt(&self.why, f),
        }
    }
}

impl std::error::Error for Need {}

/// A surface answer, or the task's stop: what every facade function that reads a record calls
/// on the read, so a missing record of a crate without text is a [`Need`] and never a fall
/// into text (docs/v2/plan/design/bottom.md, point 5).
pub trait OrNeed<T> {
    fn or_need(self) -> T;
}

impl<T> OrNeed<T> for Result<T, SurfaceError> {
    #[inline]
    #[track_caller]
    fn or_need(self) -> T {
        match self {
            Ok(value) => value,
            Err(why) => Need { why }.throw(),
        }
    }
}
