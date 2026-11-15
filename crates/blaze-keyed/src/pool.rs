//! The worker pool's interface: what the driver's host gives tasks to.
//!
//! The form it promises (target.md, "Processes"): workers started in proportion to queued work
//! ([`crate::machine::Machine::workers_for`]), none for a no-op or a one-body edit; a
//! work-stealing deque per worker; a frame per worker, reset between tasks and never unmapped;
//! counters per worker, merged when a task ends; nothing shared and mutable touched per item.

#![blaze::sans_io]

use crate::machine::Qos;

/// The number an effect is answered by.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Ticket(pub u64);

/// The pool refused a task: every worker is busy and its deque full.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PoolFull;

/// A pool of workers.
///
/// The invariants:
/// - At most [`Pool::workers`] tasks run at once; the driver never submits more than that.
/// - A task's output is borrowed from its worker's frame until the next [`Pool::finished`]:
///   the host files it from there, with no copy.
/// - Tasks submitted together with [`Pool::submit_batch`] run on one worker in order.
pub trait Pool {
    type Task;
    type Finished<'p>
    where
        Self: 'p;

    fn workers(&self) -> u32;

    /// Starts `more` workers, at `qos`; the pool never starts more than the machine's cores.
    fn grow(&mut self, more: u32, qos: Qos);

    fn submit(&self, ticket: Ticket, task: Self::Task) -> Result<(), PoolFull>;

    fn submit_batch<I: Iterator<Item = (Ticket, Self::Task)>>(&self, tasks: I) -> Result<(), PoolFull>;

    /// A finished task, if one is: the host waits on the pool's completion with a deadline
    /// (`blaze_core::wait`), under a span naming the tickets it waits for.
    fn finished(&self) -> Option<(Ticket, Self::Finished<'_>)>;
}
