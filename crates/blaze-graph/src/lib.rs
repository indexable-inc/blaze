//! blaze's build driver as a graph that does no IO (docs/driver.md).
//!
//! [`machine`] is the graph: nodes with explicit states, and a step function from a completion
//! to the requests it makes and the records it says. It reads no file, clock or thread. [`exec`]
//! does what it asks, every ready node at once as the pools allow, and feeds back what came of
//! it. [`trace`] keeps a build's inputs, so a build replays as a test.
//!
//! [`plane`] is the same graph for every request a daemon has under way
//! (docs/v2/plan/lanes/core/plane.md): single flight, fair share, critical path and memory
//! admission, with [`admission`]'s estimates of what each task holds and [`arena`]'s measure
//! and cap of what it does hold. It remembers no result: a key done before is the [`Memo`]'s,
//! the query database's. [`Machine`] is the plane with one request.
//!
//! [`pool`] is the threads the plane's tasks run on, one per core, stealing work nearest first
//! by [`topology`] (a CCD or a cluster, then a socket, then any). [`pump`] is the rule for a
//! wait inside a task (#1301): one run of a key at a time, in a table of buckets, its waiters
//! suspended in [`sched`], and a wait that would close a cycle is [`Cancelled`] and its task
//! posted again. [`build`] says a build's nodes:
//! the workspace load, codegen with its store writes, links and test runs (docs/driver.md
//! steps 4 to 7). [`par`] is the parallel loops of every crate, on the same pool: blaze has no
//! other.
//!
//! [`sched`] is the scheduler the pool and the pump are to become (docs/v2/plan/lanes/sched.md):
//! a state machine with no thread, clock or sleep, in which a waiting job holds no thread.
//!
//! Unsafe code is in [`exec`]'s slot table, [`pool`]'s jobs and queues, [`par`]'s item
//! sources and [`pump`]'s table of runs (structures whose safe versions would take a lock or
//! an `Arc` per job, run or waiter, or a lock per key held across its run, each with model
//! tests under `--cfg blaze_check`), and in `arena`'s allocator hook and
//! the kernel calls of `arena` and [`topology`], each in a small module of its own.

#![deny(unsafe_code)]

pub mod admission;
pub mod arena;
pub mod build;
#[allow(unsafe_code)]
pub mod exec;
pub mod history;
pub mod machine;
#[allow(unsafe_code)]
pub mod par;
pub mod plane;
#[allow(unsafe_code)]
pub mod pool;
#[allow(unsafe_code)]
pub mod pump;
pub mod sched;
pub mod spin;
mod sync;
pub mod topology;
pub mod trace;
pub mod wait;

pub use machine::{Config, Error, Input, Machine, NodeId, Outcome, Output, Pool, Spec};
pub use plane::{
    Budget, Key, Memo, NoMemo, Plane, PlaneConfig, PlaneInput, PlaneOutput, RequestId,
    RequestStats,
};

/// A task stopped before its end because going on could not end: a wait that would close a
/// cycle of waits (#1301), memory that could not be had while every holder waits for more, or
/// its request ended. Its task is posted again whole; nothing of it is kept.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Cancelled;

impl std::fmt::Display for Cancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("cancelled, to be run again")
    }
}

impl std::error::Error for Cancelled {}

#[cfg(test)]
#[allow(clippy::disallowed_types, reason = "a test's own bookkeeping, off every hot path")]
mod plane_tests;
#[cfg(test)]
#[allow(clippy::disallowed_types, reason = "a test's own bookkeeping, off every hot path")]
mod tests;
