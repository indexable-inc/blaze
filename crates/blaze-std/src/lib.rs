//! bstd, blaze's own standard library, step S1 (docs/v2/plan/design/std.md): the interface,
//! over blaze-core's drivers and its simulation.
//!
//! - [`io::Io`] is one concrete handle to the process's driver, the target's
//!   [`blaze_core::Native`] or [`blaze_core::sim::Sim`]. A call turns into a
//!   [`blaze_core::Effect`] and waits for its [`blaze_core::Completion`] under the task's
//!   deadline.
//! - [`cx::Cx`] is the task's context, the first parameter of every call that can wait: its
//!   deadline (which only narrows), its cancellation, its span and its frame.
//! - The capabilities ([`fs::Fs`], [`fs::Dir`], [`fs::File`], [`clock::Clock`], [`rng::Rng`],
//!   [`env::Env`], [`args::Args`], [`host::Host`], [`mem::Mem`], [`net::Net`]) have private
//!   fields and are made only from an `Io` or by narrowing one, which never widens.
//! - Tasks are started by a context: [`cx::Cx::scope`] ([`task`]), in the `Inline` layout.
//! - Nothing is global: the crate holds no `static` with interior mutability, no `static mut`
//!   and no thread-local. Spans and events go through [`cx::Cx`] into the recorder its world
//!   owns ([`trace::Trace`]).
//! - [`path::Path`] is relative and cannot hold a root or `..`.
//! - Every wait ends in a typed outcome: what it waited for, or a [`Stopped`].
//!
//! What S1 does not have yet, and why, is in the crate's report and in std.md: the calling
//! fiber does not park on a scheduler (the tree has no fiber park yet); a call waits by
//! polling the driver inline behind one seam, [`io::Io`]'s `wait`, which a fiber park replaces.
//! Standard input, output and error (`Out`), child processes (`Process`), the process's own
//! signals, the host's name, wall time, randomness from the system, name lookup, directories
//! made empty and opens resolved beneath a directory by the kernel each need an effect
//! blaze-core does not have.
//!
//! Until S4, `alloc`'s global allocator is still underneath: the few global allocations here
//! are the effects' own buffers (blaze-core's `Effect` owns its bytes) and errors' messages,
//! each with its `// alloc:` line. What bstd hands back to a caller is made in the caller's
//! allocator ([`mem::Bytes`]).

#![deny(unsafe_code)]
#![feature(allocator_api)]

pub mod args;
pub mod clock;
pub mod cx;
pub mod env;
pub mod fs;
pub mod host;
pub mod io;
pub mod mem;
pub mod net;
pub mod path;
pub mod rng;
pub mod task;
pub mod trace;

#[cfg(test)]
mod tests;

use std::{fmt, time::Duration};

pub use crate::{clock::Clock, cx::Cx, io::Io, path::Path};
pub use blaze_core::wait::Deadline;

/// Why a wait or an allocation stopped short of what it was for. Every blocking call ends in
/// what it waited for or in one of these (rules bounded-progress, deadline-waits).
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Stopped {
    /// The task's deadline passed first, at `at` on the driver's clock.
    Deadline { span: &'static str, at: Duration },
    /// The task's scope was cancelled.
    Cancelled { span: &'static str },
    /// `arena` could not hand out `asked` more bytes within its limit.
    OutOfMemory { arena: &'static str, asked: usize },
    /// The driver said nothing was ready this many times in a row before the deadline with
    /// its clock not past it: a driver that does not keep its own deadline.
    Stalled { span: &'static str, polls: u32 },
}

impl fmt::Display for Stopped {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Stopped::Deadline { span, at } => {
                write!(f, "{span}: its deadline ({at:?} on the driver's clock) passed first")
            }
            Stopped::Cancelled { span } => write!(f, "{span}: its scope was cancelled"),
            Stopped::OutOfMemory { arena, asked } => {
                write!(f, "arena {arena}: {asked} more bytes would pass its limit")
            }
            Stopped::Stalled { span, polls } => write!(
                f,
                "{span}: the driver said nothing was ready {polls} times in a row before the \
                 deadline (rule bounded-progress)"
            ),
        }
    }
}

impl std::error::Error for Stopped {}

/// A safeguard a test turns off to prove its oracle sees the bug (skill `testing`, planted-bug
/// controls). Always [`Plant::None`] outside this crate's tests: only their constructor
/// (`Io::planted`) sets another.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[cfg_attr(not(test), expect(dead_code, reason = "the planted bugs are made by the tests only"))]
pub(crate) enum Plant {
    None,
    /// `Dir::subdir` keeps its parent's base: a narrowed directory reaches its parent.
    SubdirKeepsParent,
    /// `Dir::open` of a missing file makes it, through a read-only directory too.
    OpenCreates,
    /// A path's `..` is let in and resolved by text after the join, as `a/../../x`.
    LexicalParent,
    /// A wait polls with no deadline, so it ends when its effect does.
    WaitIgnoresDeadline,
    /// `Cx::until` takes the deadline it is given, later than its parent's or not.
    UntilWidens,
    /// A wait does not look at its task's cancellation.
    WaitIgnoresCancel,
    /// An arena is made with no limit of its own.
    ArenaUnlimited,
    /// `File::sync` says it synced without asking the driver.
    SyncSkipped,
    /// `Env::only` replaces what is allowed rather than narrowing it.
    EnvOnlyReplaces,
}
