//! The task's context (docs/v2/plan/design/std.md, "Cx: the task's context"): on whose
//! behalf code runs. Every call that can wait takes `cx: &Cx` first. It is a parameter, never
//! a thread-local.
//!
//! - The deadline, on the driver's clock: [`Cx::until`] makes a child whose deadline is the
//!   earlier of the two, so like a capability it only narrows.
//! - Cancellation: a scope that fails or is cancelled cancels its tasks; a wait under a
//!   cancelled `Cx` ends with [`Stopped::Cancelled`], and [`Cx::check`] is the one line a long
//!   computation calls.
//! - The span: the task's name, which names each of its waits (rule every-wait-says-what).
//!   [`Cx::span`] and [`Cx::event`] record into the recorder of the world the root context
//!   came from ([`crate::Io::trace`]), never into a global one.
//! - The frame: the task's bump allocator ([`blaze_mem::Frame`]), freed whole when the task
//!   and every context made from it are gone.
//! - The one wait ([`Cx::wait`]): every call in bstd that waits (IO, sleeps) waits through it,
//!   until the deadline, ending ready or in [`Stopped::Deadline`] or [`Stopped::Cancelled`].
//! - Effects: what only reads the context ([`Cx::deadline`], [`Cx::name`],
//!   [`Cx::cancelled`], [`Cx::check`]) is `#[blaze::noalloc]` and `#[blaze::nonblocking]`,
//!   checked: the one line a long computation calls never waits and never allocates.
//!   [`Cx::span`] and [`Cx::event`] take the world's lock to record, and [`Cx::wait`] is the
//!   wait; none of the three is marked.
//! - Tasks: [`Cx::scope`] makes a scope of tasks, a linear [`Scope`] value joined with
//!   [`Scope::join`] ([`crate::task`]); [`Cx::scope_with`] runs a closure as one. There is no
//!   `io.tasks()`.
//! - Where the task runs: the worker it was placed on when it started, which every wait it
//!   resumes from keeps (std.md, "Layouts": a started task never changes its thread). A debug
//!   build checks it at each resume against the OS thread the task started on.
//! - Lock order: [`Cx::lock_order`] hands the task its one [`crate::sync::ranks::Order`], the
//!   token ranked locks are taken with.
//! - Crossing threads: a `Cx` never does. A task that runs on a pool's worker gets its context
//!   from a [`Seed`] made by its spawner (std.md, "A `Seed`"): what of a context may cross,
//!   grown into the task's `Cx` on the thread that runs it.

use alloc::{rc::Rc, sync::Arc};
use core::{
    cell::Cell,
    sync::atomic::{AtomicBool, Ordering},
};

use blaze_core::{Done, Tag, wait::Deadline};
use blaze_mem::Frame;

use crate::{
    Plant, Stopped,
    io::{Ends, Io},
    mem::Bytes,
    task::{
        Layout, Scope, TaskError,
        threads::{Class, Pool},
    },
    trace::RecordKind,
};

/// The task's context. Not `Send`: it shares its task's frame, which is not `Sync`, and its
/// placement, an `Rc`; a task on another thread gets its own from a [`Seed`].
pub struct Cx {
    deadline: Deadline,
    cancel: Arc<Cancel>,
    span: &'static str,
    frame: Arc<Frame>,
    /// The world the root context came from: where its spans and events are recorded.
    world: Io,
    plant: Plant,
    /// The class of the scope the context is in: a background scope's tasks run on the
    /// pool's background queues (std.md, "`Threads`", point 5), and so do its scopes'.
    class: Class,
    /// Where its task runs, and its task's lock-order token: one per task, shared by the
    /// task's contexts.
    placed: Rc<Placed>,
}

/// Where the code holding a context runs: worker `n` of its pool, or a thread outside the pool
/// (the entry's, a test's, or any thread of an `Inline` root).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Here {
    Outside,
    Worker(u32),
}

/// Where a task runs, kept from its start (std.md, "Layouts": a started task never changes its
/// thread), and whether its lock-order token is out.
struct Placed {
    /// The worker it was placed on at its start. Under `Inline` every task runs on the thread
    /// that spawned it, so the worker is the scope's count of spawns, a model of the placement
    /// a pool makes; a simulated migration is [`Plant::TaskMigrates`]. On a pool's worker it is
    /// that worker's index.
    worker: Cell<u32>,
    /// The OS thread it started on: a debug build checks each resume against it (rustc's
    /// builds only: blaze's lets a task move).
    #[cfg(all(debug_assertions, not(blaze)))]
    thread: crate::rt::ThreadMark,
    /// Whether [`Cx::lock_order`]'s token is out.
    order_out: Cell<bool>,
    /// The pool its tasks are spawned on: none for an `Inline` root and what is under it.
    pool: Option<Pool>,
    /// The thread it runs on, as the pool knows it: a spawn from worker `n` goes to `n`'s own
    /// queue, which only `n` pushes to.
    here: Here,
}

impl Placed {
    fn on(worker: u32, pool: Option<Pool>, here: Here) -> Rc<Placed> {
        // alloc: once per task, where it runs.
        Rc::new(Placed {
            worker: Cell::new(worker),
            #[cfg(all(debug_assertions, not(blaze)))]
            thread: crate::rt::thread_mark(),
            order_out: Cell::new(false),
            pool,
            here,
        })
    }
}

/// What of a task's context crosses to the thread that runs it (std.md, "A `Seed`"), made by
/// [`Cx::seed`] on the spawner and grown into the task's `Cx` by [`Seed::grow`] there.
///
/// It carries: the deadline (a time); the scope's cancellation (an `Arc` of an atomic flag and
/// its outer chain, read and set from any thread); the task's name and its parent's span (for
/// the trace, recorded on the spawner so the order of openings is the spawner's); the task's
/// frame, reserved by the spawner (so a frame that cannot be had fails the task at its spawn,
/// as under `Inline`) and never touched by it again (`Frame` is `Send`, not `Sync`: one thread
/// at a time bumps it, and from here on that is the runner); the world (`Io` is `Send` and
/// `Sync`: one handle behind its lock); the plant; the pool; and the scope's class.
///
/// It does not carry what is one thread's: the shared owner of the frame (an `Arc` of a
/// `Frame`, which is not `Sync`), the placement and its lock-order token (one task's, made where
/// it runs, as is the debug check of its thread), and anything a closure borrowed, which is the
/// closure's to be `Send` about (both spawns take `Send` closures and results).
pub(crate) struct Seed {
    deadline: Deadline,
    cancel: Arc<Cancel>,
    span: &'static str,
    frame: Frame,
    world: Io,
    plant: Plant,
    pool: Option<Pool>,
    class: Class,
    /// The placement model's worker, for a task grown outside the pool (a joiner that runs its
    /// task itself): what the spawner would have counted under `Inline`.
    worker: u32,
}

// A `Seed` crosses threads; the bound is stated where it is defined, so a field that is not
// `Send` fails here, naming it, rather than at a spawn far away.
const _: () = {
    const fn crosses<T: Send>() {}
    crosses::<Seed>();
    crosses::<Io>();
};

impl Seed {
    /// The task's context on the thread that runs it, which is `here`: its frame shared from
    /// now on by the task's contexts on this thread, its placement made here.
    pub(crate) fn grow(self, here: Here) -> Cx {
        let Seed { deadline, cancel, span, frame, world, plant, pool, class, worker } = self;
        let worker = match here {
            Here::Worker(at) => at,
            Here::Outside => worker,
        };
        Cx { deadline, cancel, span, frame: shared(frame), world, plant, class, placed: Placed::on(worker, pool, here) }
    }
}

/// A scope's cancellation, held where a scope's state is shared with its tasks' threads.
pub(crate) struct Canceller(Arc<Cancel>);

impl Canceller {
    /// Cancels the scope: as [`Cx::cancel`].
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub(crate) fn cancel(&self) {
        self.0.set.store(true, Ordering::Release);
    }
}

/// A scope's cancellation: set, or set in a scope it is inside.
struct Cancel {
    set: AtomicBool,
    outer: Option<Arc<Cancel>>,
}

impl Cancel {
    fn new(outer: Option<Arc<Cancel>>) -> Arc<Cancel> {
        // alloc: once per scope, its cancellation.
        Arc::new(Cancel { set: AtomicBool::new(false), outer })
    }

    fn is_set(&self) -> bool {
        let mut at = Some(self);
        while let Some(cancel) = at {
            if cancel.set.load(Ordering::Acquire) {
                return true;
            }
            at = cancel.outer.as_deref();
        }
        false
    }
}

/// `frame` shared by a task's contexts.
#[expect(
    clippy::arc_with_non_send_sync,
    reason = "a task's contexts share its frame on the task's thread; `Cx` is not `Send` on purpose"
)]
fn shared(frame: Frame) -> Arc<Frame> {
    // alloc: once per task, the owner of its frame.
    Arc::new(frame)
}

impl Cx {
    /// The root context of `world`: `deadline`, never cancelled, named `main`.
    pub(crate) fn root(deadline: Deadline, frame: Frame, world: Io) -> Cx {
        let plant = world.plant();
        world.record_trace("main", RecordKind::Opened { parent: "main" });
        Cx {
            deadline,
            cancel: Cancel::new(None),
            span: "main",
            frame: shared(frame),
            world,
            plant,
            class: Class::Foreground,
            placed: Placed::on(0, None, Here::Outside),
        }
    }

    /// The root context of `world` on `pool` (std.md, "`Threads`"): as [`Cx::root`], its tasks
    /// spawned on the pool's workers, its own code on the calling thread, outside the pool.
    pub(crate) fn root_on(deadline: Deadline, frame: Frame, world: Io, pool: Pool) -> Cx {
        let cx = Cx::root(deadline, frame, world);
        Cx { placed: Placed::on(0, Some(pool), Here::Outside), ..cx }
    }

    /// A child whose deadline is the earlier of this one's and `deadline`: never later.
    pub fn until(&self, deadline: Deadline) -> Cx {
        let deadline = match self.plant {
            Plant::UntilWidens => deadline,
            Plant::None | Plant::SubdirKeepsParent | Plant::OpenCreates | Plant::LexicalParent | Plant::WaitIgnoresDeadline | Plant::WaitIgnoresCancel | Plant::ArenaUnlimited | Plant::SyncSkipped | Plant::EnvOnlyReplaces | Plant::CancelSkipsWait | Plant::CloseSkipsSync | Plant::JoinDropsFailure | Plant::OffersIgnored | Plant::OnlyWidens | Plant::KvPutUnsynced | Plant::LocalDropsChunk | Plant::HttpOnlyIgnored | Plant::BudgetCheckedAfter | Plant::KeyIgnoresLast | Plant::MemoNeverEvicts | Plant::MemoSharesPoints | Plant::NestedUnderGrandparent | Plant::PublishBeforeRename | Plant::DurableSkipsNameSync | Plant::FlushedBeforeFlush | Plant::StaleFlushedCloned | Plant::ParJoinsInRunOrder | Plant::FreezeLeaks | Plant::SwapFreesAtOnce | Plant::FlightComputesTwice | Plant::ChanPassesCapacity | Plant::PublishOnceOverwrites | Plant::OrderTokenTwice | Plant::TaskMigrates => self.deadline.min(deadline),
        };
        Cx { deadline, ..self.child() }
    }

    /// A child named `name`, inside this one's span: its waits say they are `name`'s, and the
    /// span's opening is recorded.
    pub fn span(&self, name: &'static str) -> Cx {
        self.world.record_trace(name, RecordKind::Opened { parent: self.span });
        Cx { span: name, ..self.child() }
    }

    /// `what` happened, in this span: recorded on the driver's clock.
    pub fn event(&self, what: &'static str) {
        self.world.record_trace(self.span, RecordKind::Event(what));
    }

    /// The deadline every wait under it ends by.
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn deadline(&self) -> Deadline {
        self.deadline
    }

    /// The span's name.
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn name(&self) -> &'static str {
        self.span
    }

    /// Whether its scope, or a scope it is inside, was cancelled.
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn cancelled(&self) -> bool {
        self.cancel.is_set()
    }

    /// [`Stopped::Cancelled`] once its scope was cancelled: the line a long computation calls.
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn check(&self) -> Result<(), Stopped> {
        match self.cancelled() {
            true => Err(Stopped::Cancelled { span: self.span }),
            false => Ok(()),
        }
    }

    /// The task's frame, for `Vec::new_in(cx.frame())`: freed when the task ends.
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn frame(&self) -> &Frame {
        &self.frame
    }

    /// Empty bytes in the task's frame, named by the span when they run out of room.
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn bytes(&self) -> Bytes<'_> {
        Bytes::new_in(&self.frame, self.span)
    }

    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub(crate) fn plant(&self) -> Plant {
        self.plant
    }

    /// The world it came from.
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub(crate) fn io(&self) -> &Io {
        &self.world
    }

    /// A scope of tasks inside this one: spawn its tasks with [`Scope::spawn`], then
    /// [`Scope::join`] it, which returns once each has ended, with the first that failed. A
    /// task may borrow from the frame that holds the scope. The scope is linear: leaving it
    /// unjoined is a compile error (docs/v2/plan/design/std.md, "Tasks and waiting").
    #[track_caller]
    pub fn scope(&self) -> Scope<'_> {
        Scope::new(self.scoped())
    }

    /// `body` run with a scope of tasks inside this one, which is then joined: its result,
    /// or the first of its tasks that failed. The convenience over [`Cx::scope`].
    #[track_caller]
    pub fn scope_with<'p, R>(&'p self, body: impl FnOnce(&Scope<'p>) -> R) -> Result<R, TaskError> {
        let scope = self.scope();
        let made = body(&scope);
        match scope.join(self) {
            Ok(()) => Ok(made),
            Err(failed) => Err(failed),
        }
    }

    /// The one wait: the next completion of `tag` from `io`, until this context's deadline.
    /// It ends ready, or with [`Stopped::Cancelled`] once the scope is cancelled, or
    /// [`Stopped::Deadline`] once the deadline passes on the driver's clock, or
    /// [`Stopped::Stalled`] if the driver keeps answering early (rule bounded-progress). A
    /// stopped wait gives up a one-shot tag ([`Ends::Once`]); a stream's stays its owner's.
    /// A one-shot wait stopped by cancellation first waits, heeding no cancellation and until
    /// the deadline, for its effect to end ([`Io::settle`]): nothing it does lands after the
    /// task has seen `Cancelled`.
    ///
    /// In S1's `Inline` layout it polls the driver on the calling thread ([`Io::poll`]); a
    /// fiber layout parks the task here instead.
    pub(crate) fn wait(&self, io: &Io, tag: Tag, ends: Ends) -> Result<Done, Stopped> {
        let deadline = match self.plant {
            Plant::WaitIgnoresDeadline => Deadline::NEVER,
            Plant::None | Plant::SubdirKeepsParent | Plant::OpenCreates | Plant::LexicalParent | Plant::UntilWidens | Plant::WaitIgnoresCancel | Plant::ArenaUnlimited | Plant::SyncSkipped | Plant::EnvOnlyReplaces | Plant::CancelSkipsWait | Plant::CloseSkipsSync | Plant::JoinDropsFailure | Plant::OffersIgnored | Plant::OnlyWidens | Plant::KvPutUnsynced | Plant::LocalDropsChunk | Plant::HttpOnlyIgnored | Plant::BudgetCheckedAfter | Plant::KeyIgnoresLast | Plant::MemoNeverEvicts | Plant::MemoSharesPoints | Plant::NestedUnderGrandparent | Plant::PublishBeforeRename | Plant::DurableSkipsNameSync | Plant::FlushedBeforeFlush | Plant::StaleFlushedCloned | Plant::ParJoinsInRunOrder | Plant::FreezeLeaks | Plant::SwapFreesAtOnce | Plant::FlightComputesTwice | Plant::ChanPassesCapacity | Plant::PublishOnceOverwrites | Plant::OrderTokenTwice | Plant::TaskMigrates => self.deadline,
        };
        let mut idle = 0;
        loop {
            if let Some(done) = io.take(tag) {
                return Ok(done);
            }
            let stopped = if self.plant != Plant::WaitIgnoresCancel && self.cancelled() {
                Some(Stopped::Cancelled { span: self.span })
            } else if deadline.passed(io.now()) {
                Some(Stopped::Deadline { span: self.span, at: deadline.get() })
            } else if idle == io.limits().poll_fuel {
                Some(Stopped::Stalled { span: self.span, polls: idle })
            } else {
                None
            };
            if let Some(stopped) = stopped {
                if ends == Ends::Once {
                    let settles = matches!(stopped, Stopped::Cancelled { .. }) && self.plant != Plant::CancelSkipsWait;
                    match settles {
                        // Ended or given up: either way its tag is no longer waited on.
                        true => _ = io.settle(tag, deadline, self.span),
                        false => io.forget(tag, Ends::Once),
                    }
                }
                return Err(stopped);
            }
            match io.poll(deadline) {
                0 => idle += 1,
                _ => idle = 0,
            }
            self.resumed();
        }
    }

    /// The one wait, for a condition bstd keeps itself rather than a completion of the
    /// driver's (bytes in an in-memory pipe, a key the simulated terminal will send): until
    /// `ready` gives a value, or this context's deadline or cancellation, with the same typed
    /// stops as [`Cx::wait`]. `ready` is asked before each poll of the driver.
    ///
    /// In S1's `Inline` layout nothing else runs while a task waits, so a condition only
    /// another task could make true ends here at the deadline; a fiber layout parks the task
    /// on the condition's key instead, and its maker wakes it.
    pub(crate) fn wait_for<T>(&self, io: &Io, mut ready: impl FnMut() -> Option<T>) -> Result<T, Stopped> {
        let deadline = match self.plant {
            Plant::WaitIgnoresDeadline => Deadline::NEVER,
            Plant::None | Plant::SubdirKeepsParent | Plant::OpenCreates | Plant::LexicalParent | Plant::UntilWidens | Plant::WaitIgnoresCancel | Plant::ArenaUnlimited | Plant::SyncSkipped | Plant::EnvOnlyReplaces | Plant::CancelSkipsWait | Plant::CloseSkipsSync | Plant::JoinDropsFailure | Plant::OffersIgnored | Plant::OnlyWidens | Plant::KvPutUnsynced | Plant::LocalDropsChunk | Plant::HttpOnlyIgnored | Plant::BudgetCheckedAfter | Plant::KeyIgnoresLast | Plant::MemoNeverEvicts | Plant::MemoSharesPoints | Plant::NestedUnderGrandparent | Plant::PublishBeforeRename | Plant::DurableSkipsNameSync | Plant::FlushedBeforeFlush | Plant::StaleFlushedCloned | Plant::ParJoinsInRunOrder | Plant::FreezeLeaks | Plant::SwapFreesAtOnce | Plant::FlightComputesTwice | Plant::ChanPassesCapacity | Plant::PublishOnceOverwrites | Plant::OrderTokenTwice | Plant::TaskMigrates => self.deadline,
        };
        let mut idle = 0;
        loop {
            if let Some(got) = ready() {
                return Ok(got);
            }
            if self.plant != Plant::WaitIgnoresCancel && self.cancelled() {
                return Err(Stopped::Cancelled { span: self.span });
            }
            if deadline.passed(io.now()) {
                return Err(Stopped::Deadline { span: self.span, at: deadline.get() });
            }
            if idle == io.limits().poll_fuel {
                return Err(Stopped::Stalled { span: self.span, polls: idle });
            }
            match io.poll(deadline) {
                0 => idle += 1,
                _ => idle = 0,
            }
            self.resumed();
        }
    }

    /// The task resumes from a wait: on the worker, and the OS thread, it started on (std.md,
    /// "Layouts"). Under `Inline` the wait polled on the task's own thread, so this only
    /// checks; a fiber layout's park returns here, and its scheduler must put the task back
    /// where it ran. Built by blaze the rule is lifted: a task may move, because blaze refuses
    /// a value that is not `Send` live across a call that can wait, and the check goes.
    fn resumed(&self) {
        if self.plant == Plant::TaskMigrates {
            let placed = &self.placed;
            placed.worker.set(placed.worker.get() + 1);
        }
        #[cfg(all(debug_assertions, not(blaze)))]
        if crate::rt::thread_mark() != self.placed.thread {
            panic!(
                "bstd::cx task {}: resumed from a wait on another thread than it started on \
                 (std.md, \"Layouts\": a started task never changes its thread)",
                self.span
            );
        }
    }

    /// The worker this context's task was placed on at its start, which it keeps.
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub(crate) fn worker(&self) -> u32 {
        self.placed.worker.get()
    }

    /// Takes the task's lock-order token: `false` when it is out already.
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub(crate) fn take_order(&self) -> bool {
        let was_out = self.placed.order_out.replace(true);
        !was_out || self.plant == Plant::OrderTokenTwice
    }

    /// Gives the task's lock-order token back.
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub(crate) fn give_order(&self) {
        self.placed.order_out.set(false);
    }

    /// Whether this context stopped already at `now`: then a call asks nothing of the driver.
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub(crate) fn stopped_by(&self, now: core::time::Duration) -> Result<(), Stopped> {
        if self.plant != Plant::WaitIgnoresCancel {
            self.check()?;
        }
        if self.plant != Plant::WaitIgnoresDeadline && self.deadline.passed(now) {
            return Err(Stopped::Deadline { span: self.span, at: self.deadline.get() });
        }
        Ok(())
    }

    /// The context of a scope inside this task: its own cancellation, under this one's.
    pub(crate) fn scoped(&self) -> Cx {
        Cx { cancel: Cancel::new(Some(Arc::clone(&self.cancel))), ..self.child() }
    }

    /// The context of a task of the scope `self` is: the scope's deadline and cancellation,
    /// its own name and frame.
    /// It is placed on `worker` and stays there (std.md, "Layouts").
    pub(crate) fn task(&self, name: &'static str, frame: Frame, worker: u32) -> Cx {
        self.world.record_trace(name, RecordKind::Opened { parent: self.span });
        let placed = Placed::on(worker, self.placed.pool.clone(), self.placed.here);
        Cx { span: name, frame: shared(frame), placed, ..self.child() }
    }

    /// What of the context of a task of the scope `self` is crosses to the thread that will
    /// run it (std.md, "A `Seed`"): as [`Cx::task`], with the scope's `class`; `worker` is the
    /// placement model's, used if the task is grown outside the pool. The task's opening is
    /// recorded here, on the spawner, so the trace's order of openings is the spawner's.
    pub(crate) fn seed(&self, name: &'static str, frame: Frame, class: Class, worker: u32) -> Seed {
        self.world.record_trace(name, RecordKind::Opened { parent: self.span });
        Seed {
            deadline: self.deadline,
            cancel: Arc::clone(&self.cancel),
            span: name,
            frame,
            world: self.world.clone(),
            plant: self.plant,
            pool: self.placed.pool.clone(),
            class,
            worker,
        }
    }

    /// The pool its tasks are spawned on: none under `Inline`.
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub(crate) fn pool(&self) -> Option<&Pool> {
        self.placed.pool.as_ref()
    }

    /// Where the code holding this context runs, as its pool knows it.
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub(crate) fn here(&self) -> Here {
        self.placed.here
    }

    /// The class of the scope this context is in.
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub(crate) fn class(&self) -> Class {
        self.class
    }

    /// The layout a scope of this context runs: `Threads` on a pool, `Inline` without one.
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub(crate) fn layout(&self) -> Layout {
        match &self.placed.pool {
            Some(pool) => Layout::Threads { workers: pool.workers() },
            None => Layout::Inline,
        }
    }

    /// This context's cancellation, to be set from another thread (a task's failure).
    pub(crate) fn canceller(&self) -> Canceller {
        Canceller(Arc::clone(&self.cancel))
    }

    /// This context, in a scope of `class`: what a narrowed scope gives the tasks it runs.
    pub(crate) fn with_class(self, class: Class) -> Cx {
        Cx { class, ..self }
    }

    /// Cancels the scope this context's cancellation is: every context sharing it, and every
    /// scope inside it.
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub(crate) fn cancel(&self) {
        self.cancel.set.store(true, Ordering::Release);
    }

    fn child(&self) -> Cx {
        Cx {
            deadline: self.deadline,
            cancel: Arc::clone(&self.cancel),
            span: self.span,
            frame: Arc::clone(&self.frame),
            world: self.world.clone(),
            plant: self.plant,
            class: self.class,
            placed: Rc::clone(&self.placed),
        }
    }
}

impl core::fmt::Debug for Cx {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Cx")
            .field("span", &self.span)
            .field("deadline", &self.deadline)
            .field("cancelled", &self.cancelled())
            .finish()
    }
}
