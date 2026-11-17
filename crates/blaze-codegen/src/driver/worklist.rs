//! A worklist on the pool: items visited on all cores, each visit free to push more, with no
//! round between an item and what it pushed (docs/v2/plan/design/scheduler.md, "Codegen: a
//! worklist"). The build's walk is one: an instance is an item, and describing it pushes the
//! instances it newly reaches.
//!
//! The items are one queue, first in first out, so what the visits push is visited roughly
//! in the order of its distance from the first items. Up to one job per worker drains it:
//! each takes an item, visits it, and takes the next, until it finds the queue empty and
//! ends. Whoever pushes goes on draining, so an item pushed is never left: the queue is
//! empty once every job has ended. A job that sees more items waiting than it takes next
//! itself, and fewer jobs than workers, starts more, into the scope it opened
//! ([`par::scope`]); a job with no scope of its own opens one first. So the jobs follow the work: one for one item, every worker's for a wide
//! program, with nothing waiting for a level's last describe.
//!
//! Nothing here waits but a scope for the jobs it started, which is the pool's own wait (a
//! span naming the scope, its fiber parked on the scheduler). A job is never parked for an
//! item: it ends, and one that pushes later starts another.
//!
//! A visit whose wait gave way ([`blaze_graph::Cancelled`]: it would close a cycle through a
//! task that waits for this walk) is not the worklist's panic, as it is not a loop's
//! ([`blaze_graph::par`]): its item is set aside and visited again on the caller's thread
//! once the queue is empty, where the cycle is the caller's own. So a visit must be safe to
//! run again from its start. Any other panic of a visit stops the taking of items and is
//! raised on the caller once every job has ended (rule 20: it fails the caller's job).

#![forbid(unsafe_code)]

use std::{
    any::Any,
    panic::{AssertUnwindSafe, catch_unwind, resume_unwind},
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
    thread,
};

use blaze_core::lock::Lock;
use blaze_graph::{
    par::{self, Scope},
    pump,
};
use crossbeam_queue::SegQueue;

/// Items to visit, and the jobs that visit them.
pub(super) struct Worklist<T> {
    queue: SegQueue<T>,
    /// Items whose visit gave way, for the caller's thread.
    aside: SegQueue<T>,
    /// Jobs draining the queue now.
    draining: AtomicUsize,
    /// Set by [`Worklist::halt`] and by a visit's panic: no further item is taken.
    halted: AtomicBool,
    /// The first panic of a visit. Taken only when a visit panics and once as a run ends,
    /// never by a visit that returns.
    // dyn: a panic's payload, as `catch_unwind` gives it
    panic: Lock<Option<Box<dyn Any + Send>>>,
    /// The most jobs that drain at once: the pool's workers.
    most: usize,
    /// Visits begun, and jobs started beside the caller: what a test reads.
    visits: AtomicUsize,
    started: AtomicUsize,
}

impl<T: Send> Worklist<T> {
    /// An empty worklist on the pool a loop called here runs on.
    pub(super) fn new() -> Worklist<T> {
        Worklist::of(par::threads())
    }

    /// An empty worklist of at most `most` jobs at once.
    pub(super) fn of(most: usize) -> Worklist<T> {
        Worklist {
            queue: SegQueue::new(),
            aside: SegQueue::new(),
            draining: AtomicUsize::new(0),
            halted: AtomicBool::new(false),
            panic: Lock::new("codegen.worklist.panic", None),
            most: most.max(1),
            visits: AtomicUsize::new(0),
            started: AtomicUsize::new(0),
        }
    }

    /// One more item to visit: from the caller before a run, or from a visit.
    pub(super) fn push(&self, item: T) {
        self.queue.push(item);
    }

    /// No further item is taken: the visits under way end, and [`Worklist::run`] returns with
    /// what is left in the queue. For a build that stopped.
    pub(super) fn halt(&self) {
        self.halted.store(true, Ordering::Relaxed);
    }

    /// Whether [`Worklist::halt`] was called, or a visit panicked.
    pub(super) fn halted(&self) -> bool {
        self.halted.load(Ordering::Relaxed)
    }

    /// How many visits began, and how many jobs were started beside the caller's.
    #[cfg(test)]
    pub(super) fn counts(&self) -> (usize, usize) {
        (self.visits.load(Ordering::Relaxed), self.started.load(Ordering::Relaxed))
    }

    /// Visits every item, those pushed before the call and those the visits push, each once,
    /// on up to every worker at once; returns once the queue is empty and every visit has
    /// ended (or, after a [`Worklist::halt`], once the visits under way have).
    ///
    /// A visit that gave way is run again here, on the caller's thread, after the others; a
    /// rerun that gives way again unwinds to the caller, as a loop's does.
    ///
    /// # Panics
    ///
    /// With the first panic of a visit, once every job has ended.
    pub(super) fn run(&self, visit: &(impl Fn(&T) + Sync)) {
        loop {
            self.draining.fetch_add(1, Ordering::Relaxed);
            par::scope(|scope| {
                self.start(scope, visit);
                self.drain(Some(scope), visit);
                self.draining.fetch_sub(1, Ordering::Relaxed);
            });
            // Taken out before it is raised: no lock is held while the caller unwinds.
            let panic = self.panic.lock().take();
            if let Some(panic) = panic {
                resume_unwind(panic);
            }
            if self.halted() {
                return;
            }
            // Every job has ended, each having found the queue empty after its last push.
            debug_assert!(self.queue.is_empty(), "a worklist's jobs ended with items queued");
            let Some(item) = self.aside.pop() else { return };
            self.visits.fetch_add(1, Ordering::Relaxed);
            visit(&item);
        }
    }

    /// Takes items and visits them until the queue is empty. With `scope` (the job opened
    /// it, and its call returns to wait for it), more jobs are started into it while items
    /// wait and workers are free; without one, the job opens a scope of its own to start
    /// them, and goes on draining inside it.
    fn drain<'env>(&'env self, scope: Option<&Scope<'env>>, visit: &'env (impl Fn(&T) + Sync)) {
        while !self.halted() {
            let Some(item) = self.queue.pop() else { return };
            self.one(item, visit);
            if self.spare() == 0 {
                continue;
            }
            match scope {
                Some(scope) => self.start(scope, visit),
                None => {
                    // While this job waits for the jobs it started it drains nothing: it
                    // counts as draining only until its own drain inside the scope ends.
                    par::scope(|scope| {
                        self.start(scope, visit);
                        self.drain(Some(scope), visit);
                        self.draining.fetch_sub(1, Ordering::Relaxed);
                    });
                    self.draining.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
    }

    /// How many more jobs are worth starting: one for each item waiting past the one the
    /// asking job takes next itself, up to the workers no job of this worklist drains on. One
    /// load when every worker drains.
    fn spare(&self) -> usize {
        let free = self.most.saturating_sub(self.draining.load(Ordering::Relaxed));
        if free == 0 {
            return 0;
        }
        self.queue.len().saturating_sub(1).min(free)
    }

    /// Starts the jobs [`Self::spare`] counts. Two jobs that start more at once may start a
    /// few past the workers between them: each then shares the queue or finds it empty and
    /// ends, and none waits.
    fn start<'env>(&'env self, scope: &Scope<'env>, visit: &'env (impl Fn(&T) + Sync)) {
        for _ in 0..self.spare() {
            self.draining.fetch_add(1, Ordering::Relaxed);
            self.started.fetch_add(1, Ordering::Relaxed);
            scope.submit(
                None,
                move || {
                    self.drain(None, visit);
                    self.draining.fetch_sub(1, Ordering::Relaxed);
                },
                move |ran: thread::Result<()>| {
                    // A visit's panic is caught where it ran ([`Self::one`]): this is the
                    // job's own (the pool broke under it), and its count was not given back.
                    if let Err(panic) = ran {
                        self.draining.fetch_sub(1, Ordering::Relaxed);
                        self.failed(panic);
                    }
                },
            );
        }
    }

    /// One visit. Its give-way sets the item aside for the caller's thread; any other panic
    /// is the worklist's.
    fn one(&self, item: T, visit: &(impl Fn(&T) + Sync)) {
        self.visits.fetch_add(1, Ordering::Relaxed);
        let before = pump::gave_way();
        let Err(panic) = catch_unwind(AssertUnwindSafe(|| visit(&item))) else { return };
        if pump::gave_way() == before {
            self.failed(panic);
            return;
        }
        self.aside.push(item);
        // This job goes on to its next item, not the same ask: the give-way counts toward no
        // bound, however many items give way to one run (as in `par`'s loops).
        pump::rerun_later();
    }

    // dyn: a panic's payload, as `catch_unwind` gives it
    fn failed(&self, panic: Box<dyn Any + Send>) {
        self.halt();
        self.panic.lock().get_or_insert(panic);
    }
}

#[cfg(test)]
mod tests;
