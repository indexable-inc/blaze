//! Where a task runs: on the request's own thread, or on blaze-graph's worker pool, behind
//! blaze-keyed's [`Pool`]. No thread pool of this crate's own: blaze-graph's
//! ([`blaze_graph::pool::Pool`]: a deque per worker, stealing nearest first, its workers parked
//! on their own futex words) is the tree's one, and its [`Scope`] lets a job borrow the store's
//! mapped bytes and the front end for the request's life.
//!
//! A task is handed exactly its inputs: the tables the driver's
//! [`Effect::Run`](blaze_keyed::driver::Effect::Run) names ([`Source`]), each surface in place
//! (mapped from the store) or as the record this request wrote, shared, never copied per task;
//! each made table read once per manifest root and shared; and the request's late reads. The
//! task sees them as a [`Universe`], and answers every ask of its own by
//! [`Universe::answer`]: nothing else is readable.
//!
//! A task that panics fails that task only: its panic is caught ([`guarded`], on whichever
//! thread runs it) and answered as [`TaskFailed::Panicked`], naming the task's kind; the driver
//! fails the request with the unit, and the pool and the store go on.

use std::{
    alloc::Global,
    cell::Cell,
    marker::PhantomData,
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use blaze_core::wait::Parker;
use blaze_format::Aligned;
use blaze_graph::pool::Scope;
use blaze_keyed::{
    driver::{Task, TaskFailed},
    front::{Front, Keyed, Table, Unanswerable, Universe},
    machine::Qos,
    pool::{Pool, PoolFull, Ticket},
    record::{Fact, Owner},
};
use crossbeam_queue::SegQueue;

/// A surface's bytes as a task reads them.
#[derive(Clone, Debug)]
pub(crate) enum Bytes<'a> {
    /// In place in its mapped segment.
    Mapped(&'a [u8]),
    /// The record this request wrote, not yet committed (so not mapped): shared by every task
    /// that reads it.
    Written(Arc<Aligned, Global>),
}

impl Bytes<'_> {
    pub(crate) fn get(&self) -> &[u8] {
        match self {
            Bytes::Mapped(bytes) => bytes,
            Bytes::Written(record) => record,
        }
    }
}

/// A made table, read once from its manifest root and shared by every task and answer over it.
pub(crate) type Facts = Arc<Vec<Fact, Global>, Global>;

/// One table of a universe: a unit's surface and the facts its bodies made.
#[derive(Clone, Debug)]
pub(crate) struct Source<'a> {
    pub(crate) unit: u32,
    pub(crate) owner: Owner,
    pub(crate) surface: Bytes<'a>,
    /// From its manifest root; `None` for the asking unit's own, whose made facts come with the
    /// effect.
    pub(crate) made: Option<Facts>,
}

/// The tables of `sources` as the front end and the answerer take them: one small vector (a
/// pointer pair per table), each surface's fact table opened in place. `made` is the asking
/// unit's own made facts.
pub(crate) fn tables<'b>(sources: &'b [Source<'_>], made: &'b [Fact]) -> Result<Vec<Table<'b>, Global>, Unanswerable> {
    let mut tables = Vec::with_capacity_in(sources.len(), Global);
    for source in sources {
        let made = match &source.made {
            Some(facts) => &facts[..],
            None => made,
        };
        tables.push(Table::of(source.unit, source.owner, source.surface.get(), made)?);
    }
    Ok(tables)
}

/// What a task made.
pub(crate) enum Made {
    Keyed(Keyed<Global>),
    Wrote(Aligned),
}

/// What a task is called in a failure: its kind.
pub(crate) fn what_of(task: Task) -> &'static str {
    match task {
        Task::Key { .. } => "a unit's key task",
        Task::Surface { .. } => "a unit's surface task",
        Task::Body { .. } => "a body task",
    }
}

pub(crate) fn unit_of(task: Task) -> u32 {
    match task {
        Task::Key { unit } | Task::Surface { unit, .. } | Task::Body { unit, .. } => unit,
    }
}

/// A task as the pool takes it: the front end, the task, and exactly its inputs.
pub(crate) struct Job<'a, F> {
    pub(crate) front: &'a F,
    pub(crate) task: Task,
    /// Sorted by owner, as the driver named them.
    pub(crate) sources: Vec<Source<'a>, Global>,
    /// The asking unit's own made facts, sorted by id.
    pub(crate) made: Vec<Fact, Global>,
    /// The request's late reads, sorted by id.
    pub(crate) late: &'a [Fact],
}

/// Runs `job` with the front end over its universe, under a span naming it.
pub(crate) fn run_task<F: Front>(job: &Job<'_, F>) -> Result<Made, TaskFailed> {
    let task = job.task;
    let _span = tracing::info_span!(target: "blaze", "host: task", what = what_of(task), unit = unit_of(task)).entered();
    let tables = tables(&job.sources, &job.made).map_err(Unanswerable::failed)?;
    let universe = Universe { asker: unit_of(task), tables: &tables, outside: job.late };
    match task {
        Task::Key { unit } => job.front.key(unit, Global).map(Made::Keyed),
        Task::Surface { unit, key } => job.front.surface(unit, key, universe).map(Made::Wrote),
        Task::Body { unit, item, key } => job.front.body(unit, item, key, universe).map(Made::Wrote),
    }
}

/// [`run_task`], its panic caught and answered as the task's failure (rule 20).
pub(crate) fn guarded<F: Front>(job: &Job<'_, F>) -> Result<Made, TaskFailed> {
    match catch_unwind(AssertUnwindSafe(|| run_task(job))) {
        Ok(made) => made,
        Err(_panic) => Err(TaskFailed::Panicked { what: what_of(job.task) }),
    }
}

/// One finished task: its ticket, the task, what it made, and whether it was the last of its
/// job (a batch's tasks finish one by one; the worker is free after the last).
pub(crate) struct Finished {
    ticket: Ticket,
    task: Task,
    made: Result<Made, TaskFailed>,
    last: bool,
}

/// Where workers leave what they finished, and the request's thread's parker they wake.
pub(crate) struct Finish {
    done: SegQueue<Finished>,
    parker: Parker,
}

impl Finish {
    pub(crate) fn new() -> Finish {
        Finish { done: SegQueue::new(), parker: Parker::new() }
    }

    fn push(&self, finished: Finished) {
        self.done.push(finished);
        self.parker.unpark();
    }
}

/// blaze-graph's pool, as blaze-keyed's [`Pool`], within one request's scope. Used only from the
/// request's thread (the counts are its own); its jobs run on the pool's workers.
///
/// Workers start in proportion to queued tasks: the host makes the pool when a request first
/// has two or more tasks ready, with
/// [`Machine::workers_for`](blaze_keyed::machine::Machine::workers_for) threads, and lets as
/// many jobs run at once. A blaze-graph pool cannot add threads once made, so [`Pool::grow`]
/// raises nothing past them, and a request that queues more later runs them on those (the
/// missing piece: a pool that starts a worker when asked).
///
/// QoS: blaze-graph runs every worker at one class (`user-initiated` on macOS, by its rule that
/// a worker below it stalls a build), [`Qos::Interactive`]'s and [`Qos::Build`]'s both; a
/// request's pool is never asked [`Qos::Utility`].
pub(crate) struct Workers<'scope, 'env, F> {
    scope: &'scope Scope<'env>,
    finish: &'env Finish,
    threads: u32,
    allowed: u32,
    /// Jobs submitted and not yet finished: workers taken.
    jobs: Cell<u32>,
    of: PhantomData<&'env F>,
}

impl<'scope, 'env, F> Workers<'scope, 'env, F> {
    pub(crate) fn new(scope: &'scope Scope<'env>, finish: &'env Finish, threads: u32) -> Self {
        Workers { scope, finish, threads, allowed: threads.max(1), jobs: Cell::new(0), of: PhantomData }
    }

    /// Workers free for another job.
    pub(crate) fn free(&self) -> u32 {
        self.allowed.saturating_sub(self.jobs.get())
    }

    pub(crate) fn parker(&self) -> &Parker {
        &self.finish.parker
    }

    /// Whether a finished task is waiting to be taken.
    pub(crate) fn any_finished(&self) -> bool {
        !self.finish.done.is_empty()
    }
}

impl<'scope, 'env, F: Front + 'env> Pool for Workers<'scope, 'env, F> {
    type Task = Job<'env, F>;
    type Finished<'p>
        = (Task, Result<Made, TaskFailed>)
    where
        Self: 'p;

    fn workers(&self) -> u32 {
        self.allowed
    }

    fn grow(&mut self, more: u32, _qos: Qos) {
        self.allowed = self.allowed.saturating_add(more).min(self.threads.max(1));
    }

    /// One task, one job: no allocation beyond the pool's own job cell.
    fn submit(&self, ticket: Ticket, job: Job<'env, F>) -> Result<(), PoolFull> {
        if self.free() == 0 {
            return Err(PoolFull);
        }
        self.jobs.set(self.jobs.get() + 1);
        let finish = self.finish;
        let task = job.task;
        self.scope.submit(
            None,
            move || {
                let made = guarded(&job);
                finish.push(Finished { ticket, task: job.task, made, last: true });
            },
            move |ran| {
                // `guarded` caught the task's panic; a panic here is of the push itself, before
                // the answer was left: the task fails, so none is left unanswered.
                if ran.is_err() {
                    let made = Err(TaskFailed::Panicked { what: what_of(task) });
                    finish.push(Finished { ticket, task, made, last: true });
                }
            },
        );
        Ok(())
    }

    /// The tasks run in order on one worker, as one job: they take one worker.
    fn submit_batch<I: Iterator<Item = (Ticket, Job<'env, F>)>>(&self, tasks: I) -> Result<(), PoolFull> {
        if self.free() == 0 {
            return Err(PoolFull);
        }
        let mut batch = Vec::new_in(Global);
        batch.extend(tasks);
        if batch.is_empty() {
            return Ok(());
        }
        self.jobs.set(self.jobs.get() + 1);
        let finish = self.finish;
        // Which tasks are still to answer if the batch's own code panics: its tickets and tasks.
        let mut owed = Vec::with_capacity_in(batch.len(), Global);
        owed.extend(batch.iter().map(|(ticket, job): &(Ticket, Job<'env, F>)| (*ticket, job.task)));
        let answered = Arc::new_in(AtomicUsize::new(0), Global);
        let seen = Arc::clone(&answered);
        self.scope.submit(
            None,
            move || {
                let count = batch.len();
                for (at, (ticket, job)) in batch.into_iter().enumerate() {
                    let made = guarded(&job);
                    finish.push(Finished { ticket, task: job.task, made, last: at + 1 == count });
                    answered.store(at + 1, Ordering::Release);
                }
            },
            move |ran| {
                if ran.is_err() {
                    let count = owed.len();
                    let from = seen.load(Ordering::Acquire);
                    for (at, (ticket, task)) in owed.into_iter().enumerate().skip(from) {
                        let made = Err(TaskFailed::Panicked { what: what_of(task) });
                        finish.push(Finished { ticket, task, made, last: at + 1 == count });
                    }
                }
            },
        );
        Ok(())
    }

    fn finished(&self) -> Option<(Ticket, Self::Finished<'_>)> {
        let finished = self.finish.done.pop()?;
        if finished.last {
            self.jobs.set(self.jobs.get() - 1);
        }
        Some((finished.ticket, (finished.task, finished.made)))
    }
}
