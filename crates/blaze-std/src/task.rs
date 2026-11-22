//! Structured tasks (docs/v2/plan/design/std.md, "Tasks and waiting"): a scope is a value.
//!
//! ```text
//! let scope = cx.scope();
//! let a = scope.may_overlap("a", TASK_FRAME, |cx| read(cx, &dir));
//! let b = scope.may_overlap("b", TASK_FRAME, |cx| read(cx, &dir));
//! let (a, b) = (a.join(cx), b.join(cx));
//! scope.join(cx)?;
//! ```
//!
//! There is no `Tasks` capability: starting work is something a [`Cx`] does. Each task gets its
//! own `Cx`, borrowed for the task's run so it cannot outlive the scope: its name, its own
//! frame of the size its spawn names (`scratch`: the most bytes the task's frame hands out;
//! [`crate::mem::TASK_FRAME`] is the usual value, said at the call), the scope's deadline and
//! cancellation. A task's closure may borrow from the frame
//! that holds the scope. A task's error or panic cancels the scope; a task learns it only as
//! [`Stopped::Cancelled`] from its next wait, never by being dropped. [`Task::join`] gives a
//! task's own outcome, [`Scope::join`] the scope's first failure, typed. [`Cx::scope_with`] is
//! the closure form.
//!
//! **Linear.** [`Scope`] and [`Task`] are `#[blaze::linear]` (crates/hir-ty-linear): leaving
//! either unjoined is a compile error, since a scope's tasks must have ended and its failure
//! been seen before the frame holding it goes on. A [`Task`] borrows its scope, so it is
//! joined before the scope is. A panic that unwinds past a scope runs its abandonment path,
//! its `Drop`: it cancels the scope, then waits, bounded by [`crate::io::Limits::abandon_wait`], for its tasks to
//! end, and records how ([`Settled`]) in the trace. The scope holds its tasks' state itself, so
//! a `Task` is not put in a `Vec` or an `Option` (the checker refuses a linear value in a type
//! argument of a type that is not marked for it): each is joined where it was spawned.
//!
//! **A started task never changes its thread** (std.md, "Layouts"; docs/v2/plan/design/bottom.md,
//! "Fibers and `Send`"). rustc checks `Send` across an `.await`, not across a fiber's wait: a
//! task that holds an `Rc`, a lock guard or a thread-local borrow, waits and resumes on another
//! thread is a data race rustc accepts. So, as rustc builds bstd: a new task may be placed on
//! any worker ([`Scope::may_overlap`] places it, and [`crate::par`] spreads its chunks that way), and
//! once it has started, every wait it resumes from resumes it there. Under `Inline` that holds
//! by construction (a task runs on its spawner's thread to its end); a debug build checks it at
//! every resume, and the simulation test `placement` holds a planted migration against it. The
//! contract of the blaze form is different and is not code: built by blaze, a task may move at
//! a wait, because blaze refuses, at compile time, a value that is not `Send` live across a call
//! that can wait (rule 27).
//!
//! **May overlap, or must** (the distinction is Zig's `std.Io`: `async` against `concurrent`).
//! There are two ways to start a task, and the one chosen says what the code's correctness
//! rests on:
//!
//! - [`Scope::may_overlap`]: the task may run later than its spawn, or at the same time as
//!   what follows it, and the code is correct either way. With no worker free it runs inline,
//!   to completion, at its spawn. Almost every task is this one: `par`'s chunks, independent
//!   reads, anything joined before its result is used.
//! - [`Scope::must_overlap`]: the code is correct only if the task and its spawner make
//!   progress at once: a producer and a consumer of one bounded channel, a server and the
//!   client that talks to it. Run inline, the first full or empty wait would never be answered
//!   and would end only at its deadline; so where the scope cannot give the task a worker of
//!   its own, `must_overlap` does not run it and returns [`NoParallelism`] at once, before
//!   anything waits.
//!
//! How many tasks can run at once is the scope's [`Scope::workers`]: the layout's
//! ([`Layout::workers`]) narrowed by the scope's budget ([`Scope::narrow_workers`]: a test
//! runner narrowed to one thread proves code against the one-thread executor).
//!
//! **Layout** (std.md, "Layouts"), chosen by the root a context comes from:
//!
//! - `Inline` ([`LAYOUT`]), a root made by `Io::cx` (the simulation's, a test's): one worker.
//!   [`Scope::may_overlap`] runs its task to completion before it returns, in spawn order, so
//!   nothing else runs while a task waits, and [`Scope::must_overlap`] always returns
//!   [`NoParallelism`] `{ needed: 2, have: 1 }`.
//! - `Threads` (task/threads.rs), the root `crate::entry::run` gives `main`: a thread-per-core
//!   pool. A `may_overlap` task is queued on the spawner's worker (or an injection queue from
//!   outside the pool) and taken by whichever worker starts it, which keeps it to its end; a
//!   full queue, or a scope narrowed to one worker, runs it inline as under `Inline`. Its
//!   joiner runs it itself if no worker started it. A `must_overlap` task is handed to a worker
//!   that is idle now; with none, it is refused as under `Inline`. A task's context crosses to
//!   its worker as a `Seed` (crate::cx), so both spawns take `Send` closures and results in
//!   every layout: code proven under `Inline` runs unchanged on the pool.
//!
//! A scope's failure is the failure of its lowest spawn index under both, the one `Inline`
//! meets first, so a scope fails the same whatever ran first.
//!
//! **A scope never ends while a task of it runs.** Its tasks borrow from its frame (as
//! `std::thread::scope`'s do), so under `Threads` [`Task::join`] waits for a started task,
//! until the context's deadline and then [`crate::io::Limits::abandon_wait`] more with the
//! scope cancelled; a task still running then is a hang (rule 18), and the process stops,
//! naming it, rather than free what it borrows.

mod claim;
mod deque;
mod help;
mod idle;
#[expect(unsafe_code, reason = "per-worker slots handed to the worker that owns them: each `unsafe` states why")]
mod per_worker;
#[expect(unsafe_code, reason = "a batch's ring written by the tasks that end into it: each `unsafe` states its invariant")]
mod ring;
#[expect(unsafe_code, reason = "a running task's name read back from its pointer and length: the `unsafe` states why")]
mod seat;
#[expect(unsafe_code, reason = "type-erased tasks shared with the pool's workers: each `unsafe` states its invariant")]
pub(crate) mod threads;
#[cfg(all(test, not(blaze_check)))]
mod tests;

pub use help::{Batch, BatchError, JoinCounts, Joined};
pub use per_worker::{Exclusive, FoldError, NotAWorker, PerWorker, Racy, Shared};
pub use threads::{Class, Hung, Stuck};

use alloc::{
    alloc::Global,
    borrow::ToOwned,
    boxed::Box as Payload,
    string::{String, ToString},
};
use core::{
    any::Any,
    cell::{Cell, RefCell},
    fmt,
    marker::PhantomData,
    num::NonZeroU32,
    panic::AssertUnwindSafe,
};

use blaze_core::wait::{Clock, Deadline};
use blaze_mem::Frame;
use blaze_own::Arc;

use crate::{
    Plant, Stopped,
    cx::{Cx, Here},
    host::Exit,
    rt::{catch_unwind, resume_unwind},
    unfinished::Unfinished,
};
use claim::Start;
use threads::{Full, Pool, Queued, Ran, Tally};

/// How a waiting task's frames are stored (std.md, "Layouts").
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Layout {
    /// A task runs to completion at its spawn; nothing else runs while it waits.
    Inline,
    /// A thread-per-core pool of `workers` (task/threads.rs): a task runs on one worker, from
    /// its start to its end, and blocks that worker's thread while it waits.
    Threads { workers: u32 },
}

impl Layout {
    /// How many tasks it runs at once: what [`Scope::must_overlap`] asks of it.
    #[blaze::pure]
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub const fn workers(self) -> u32 {
        match self {
            Layout::Inline => 1,
            Layout::Threads { workers } => workers,
        }
    }
}

/// The layout of a context with no pool: a root made by `Io::cx`, and the simulation's.
pub const LAYOUT: Layout = Layout::Inline;

/// [`Scope::must_overlap`] was asked for a task that must run at the same time as its spawner,
/// and the scope has fewer workers than that needs: the task never ran. Nothing waited first,
/// so a code path that is correct only with parallelism fails here, at once, instead of
/// waiting out its deadline on a wait nobody else can answer.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct NoParallelism {
    /// The scope's task (its span).
    pub scope: &'static str,
    /// The task that was not run.
    pub task: &'static str,
    /// How many tasks must make progress at once: the task and its spawner.
    pub needed: u32,
    /// How many the scope can run at once ([`Scope::workers`]).
    pub have: u32,
    /// The layout that gave `have`, before the scope's budget narrowed it.
    pub layout: Layout,
}

impl fmt::Display for NoParallelism {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let NoParallelism { scope, task, needed, have, layout } = *self;
        write!(
            f,
            "bstd::task {task} of a scope in {scope}: must_overlap needs {needed} tasks running at once, and the \
             scope has {have} worker(s) ({layout:?} layout, narrowed by the scope's budget); it never ran. Start \
             it with may_overlap only if it is correct when run inline"
        )
    }
}

impl core::error::Error for NoParallelism {}

/// How a task failed.
#[derive(Clone, PartialEq, Eq, Debug)] // clone: an error
pub enum TaskWhy {
    /// It returned this error.
    Failed(String),
    /// It panicked, saying this.
    Panicked(String),
    /// Its frame could not be made.
    OutOfMemory(Stopped),
    /// The scope was joined with this many tasks still running, after its wait for them
    /// stopped so.
    Unended { running: usize, stopped: Stopped },
}

/// The first task of a scope that failed: the scope (its task's span), the task and how.
#[derive(Clone, PartialEq, Eq, Debug)] // clone: an error
pub struct TaskError {
    pub scope: &'static str,
    pub task: &'static str,
    pub why: TaskWhy,
}

impl fmt::Display for TaskError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (scope, task) = (self.scope, self.task);
        match &self.why {
            TaskWhy::Failed(said) => write!(f, "bstd::task {task} of a scope in {scope} failed: {said}"),
            TaskWhy::Panicked(said) => {
                write!(f, "bstd::task {task} of a scope in {scope} panicked: {said} (rule panics-fail-one-job)")
            }
            TaskWhy::OutOfMemory(stopped) => {
                write!(f, "bstd::task {task} of a scope in {scope} got no frame: {stopped}")
            }
            TaskWhy::Unended { running, stopped } => write!(
                f,
                "bstd::task the scope in {scope} was joined with {running} tasks still running (the last \
                 spawned is {task}): {stopped}"
            ),
        }
    }
}

impl core::error::Error for TaskError {}

/// How a scope's wait for its tasks ended: at its join, or when a panic dropped it.
#[derive(Clone, PartialEq, Eq, Debug)] // clone: a stop
pub enum Settled {
    /// Every task had ended.
    All,
    /// `running` tasks had not, when the wait stopped so.
    Left { running: usize, stopped: Stopped },
}

/// What a scope keeps of its tasks while they run, and its abandonment path.
struct Live {
    cx: Cx,
    /// The first failure, under `Inline`.
    failed: RefCell<Option<TaskError>>,
    /// What the scope shares with the workers that run its tasks: under `Threads` only. One
    /// `Arc` per scope, in `Global`: it outlives no task of the scope but is shared by them.
    shared: Option<Arc<Tally, Global>>,
    /// The scope's class: what its tasks and the scopes in them are queued as.
    class: Cell<threads::Class>,
    /// The layout its context gave it.
    layout: Layout,
    /// Tasks run inline and not yet ended (under `Threads`, those queued are its tally's).
    running: Cell<usize>,
    /// The task spawned last: who an [`TaskWhy::Unended`] names.
    last: Cell<&'static str>,
    /// Tasks spawned so far: the next one is placed on the worker after the scope's own plus
    /// this many (the model of a pool's spread under `Inline`).
    placed: Cell<u32>,
    /// The most tasks the scope runs at once: the layout's, narrowed by
    /// [`Scope::narrow_workers`], never widened.
    workers: Cell<u32>,
    /// Whether the scope is still this value's to end: cleared by [`Scope::join`].
    armed: bool,
}

impl Live {
    /// Waits until `until` for the scope's tasks to end. Under `Inline` each task ended at its
    /// spawn, so none is ever left. Under `Threads` every task's handle waited for it already
    /// ([`Task::join`] or its drop), so this waits only for the count to reach zero; a task
    /// still running past `until` and the abandon wait stops the process (the module's doc).
    fn settle(&self, until: Deadline) -> Settled {
        if let (Some(pool), Some(tally)) = (self.cx.pool(), &self.shared) {
            outlast(&self.cx, pool, tally, self.last.get(), until, || tally.running() == 0);
        }
        let running = self.running.get();
        match running {
            0 => Settled::All,
            running => {
                let stopped = Stopped::Deadline { span: self.cx.name(), at: until.get() };
                Settled::Left { running, stopped }
            }
        }
    }

    /// Task `index`, `task`, failed so: the scope is cancelled, and the failure of the lowest
    /// index kept (under `Inline` that is the first).
    fn fail(&self, index: u32, task: &'static str, why: TaskWhy) -> TaskError {
        self.cx.cancel();
        let error = TaskError { scope: self.cx.name(), task, why };
        if let Some(tally) = &self.shared {
            // clone: an error, kept for the scope and returned to its task.
            tally.fail(index, error.clone());
            return error;
        }
        let mut failed = self.failed.borrow_mut();
        if failed.is_none() {
            // clone: an error, kept for the scope and returned to its task.
            *failed = Some(error.clone());
        }
        error
    }

    /// The scope's kept failure, taken.
    fn take_failed(&self) -> Option<TaskError> {
        match &self.shared {
            Some(tally) => tally.take_failed().map(|(_, error)| error),
            None => self.failed.borrow_mut().take(),
        }
    }
}

/// Waits on `cx`'s thread until `ready`: until `until` on the world's clock, then, with the
/// scope cancelled, [`crate::io::Limits::abandon_wait`] more on the pool's. Past that, what it
/// waits for is a task still running that may borrow the frame the waiter is about to leave:
/// a hang, and the process stops naming `task` (rule 18; the module's doc).
fn outlast(cx: &Cx, pool: &Pool, tally: &Tally, task: &'static str, until: Deadline, ready: impl FnMut() -> bool) {
    // The join's time is charged to the joiner's worker under the task it waited on (D171).
    cx.charged(cx.io(), task, None, || outlast_now(cx, pool, tally, task, until, ready));
}

fn outlast_now(cx: &Cx, pool: &Pool, tally: &Tally, task: &'static str, until: Deadline, mut ready: impl FnMut() -> bool) {
    let io = cx.io();
    let waited = pool.wait(cx.here(), tally, "bstd.task.join", &mut ready, || until.passed(io.now()));
    match waited {
        Ok(true) => return,
        Ok(false) => {}
        Err(why) => panic!("bstd::task {task} of a scope in {}: the join's park failed: {why}", cx.name()),
    }
    tally.cancel();
    let clock = *pool.clock();
    let grace = Deadline::after(clock.now(), io.limits().abandon_wait);
    match pool.wait(cx.here(), tally, "bstd.task.join.abandon", &mut ready, || grace.passed(clock.now())) {
        Ok(true) => {}
        Ok(false) | Err(_) => crate::rt::hang(cx.name(), task),
    }
}

/// How a task's closure ended, short of a result.
enum Ending {
    Why(TaskWhy),
    /// The process exits: passed on.
    Exit(Payload<dyn Any + Send>),
}

/// A task's closure's outcome, its error said and a panic's payload read.
fn ended<T, E: fmt::Display>(ran: Result<Result<T, E>, Payload<dyn Any + Send>>) -> Result<T, Ending> {
    match ran {
        Ok(Ok(made)) => Ok(made),
        // alloc: an error's message.
        Ok(Err(error)) => Err(Ending::Why(TaskWhy::Failed(error.to_string()))),
        Err(payload) => match payload.downcast::<Exit>() {
            Ok(exit) => Err(Ending::Exit(exit)),
            Err(payload) => Err(Ending::Why(TaskWhy::Panicked(said(&*payload)))),
        },
    }
}

impl Drop for Live {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        self.cx.cancel();
        let until = Deadline::after(self.cx.io().now(), self.cx.io().limits().abandon_wait);
        let what = match self.settle(until) {
            Settled::All => "scope: abandoned, its tasks ended",
            Settled::Left { .. } => "scope: abandoned, tasks still running",
        };
        self.cx.event(what);
    }
}

/// A scope of tasks, from [`Cx::scope`]: where its tasks are spawned. It borrows the context it
/// was made from, so nothing of it outlives that. Finished by [`Scope::join`] (see the
/// module's doc for why it is linear and what a panic that drops it does).
#[blaze::linear]
#[must_use = "a scope is finished with join(cx)"]
pub struct Scope<'p> {
    live: Live,
    parent: PhantomData<&'p Cx>,
    unfinished: Unfinished,
}

/// One task of a scope, from [`Scope::may_overlap`] or [`Scope::must_overlap`]: finished by [`Task::join`], before its scope
/// is. Linear for the scope's reason; dropping one in a panic drops its outcome, which its
/// scope has seen already.
#[blaze::linear]
#[must_use = "a task is finished with join(cx)"]
pub struct Task<'s, T> {
    name: &'static str,
    held: Held<T>,
    scope: PhantomData<&'s ()>,
    /// Not `Send`: it is joined on its scope's thread, the one that parks on the scope.
    thread: PhantomData<*const ()>,
    unfinished: Unfinished,
}

/// What a task's handle holds.
enum Held<T> {
    /// It ran inline (or never ran): its outcome.
    Ended(Result<T, TaskError>),
    /// It is queued on the pool, or running there.
    Queued(Pending<T>),
}

/// A task on the pool, and what its handle needs to wait for it.
struct Pending<T> {
    queued: Queued<T>,
    pool: Pool,
    /// The scope's thread, its name and its context's deadline: what a wait in the handle's
    /// drop is bounded by.
    here: Here,
    scope: &'static str,
    deadline: Deadline,
    world: crate::io::Io,
}

impl<T> Pending<T> {
    /// Its outcome: run here if no worker started it, else waited for.
    fn finish(&self, cx: &Cx, name: &'static str) -> Ran<T> {
        // Help first: while the task has not ended, this thread runs queued tasks it may run,
        // and parks (`outlast`) only when there is none (task/help.rs, the module's rule).
        help::finish(self, cx, name)
    }
}

impl<T> Drop for Pending<T> {
    /// A handle dropped unjoined (a panic unwinds past it): an unstarted task is dropped unrun;
    /// a started one is waited for, since it may borrow the frame the unwind leaves, and what it
    /// made is dropped here, while what it borrows still lives.
    fn drop(&mut self) {
        if self.queued.drop_unstarted() {
            return;
        }
        let clock = *self.pool.clock();
        let until = Deadline::after(clock.now(), self.world.limits().abandon_wait);
        let world = &self.world;
        let deadline = self.deadline;
        let tally = self.queued.tally();
        let ready = || self.queued.done();
        match self.pool.wait(self.here, tally, "bstd.task.drop", ready, || deadline.passed(world.now()) || until.passed(clock.now())) {
            // A joined handle's result was taken already; an unjoined one's is dropped now.
            Ok(true) => self.queued.drop_out(),
            Ok(false) | Err(_) => crate::rt::hang(self.scope, self.queued.name()),
        }
    }
}

impl<'p> Scope<'p> {
    /// The scope whose context is `cx`, from [`Cx::scope`].
    #[track_caller]
    pub(crate) fn new(cx: Cx) -> Scope<'p> {
        let layout = cx.layout();
        let shared = cx.pool().map(|_| Tally::new(cx.canceller()));
        let class = cx.class();
        let live = Live {
            cx,
            failed: RefCell::new(None),
            shared,
            class: Cell::new(class),
            layout,
            running: Cell::new(0),
            last: Cell::new(""),
            placed: Cell::new(0),
            workers: Cell::new(layout.workers()),
            armed: true,
        };
        Scope { live, parent: PhantomData, unfinished: Unfinished::new("task::Scope", "join(cx)") }
    }

    /// A task's handle. `#[track_caller]` down from the spawn, so an unfinished task names the
    /// line that spawned it (`crate::unfinished`).
    #[track_caller]
    fn handle<T>(name: &'static str, held: Held<T>) -> Task<'_, T> {
        Task { name, held, scope: PhantomData, thread: PhantomData, unfinished: Unfinished::new("task::Task", "join(cx)") }
    }

    /// The next spawn's index and frame: `Err` with the task's handle when no frame can be had.
    #[track_caller]
    fn prepare<T>(&self, name: &'static str, scratch: usize) -> Result<(u32, Frame), Task<'_, T>> {
        let live = &self.live;
        live.last.set(name);
        let index = live.placed.get() + 1;
        live.placed.set(index);
        match Frame::with_reserve(scratch, blaze_mem::Tag::Frames) {
            Ok(frame) => Ok((index, frame)),
            Err(blaze_mem::MemError::Reserve { .. } | blaze_mem::MemError::Commit { .. } | blaze_mem::MemError::Exhausted { .. } | blaze_mem::MemError::TooLarge { .. } | blaze_mem::MemError::Evict { .. }) => {
                let stopped = Stopped::OutOfMemory { arena: name, asked: scratch };
                let error = live.fail(index, name, TaskWhy::OutOfMemory(stopped));
                Err(Scope::handle(name, Held::Ended(Err(error))))
            }
        }
    }

    /// A job for `task` on the pool, its context grown from a seed on the thread that runs it.
    #[expect(unsafe_code, reason = "erasing the task's borrows to the scope's lifetime, as std::thread::scope does: the SAFETY below")]
    fn pool_job<T: Send, E: fmt::Display>(
        &self,
        tally: &Arc<Tally, Global>,
        name: &'static str,
        index: u32,
        frame: Frame,
        start: Start,
        task: impl FnOnce(&Cx) -> Result<T, E> + Send,
    ) -> (Queued<T>, threads::JobPtr) {
        let live = &self.live;
        let worker = live.cx.worker().wrapping_add(index);
        let seed = live.cx.seed(name, frame, live.class.get(), worker);
        let scope = live.cx.name();
        let failures = Arc::clone(tally);
        let body = move |here: Here| -> Ran<T> {
            // The task's name on its thread while it runs: what a lock it holds leaves for a
            // waiter at a release someone waited for (blaze-core's lock record, D179). Two stores
            // a task; a child run inline keeps its parent's name.
            let was = blaze_meter::set_doing(name);
            let cx = seed.grow(here);
            // The telemetry's task hooks (decision 2): the worker's cell names this task as its
            // current one until it ends, so its waits' records carry its id.
            let begun = cx.task_began(name);
            let ran = catch_unwind(AssertUnwindSafe(|| task(&cx)));
            cx.task_ended(begun, !matches!(ran, Ok(Ok(_))));
            drop(cx);
            blaze_meter::set_doing(was);
            match ended(ran) {
                Ok(made) => Ran::Made(made),
                Err(Ending::Exit(exit)) => Ran::Exit(exit),
                Err(Ending::Why(why)) => {
                    let error = TaskError { scope, task: name, why };
                    // clone: an error, kept for the scope and returned to its task.
                    failures.fail(index, error.clone());
                    Ran::Failed(error)
                }
            }
        };
        // SAFETY: the job's body borrows no longer than the scope, and the scope does not end
        // while the job is queued or running: its handle runs it, waits for it or drops it
        // unrun (`Pending`), and a task that will not end stops the process (`outlast`).
        unsafe { threads::job(name, tally, start, body) }
    }

    /// A pool task's outcome, as its handle gives it.
    fn outcome<T>(ran: Ran<T>) -> Result<T, TaskError> {
        match ran {
            Ran::Made(made) => Ok(made),
            Ran::Failed(error) => Err(error),
            Ran::Exit(exit) => resume_unwind(exit),
        }
    }

    /// Runs `task` as the task `name` of this scope, which may overlap what follows it or not:
    /// the code must be correct either way (the module's doc). With no worker free it runs
    /// inline, now, to completion: always under `Inline`, so siblings run in spawn order. An
    /// error it returns, or a panic, cancels the scope: the next wait of each task after it
    /// ends with [`Stopped::Cancelled`], and the scope fails with the first. A `Task` value of
    /// a process's exit is never made: the exit unwinds on.
    ///
    /// `scratch` is the most bytes the task's frame (`cx.frame()`) hands out: an allocation
    /// past it is the task's [`Stopped::OutOfMemory`], naming the task. It is reserved as
    /// address space here and committed as it is used.
    ///
    /// Under `Threads` the task is queued (its spawner's worker's own queue, or an injection
    /// queue from outside the pool) and runs on whichever worker starts it, or on its joiner if
    /// none has; it runs inline as under `Inline` when the scope is narrowed to one worker or
    /// the queue is full (a bound of [`threads::DEQUE`] per worker and [`threads::INJECT`] per
    /// group: a spawn never waits for room and never grows a queue). The closure and its result
    /// are `Send` in every layout, so code proven under `Inline` runs unchanged on the pool.
    #[track_caller]
    pub fn may_overlap<T: Send, E: fmt::Display>(
        &self,
        name: &'static str,
        scratch: usize,
        task: impl FnOnce(&Cx) -> Result<T, E> + Send,
    ) -> Task<'_, T> {
        let live = &self.live;
        let (index, frame) = match self.prepare(name, scratch) {
            Ok(prepared) => prepared,
            Err(failed) => return failed,
        };
        if let (Some(pool), Some(tally)) = (live.cx.pool(), &live.shared)
            && self.workers() > 1
        {
            let (queued, job) = self.pool_job(tally, name, index, frame, Start::Open, task);
            let here = live.cx.here();
            if let Err(Full(job)) = pool.spawn(here, live.class.get(), job) {
                pool.run_now(here, job);
            }
            return Scope::handle(name, Held::Queued(self.pending(pool, queued)));
        }
        // A new task may go to any worker; once started it stays there (the module's doc).
        let cx = live.cx.task(name, frame, live.cx.worker().wrapping_add(index)).with_class(live.class.get());
        live.running.set(live.running.get() + 1);
        let begun = cx.task_began(name);
        let ran = catch_unwind(AssertUnwindSafe(|| task(&cx)));
        cx.task_ended(begun, !matches!(ran, Ok(Ok(_))));
        live.running.set(live.running.get() - 1);
        drop(cx);
        let outcome = match ended(ran) {
            Ok(made) => Ok(made),
            // The process exits: not this task's failure, so it is passed on.
            Err(Ending::Exit(exit)) => resume_unwind(exit),
            Err(Ending::Why(why)) => Err(live.fail(index, name, why)),
        };
        Scope::handle(name, Held::Ended(outcome))
    }

    /// How many L3-cache / core-class worker groups this scope's pool has (1 under `Inline` or
    /// when narrowed to a single worker). Used by [`crate::par::by_group`] to keep related work
    /// (e.g. a crate or module shard) hot inside one L3 cache cluster.
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn groups(&self) -> u32 {
        if self.workers() <= 1 {
            return 1;
        }
        self.live.cx.pool().map_or(1, Pool::groups)
    }

    /// Like [`Scope::may_overlap`], steering the task onto L3-cache `group` (modulo
    /// [`Scope::groups`]) so the task and any sub-tasks it spawns stay warm inside one L3/CCX
    /// cluster on multi-cluster CPUs (EPYC, Graviton, Apple Silicon).
    #[track_caller]
    pub fn may_overlap_in_group<T: Send, E: fmt::Display>(
        &self,
        group: u32,
        name: &'static str,
        scratch: usize,
        task: impl FnOnce(&Cx) -> Result<T, E> + Send,
    ) -> Task<'_, T> {
        let live = &self.live;
        let (index, frame) = match self.prepare(name, scratch) {
            Ok(prepared) => prepared,
            Err(failed) => return failed,
        };
        if let (Some(pool), Some(tally)) = (live.cx.pool(), &live.shared)
            && self.workers() > 1
        {
            let (queued, job) = self.pool_job(tally, name, index, frame, Start::Open, task);
            let here = live.cx.here();
            if let Err(Full(job)) = pool.spawn_in_group(group, live.class.get(), job) {
                pool.run_now(here, job);
            }
            return Scope::handle(name, Held::Queued(self.pending(pool, queued)));
        }
        let cx = live.cx.task(name, frame, live.cx.worker().wrapping_add(index)).with_class(live.class.get());
        live.running.set(live.running.get() + 1);
        let begun = cx.task_began(name);
        let ran = catch_unwind(AssertUnwindSafe(|| task(&cx)));
        cx.task_ended(begun, !matches!(ran, Ok(Ok(_))));
        live.running.set(live.running.get() - 1);
        drop(cx);
        let outcome = match ended(ran) {
            Ok(made) => Ok(made),
            Err(Ending::Exit(exit)) => resume_unwind(exit),
            Err(Ending::Why(why)) => Err(live.fail(index, name, why)),
        };
        Scope::handle(name, Held::Ended(outcome))
    }

    /// A queued task's handle state.
    fn pending<T>(&self, pool: &Pool, queued: Queued<T>) -> Pending<T> {
        let cx = &self.live.cx;
        Pending {
            queued,
            pool: pool.clone(),
            here: cx.here(),
            scope: cx.name(),
            deadline: cx.deadline(),
            world: cx.io().clone(),
        }
    }

    /// Runs `task` as the task `name` of this scope, at the same time as what follows it: for
    /// code that is correct only if both make progress at once (the module's doc). Where the
    /// scope has fewer than the two workers that needs ([`Scope::workers`]), the task does not
    /// run, no frame is made for it, nothing waits, and the answer is [`NoParallelism`]; the
    /// scope is not failed by it, so the caller says what follows (usually `?`). Always so
    /// under `Inline`, which has one worker. `scratch` is the task's frame, as
    /// [`Scope::may_overlap`]'s.
    ///
    /// Under `Threads` the task is handed to a worker that is idle now (searching or parked),
    /// its spawner's group first, which starts it before anything else. It is refused the
    /// same way when no worker is idle at that moment, or when the scope already has
    /// `workers - 1` tasks out (its budget, the spawner being one): a spawn never waits for a
    /// worker.
    #[track_caller]
    pub fn must_overlap<T: Send, E: fmt::Display>(
        &self,
        name: &'static str,
        scratch: usize,
        task: impl FnOnce(&Cx) -> Result<T, E> + Send,
    ) -> Result<Task<'_, T>, NoParallelism> {
        let live = &self.live;
        // The task and its spawner.
        let needed = 2;
        let have = self.workers();
        let refused = NoParallelism { scope: live.cx.name(), task: name, needed, have, layout: live.layout };
        let (Some(pool), Some(tally)) = (live.cx.pool(), &live.shared) else {
            return Err(NoParallelism { have: have.min(1), ..refused });
        };
        let out = u32::try_from(tally.running()).unwrap_or(u32::MAX);
        if have < needed || out.saturating_add(needed) > have {
            return Err(NoParallelism { have: have.saturating_sub(out).min(have), ..refused });
        }
        let (index, frame) = match self.prepare(name, scratch) {
            Ok(prepared) => prepared,
            Err(failed) => return Ok(failed),
        };
        // Reserved: only the worker it is handed to may start it, never its joiner, so it runs
        // beside its spawner however soon the join comes (task/claim.rs).
        let (queued, job) = self.pool_job(tally, name, index, frame, Start::Reserved, task);
        match pool.reserve(live.cx.here(), job) {
            Ok(_) => Ok(Scope::handle(name, Held::Queued(self.pending(pool, queued)))),
            Err(job) => {
                // No worker idle: the task never runs. It was never queued, so this thread
                // withdraws it and drops it unrun, then gives up both references.
                queued.withdraw();
                job.release_unrun();
                drop(queued);
                Err(NoParallelism { have: 1, ..refused })
            }
        }
    }

    /// How many of its tasks the scope runs at once: the layout's [`Layout::workers`], narrowed
    /// by [`Scope::narrow_workers`]. One under `Inline`.
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn workers(&self) -> u32 {
        self.live.workers.get()
    }

    /// Lets the scope run at most `most` of its tasks at once from now on, as `class`; both
    /// only narrow. A scope narrowed to one runs as the one-thread executor does: every
    /// [`Scope::may_overlap`] inline and in order, every [`Scope::must_overlap`] refused. A
    /// scope narrowed to [`Class::Background`] queues its tasks, and the scopes in them, on the
    /// pool's background queues: on a background worker (macOS: an efficiency core, at utility
    /// QoS), or on a foreground worker only when it sees no foreground work.
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn narrow_workers(&self, most: NonZeroU32, class: Class) {
        self.live.workers.set(self.live.workers.get().min(most.get()));
        if class == Class::Background {
            self.live.class.set(Class::Background);
        }
    }

    /// Cancels the scope: the next wait of each task under it ends with
    /// [`Stopped::Cancelled`].
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn cancel(&self) {
        self.live.cx.cancel();
    }

    /// The scope's own context.
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn cx(&self) -> &Cx {
        &self.live.cx
    }

    /// How many of its tasks are running (or, under `Threads`, queued) now: none, under
    /// `Inline`, outside a task.
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn running(&self) -> usize {
        let queued = self.live.shared.as_ref().map_or(0, |tally| tally.running());
        self.live.running.get() + queued
    }

    /// The scope finished: once each of its tasks has ended (waited for until `cx`'s
    /// deadline), its first failure, or [`TaskWhy::Unended`] if tasks were still running when
    /// that wait stopped.
    #[blaze::consumes]
    pub fn join(mut self, cx: &Cx) -> Result<(), TaskError> {
        self.unfinished.finished();
        self.live.armed = false;
        let settled = self.live.settle(cx.deadline());
        let failed = self.live.take_failed();
        match settled {
            Settled::Left { running, stopped } => Err(TaskError {
                scope: self.live.cx.name(),
                task: self.live.last.get(),
                why: TaskWhy::Unended { running, stopped },
            }),
            Settled::All => match (failed, cx.plant()) {
                (Some(_), Plant::JoinDropsFailure) | (None, Plant::None | Plant::SubdirKeepsParent | Plant::OpenCreates | Plant::LexicalParent | Plant::WaitIgnoresDeadline | Plant::UntilWidens | Plant::WaitIgnoresCancel | Plant::ArenaUnlimited | Plant::SyncSkipped | Plant::EnvOnlyReplaces | Plant::CancelSkipsWait | Plant::CloseSkipsSync | Plant::JoinDropsFailure | Plant::OffersIgnored | Plant::OnlyWidens | Plant::KvPutUnsynced | Plant::LocalDropsChunk | Plant::HttpOnlyIgnored | Plant::BudgetCheckedAfter | Plant::KeyIgnoresLast | Plant::MemoNeverEvicts | Plant::MemoSharesPoints | Plant::NestedUnderGrandparent | Plant::PublishBeforeRename | Plant::DurableSkipsNameSync | Plant::UnsyncedRenameSyncs | Plant::FlushedBeforeFlush | Plant::StaleFlushedCloned | Plant::ParJoinsInRunOrder | Plant::FreezeLeaks | Plant::SwapFreesAtOnce | Plant::FlightComputesTwice | Plant::ChanPassesCapacity | Plant::PublishOnceOverwrites | Plant::OrderTokenTwice | Plant::TaskMigrates | Plant::AncestorsUnbounded) => Ok(()),
                (Some(failed), Plant::None | Plant::SubdirKeepsParent | Plant::OpenCreates | Plant::LexicalParent | Plant::WaitIgnoresDeadline | Plant::UntilWidens | Plant::WaitIgnoresCancel | Plant::ArenaUnlimited | Plant::SyncSkipped | Plant::EnvOnlyReplaces | Plant::CancelSkipsWait | Plant::CloseSkipsSync | Plant::OffersIgnored | Plant::OnlyWidens | Plant::KvPutUnsynced | Plant::LocalDropsChunk | Plant::HttpOnlyIgnored | Plant::BudgetCheckedAfter | Plant::KeyIgnoresLast | Plant::MemoNeverEvicts | Plant::MemoSharesPoints | Plant::NestedUnderGrandparent | Plant::PublishBeforeRename | Plant::DurableSkipsNameSync | Plant::UnsyncedRenameSyncs | Plant::FlushedBeforeFlush | Plant::StaleFlushedCloned | Plant::ParJoinsInRunOrder | Plant::FreezeLeaks | Plant::SwapFreesAtOnce | Plant::FlightComputesTwice | Plant::ChanPassesCapacity | Plant::PublishOnceOverwrites | Plant::OrderTokenTwice | Plant::TaskMigrates | Plant::AncestorsUnbounded) => Err(failed),
            },
        }
    }
}

impl<T> Task<'_, T> {
    /// Its name in its scope.
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn name(&self) -> &'static str {
        self.name
    }

    /// The task finished: what it returned, or how it failed (named by its scope and its
    /// name). Under `Inline` it ended at its spawn, so this does not wait. Under `Threads` it
    /// runs the task here if no worker has started it, else waits for it on `cx`'s thread (a
    /// worker runs its own queued tasks meanwhile), until `cx`'s deadline and the abandon wait
    /// (the module's doc).
    #[blaze::consumes]
    pub fn join(self, cx: &Cx) -> Result<T, TaskError> {
        let Task { name, held, unfinished, .. } = self;
        unfinished.finished();
        match held {
            Held::Ended(outcome) => outcome,
            Held::Queued(pending) => Scope::outcome(pending.finish(cx, name)),
        }
    }
}

/// What a panic said.
pub(crate) fn said(payload: &(dyn Any + Send)) -> String {
    match payload.downcast_ref::<&'static str>() {
        // alloc: an error's message.
        Some(said) => (*said).to_owned(),
        None => match payload.downcast_ref::<String>() {
            // clone: an error's message.
            Some(said) => said.clone(),
            // alloc: an error's message.
            None => "a payload that is not text".to_owned(),
        },
    }
}

impl fmt::Debug for Scope<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Scope(in {}, running {}, failed {:?})",
            self.live.cx.name(),
            self.live.running.get(),
            self.live.failed.borrow()
        )
    }
}

impl<T> fmt::Debug for Task<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let outcome = match &self.held {
            Held::Ended(Ok(_)) => "ended",
            Held::Ended(Err(_)) => "failed",
            Held::Queued(pending) if pending.queued.done() => "ended, not joined",
            Held::Queued(_) => "queued or running",
        };
        write!(f, "Task({}, {outcome})", self.name)
    }
}
