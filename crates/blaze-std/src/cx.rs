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
    cell::{Cell, OnceCell},
    sync::atomic::{AtomicBool, Ordering},
};

use blaze_core::{Done, Tag, wait::Deadline};
use blaze_mem::Frame;
use blaze_own::planted::NoPlant;

use crate::{
    Plant, Stopped,
    io::{Ends, Io},
    mem::Bytes,
    task::{
        Layout, Scope, TaskError,
        threads::{Class, Pool},
    },
    trace::RecordKind,
    telemetry::{Begun, Slot as OpenSlot, SpanPath},
    workers::{Lease, Worker, WorkerId, Workers, charge_wait_at},
};

/// A reading of the world's clock as nanoseconds: what the telemetry hooks record.
#[inline]
fn ns_of(at: core::time::Duration) -> u64 {
    u64::try_from(at.as_nanos()).unwrap_or(u64::MAX)
}

/// The end of a span opened by [`Cx::span`] (decision 1 of [`crate::telemetry`]'s doc): held
/// by the context `span` returned and by nothing else (a child of it, `until`, `scoped`, a
/// task's context, takes `None`), so the span is exactly that value's life, and its drop is
/// the close: no close call to forget (rule 76). `Cx` is not `Send`, so it drops on the thread,
/// and the worker cell, that opened it.
struct SpanEnd {
    slot: OpenSlot,
    placed: Placement,
    span: &'static str,
}

impl Drop for SpanEnd {
    fn drop(&mut self) {
        let io = self.placed.io();
        let workers = io.workers();
        let placed = self.placed.get();
        let id = placed.cell(workers, self.span);
        let wall = ns_of(io.now());
        let slot = self.slot;
        // A close the cell refuses (its world's workers were replaced while the span was
        // open: `Io::on_workers`) is counted by the cell in `unrecorded`, which the report
        // prints; a drop cannot return it, and a panic here could abort an unwind.
        workers.enter(id, |w| {
            if let Some((tally, cell)) = w.telemetry() {
                let now = cell.snapshot(tally);
                cell.span_close(slot, wall, now);
            }
        });
    }
}

/// Thread-local non-atomic handle to a scope's [`Cancel`] `Arc`: every child context on the
/// same thread (`Cx::span`, `Cx::until`) clones this via a local L1 `Rc::clone` instead of
/// bouncing the shared `Arc<Cancel>` cache line across cores.
#[derive(Clone)]
struct LocalCancel(Rc<Arc<Cancel>>);

impl LocalCancel {
    fn new(cancel: Arc<Cancel>) -> LocalCancel {
        // alloc: once per task/scope thread, avoids atomic refcount churn on every span/until.
        LocalCancel(Rc::new(cancel))
    }

    #[inline]
    fn arc(&self) -> &Arc<Cancel> {
        &self.0
    }

    #[inline]
    fn is_set(&self) -> bool {
        self.0.is_set()
    }
}

/// The task's context. Not `Send`: it shares its task's frame, which is not `Sync`, and its
/// placement, an `Rc`; a task on another thread gets its own from a [`Seed`].
///
/// What a context that never uses something does not build (std.md, "What a program pays for
/// at start"): a root whose budget is unbounded has no deadline to read the clock for, a root
/// has no cancellation (nothing can cancel it), and a task's frame is made at its first use.
/// A hosted run's root context is no allocation and no atomic: its placement is held by the
/// run ([`Placement::HostedRoot`]).
pub struct Cx {
    /// `None`: never; no wait or check under it reads the clock for it.
    deadline: Option<Deadline>,
    /// `None`: never cancelled (a root); a scope's is `Some`, its outer chain the scopes it
    /// is in. Wrapped in a local `Rc` so `Cx::span`/`Cx::until` execute zero atomic RMWs.
    cancel: Option<LocalCancel>,
    span: &'static str,
    plant: Plant,
    /// The class of the scope the context is in: a background scope's tasks run on the
    /// pool's background queues (std.md, "`Threads`", point 5), and so do its scopes'.
    class: Class,
    /// Where its task runs, its task's lock-order token, its world handle (`Io`), and its
    /// frame: one per task, shared by the task's contexts with zero atomic refcount ops.
    placed: Placement,
    /// The span path it is in for the telemetry (from `Spans`; the root below): copied by
    /// every child and handed to the tasks it spawns, so their use is charged under it.
    path: SpanPath,
    /// The end of the span this context is (only the one [`Cx::span`] returned, and only when
    /// its worker keeps spans): dropping it closes the span.
    end: Option<SpanEnd>,
}

/// Where a context's task placement and world handle are held.
#[derive(Clone)]
enum Placement {
    /// Shared by the task's contexts: one allocation per task; holds `world: Some(Io)` so
    /// child contexts never atomically clone `Io`'s `Arc`.
    Task(Rc<Placed>),
    /// The root task of a hosted run: held by the run itself ([`crate::guest::Run`], in
    /// `bstd_hosted_start`'s frame), reached through the calling thread's run as every hosted
    /// reach is, so it needs no allocation and no atomic (`Io` is `Reach::Hosted`).
    HostedRoot(Io),
}

impl Placement {
    #[inline]
    fn get(&self) -> &Placed {
        match self {
            Placement::Task(placed) => placed,
            Placement::HostedRoot(_) => crate::guest::Run::current().placed(),
        }
    }

    #[inline]
    fn io(&self) -> &Io {
        match self {
            Placement::Task(placed) => placed.world.as_ref().expect("bstd::cx: task placement holds its world"),
            Placement::HostedRoot(io) => io,
        }
    }
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
pub(crate) struct Placed {
    /// The worker it was placed on at its start. Under `Inline` every task runs on the thread
    /// that spawned it, so the worker is the scope's count of spawns, a model of the placement
    /// a pool makes; a simulated migration is [`Plant::TaskMigrates`]. On a pool's worker it is
    /// that worker's index.
    worker: Cell<u32>,
    /// The OS thread it runs on, marked at the start of its first wait: a debug build checks
    /// each resume against it (rustc's builds only: blaze's lets a task move). Marked there,
    /// not at the task's start, because a task changes thread only across a wait: until its
    /// first, it is on the thread it started on, and a task that never waits (a hello) never
    /// asks the thread who it is.
    #[cfg(all(debug_assertions, not(blaze)))]
    thread: Cell<Option<crate::rt::ThreadMark>>,
    /// Whether [`Cx::lock_order`]'s token is out.
    order_out: Cell<bool>,
    /// The pool its tasks are spawned on: none for an `Inline` root and what is under it.
    pool: Option<Pool>,
    /// The thread it runs on, as the pool knows it: a spawn from worker `n` goes to `n`'s own
    /// queue, which only `n` pushes to.
    here: Here,
    /// The task's frame: given by its spawner (reserved there, so one that cannot be had
    /// fails the spawn), or, for a root, made at its first use with `scratch` bytes.
    frame: OnceCell<Frame>,
    scratch: usize,
    /// Which worker cell its `on_worker` holds (D171, crate::workers): read at each
    /// `on_worker`, never kept across a wait.
    seat: Seat,
    /// The world handle for `Placement::Task` (`None` only for `Placed::hosted_root`, whose
    /// `Placement::HostedRoot` carries the zero-atomic hosted `Io`).
    world: Option<Io>,
}

/// Which cell of the world's [`Workers`] a task's `on_worker` holds.
enum Seat {
    /// Pool worker `at`'s: the task runs on that worker's thread for its life.
    Pool(u32),
    /// A cell taken from an enclosing task on the same thread (an `Inline` child of a root
    /// that leased a caller cell, or of a pool task): one closure holds it at a time.
    Fixed(WorkerId),
    /// A simulated world's: the simulated worker the task's model worker is on now, which
    /// changes at a resume ([`Cx::resumed`]).
    Model,
    /// A root's, or a task a thread outside the pool runs: a simulated world's model worker,
    /// else a caller cell leased at the first `on_worker` and given back with the placement.
    Root(OnceCell<Lease>),
}

impl Placed {
    fn on(worker: u32, pool: Option<Pool>, here: Here, seat: Seat, frame: Option<Frame>, scratch: usize, world: Io) -> Placement {
        // alloc: once per task, where it runs, its world handle, and its frame.
        Placement::Task(Rc::new(Placed::new(worker, pool, here, seat, frame, scratch, Some(world))))
    }

    /// A hosted run's root task's placement, held by the run: on no pool, outside, its frame
    /// of `scratch` bytes made at its first use.
    pub(crate) fn hosted_root(scratch: usize) -> Placed {
        Placed::new(0, None, Here::Outside, Seat::Root(OnceCell::new()), None, scratch, None)
    }

    /// The cell of `workers` (the world's) this task holds in `on_worker` now. `span` names the
    /// task in a refusal.
    fn cell(&self, workers: &Workers, span: &'static str) -> WorkerId {
        match &self.seat {
            Seat::Pool(at) => workers.pool_worker(*at),
            Seat::Fixed(id) => *id,
            Seat::Model => workers.model(self.worker.get()),
            Seat::Root(_) if workers.simulated() => workers.model(self.worker.get()),
            Seat::Root(lease) => lease
                .get_or_init(|| match workers.lease() {
                    Ok(lease) => lease,
                    Err(error) => panic!("bstd::cx {span}: no worker cell for a thread outside the pool: {error}"),
                })
                .id(),
        }
    }

    /// The seat of an `Inline` child of this task: on its thread, so the same cell, or in a
    /// simulated world a model worker of its own.
    fn child_seat(&self, workers: &Workers, span: &'static str) -> Seat {
        match &self.seat {
            Seat::Pool(at) => Seat::Pool(*at),
            Seat::Fixed(id) => Seat::Fixed(*id),
            Seat::Model => Seat::Model,
            Seat::Root(_) if workers.simulated() => Seat::Model,
            Seat::Root(_) => Seat::Fixed(self.cell(workers, span)),
        }
    }

    fn new(worker: u32, pool: Option<Pool>, here: Here, seat: Seat, frame: Option<Frame>, scratch: usize, world: Option<Io>) -> Placed {
        let made = OnceCell::new();
        if let Some(frame) = frame {
            // A fresh cell takes its value: `set` cannot find one there.
            _ = made.set(frame);
        }
        Placed {
            worker: Cell::new(worker),
            #[cfg(all(debug_assertions, not(blaze)))]
            thread: Cell::new(None),
            order_out: Cell::new(false),
            pool,
            here,
            frame: made,
            scratch,
            seat,
            world,
        }
    }

    /// The task's frame, made now if this is its first use.
    fn frame(&self) -> &Frame {
        self.frame.get_or_init(|| match Frame::with_reserve(self.scratch, blaze_mem::Tag::Frames) {
            Ok(frame) => frame,
            Err(why) => panic!("bstd::cx: the task's frame of {} bytes could not be made: {why:?}", self.scratch),
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
    deadline: Option<Deadline>,
    cancel: Option<Arc<Cancel>>,
    span: &'static str,
    frame: Frame,
    world: Io,
    plant: Plant,
    pool: Option<Pool>,
    class: Class,
    /// The placement model's worker, for a task grown outside the pool (a joiner that runs its
    /// task itself): what the spawner would have counted under `Inline`.
    worker: u32,
    /// The spawner's span path: the task's use is charged under it (telemetry).
    path: SpanPath,
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
        let Seed { deadline, cancel, span, frame, world, plant, pool, class, worker, path } = self;
        // The seat (D171): a pool worker's own cell, or, on a thread outside the pool (a
        // joiner that runs its task itself), a caller cell of the task's own.
        let (worker, seat) = match here {
            Here::Worker(at) => (at, Seat::Pool(at)),
            Here::Outside => (worker, Seat::Root(OnceCell::new())),
        };
        Cx {
            deadline,
            cancel: cancel.map(LocalCancel::new),
            span,
            plant,
            class,
            placed: Placed::on(worker, pool, here, seat, Some(frame), 0, world),
            path,
            end: None,
        }
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

impl Cx {
    /// The root context of `world`: ending at `deadline` (`None`: never), never cancelled,
    /// named `main`, its frame of `scratch` bytes made at its first use.
    pub(crate) fn root(deadline: Option<Deadline>, scratch: usize, world: Io) -> Cx {
        let plant = world.plant();
        world.record_trace("main", RecordKind::Opened { parent: "main" });
        Cx {
            deadline,
            cancel: None,
            span: "main",
            plant,
            class: Class::Foreground,
            placed: Placed::on(0, None, Here::Outside, Seat::Root(OnceCell::new()), None, scratch, world),
            path: SpanPath::ROOT,
            end: None,
        }
    }

    /// The root context of a hosted run, `world` its hosted `Io`: as [`Cx::root`], its
    /// placement the one the run holds ([`Placement::HostedRoot`]). Only `bstd_hosted_start`
    /// makes it, once per run; a root the program makes itself (`io.cx`) has its own.
    pub(crate) fn hosted_root(deadline: Option<Deadline>, world: Io) -> Cx {
        let plant = world.plant();
        world.record_trace("main", RecordKind::Opened { parent: "main" });
        Cx { deadline, cancel: None, span: "main", plant, class: Class::Foreground, placed: Placement::HostedRoot(world), path: SpanPath::ROOT, end: None }
    }

    /// Where its task runs: shared by the task's contexts, or the hosted run's root's.
    fn placed(&self) -> &Placed {
        self.placed.get()
    }

    /// The root context of `world` on `pool` (std.md, "`Threads`"): as [`Cx::root`], its tasks
    /// spawned on the pool's workers, its own code on the calling thread, outside the pool.
    pub(crate) fn root_on(deadline: Deadline, frame: Frame, world: Io, pool: Pool) -> Cx {
        let plant = world.plant();
        world.record_trace("main", RecordKind::Opened { parent: "main" });
        Cx {
            deadline: Some(deadline),
            // A pool's fold may cancel its root (`PerWorker`'s tally): it has one from the start.
            cancel: Some(LocalCancel::new(Cancel::new(None))),
            span: "main",
            plant,
            class: Class::Foreground,
            placed: Placed::on(0, Some(pool), Here::Outside, Seat::Root(OnceCell::new()), Some(frame), 0, world),
            path: SpanPath::ROOT,
            end: None,
        }
    }

    /// A child whose deadline is the earlier of this one's and `deadline`: never later.
    pub fn until(&self, deadline: Deadline) -> Cx {
        let deadline = match self.plant {
            Plant::UntilWidens => deadline,
            Plant::None | Plant::SubdirKeepsParent | Plant::OpenCreates | Plant::LexicalParent | Plant::WaitIgnoresDeadline | Plant::WaitIgnoresCancel | Plant::ArenaUnlimited | Plant::SyncSkipped | Plant::EnvOnlyReplaces | Plant::CancelSkipsWait | Plant::CloseSkipsSync | Plant::JoinDropsFailure | Plant::OffersIgnored | Plant::OnlyWidens | Plant::KvPutUnsynced | Plant::LocalDropsChunk | Plant::HttpOnlyIgnored | Plant::BudgetCheckedAfter | Plant::KeyIgnoresLast | Plant::MemoNeverEvicts | Plant::MemoSharesPoints | Plant::NestedUnderGrandparent | Plant::PublishBeforeRename | Plant::DurableSkipsNameSync | Plant::UnsyncedRenameSyncs | Plant::FlushedBeforeFlush | Plant::StaleFlushedCloned | Plant::ParJoinsInRunOrder | Plant::FreezeLeaks | Plant::SwapFreesAtOnce | Plant::FlightComputesTwice | Plant::ChanPassesCapacity | Plant::PublishOnceOverwrites | Plant::OrderTokenTwice | Plant::TaskMigrates | Plant::AncestorsUnbounded => self.deadline().min(deadline),
        };
        Cx { deadline: Some(deadline), ..self.child() }
    }

    /// A child named `name`, inside this one's span: its waits say they are `name`'s, and the
    /// span's opening is recorded.
    ///
    /// For the telemetry ([`crate::telemetry`], "Spans across tasks") the span is the life of
    /// the value returned: from `Spans` its worker's cell opens it now and its drop closes it;
    /// its children and the tasks it spawns are charged under its path. At `Totals`, or on a
    /// world with no cells, nothing more than the trace's record is done.
    pub fn span(&self, name: &'static str) -> Cx {
        let io = self.io();
        io.record_trace(name, RecordKind::Opened { parent: self.span });
        let child = Cx { span: name, ..self.child() };
        let workers = io.workers();
        if !workers.telemetry_level().is_some_and(|level| level.keeps(crate::telemetry::Level::Spans)) {
            return child;
        }
        let wall = ns_of(io.now());
        let parent = self.path;
        let opened = self.on_worker(|w| {
            let (tally, cell) = w.telemetry()?;
            let at = cell.snapshot(tally);
            cell.span_open(&parent, name, wall, at)
        });
        match opened {
            Some((slot, path)) => Cx {
                path,
                end: Some(SpanEnd { slot, placed: self.placed.clone(), span: self.span }),
                ..child
            },
            None => child,
        }
    }

    /// `what` happened, in this span: recorded on the driver's clock.
    pub fn event(&self, what: &'static str) {
        self.io().record_trace(self.span, RecordKind::Event(what));
    }

    /// The deadline every wait under it ends by.
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn deadline(&self) -> Deadline {
        match self.deadline {
            Some(deadline) => deadline,
            None => Deadline::NEVER,
        }
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
        match &self.cancel {
            Some(cancel) => cancel.is_set(),
            None => false,
        }
    }

    /// Whether this context belongs to a cancellable scope (a root context has none).
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub(crate) fn cancellable(&self) -> bool {
        self.cancel.is_some()
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

    /// The task's frame, for `Vec::new_in(cx.frame())`: freed when the task ends. A root's is
    /// made at this first ask (it reserves nothing until it hands out bytes).
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn frame(&self) -> &Frame {
        self.placed().frame()
    }

    /// Empty bytes in the task's frame, named by the span when they run out of room.
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn bytes(&self) -> Bytes<'_> {
        Bytes::new_in(self.placed().frame(), self.span)
    }

    /// Runs `f` with a child context backed by a fresh scratch frame of at most `scratch` bytes
    /// (taken from the calling thread's warm chunk shelf, ~2 ns once warm) that is released when
    /// `f` returns or unwinds. What `f` allocates in `scratch_cx.frame()` never accumulates across
    /// iterations of a loop or batch, while still inheriting this context's deadline,
    /// cancellation, span, worker seat and telemetry path.
    ///
    /// Returns [`Stopped::OutOfMemory`] if the scratch frame cannot be reserved.
    pub fn scratch<R>(&self, scratch: usize, f: impl FnOnce(&Cx) -> R) -> Result<R, Stopped> {
        self.check()?;
        let frame = Frame::with_reserve(scratch, blaze_mem::Tag::Frames)
            .map_err(|_| Stopped::OutOfMemory { arena: self.span, asked: scratch })?;
        let placed = self.placed();
        let io = self.io();
        let seat = placed.child_seat(io.workers(), self.span);
        let child = Cx {
            deadline: self.deadline,
            // clone: local non-atomic Rc clone.
            cancel: self.cancel.clone(),
            span: self.span,
            plant: self.plant,
            class: self.class,
            placed: Placed::on(placed.worker.get(), placed.pool.clone(), placed.here, seat, Some(frame), scratch, io.clone()),
            path: self.path,
            end: None,
        };
        Ok(f(&child))
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
        self.placed.io()
    }

    /// A narrow `Send + Sync + Clone` handle for spawning root [`Cx`] values on background
    /// threads (e.g. tickers, status watchers, shared stores) without handing down the whole `Io`.
    pub fn spawner(&self) -> CxSpawn {
        CxSpawn::new(self.io().clone())
    }

    /// Whether this context's world is simulated (no real host OS/filesystem).
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn is_simulated(&self) -> bool {
        self.io().with_sim(|_| ()).is_some()
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
        self.charged(io, self.span, None, || self.wait_now(io, tag, ends))
    }

    /// Waits for any tag in `tags` to complete, returning `(index, done)` for the first ready
    /// op, or `Err(stopped)` without abandoning any tag (so linear callers can finish or cancel
    /// every still-in-flight `Pending` explicitly).
    pub(crate) fn wait_any(&self, io: &Io, tags: &[Tag]) -> Result<(usize, Done), Stopped> {
        self.charged(io, self.span, None, || self.wait_any_now(io, tags))
    }

    fn wait_any_now(&self, io: &Io, tags: &[Tag]) -> Result<(usize, Done), Stopped> {
        let deadline = match self.plant {
            Plant::WaitIgnoresDeadline => Deadline::NEVER,
            _ => self.deadline(),
        };
        self.waits();
        let mut idle = 0;
        loop {
            if let Some(found) = io.take_any(tags) {
                return Ok(found);
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

    /// `f` with the cell of the worker this task is on now held, `&mut` (D171,
    /// [`crate::workers`]): its registered slots ([`Worker::slot`]), its scratch frame, its
    /// meter, its trace rows and its random stream. The closure cannot wait: it is `Send`, so
    /// it captures no `&Cx`; what it returns cannot borrow the cell. The worker is found from
    /// the task's placement, read now, so a task that moved since its last call finds the
    /// worker it is on now.
    ///
    /// # Panics
    ///
    /// When the cell is held already (an `on_worker` reached from inside another's on the
    /// same cell), a slot's make panicked, or a thread outside the pool finds every caller
    /// cell leased: the task fails naming it (rule 20).
    pub fn on_worker<R>(&self, f: impl FnOnce(&mut Worker<'_>) -> R + Send) -> R {
        let workers = self.io().workers();
        let id = self.placed().cell(workers, self.span);
        workers.enter(id, f)
    }

    /// `wait` run, and the time it took on the world's clock charged to the worker this task
    /// is on as it returns, under `on` (`id`): every wait of bstd accounts itself this way
    /// (D171), whatever it ends in.
    pub(crate) fn charged<R>(&self, io: &Io, on: &'static str, id: Option<u64>, wait: impl FnOnce() -> R) -> R {
        let began = ns_of(io.now());
        let out = wait();
        let ended = ns_of(io.now()).max(began);
        self.on_worker(|w| charge_wait_at(w, on, id, began, ended, NoPlant));
        out
    }

    fn wait_now(&self, io: &Io, tag: Tag, ends: Ends) -> Result<Done, Stopped> {
        let deadline = match self.plant {
            Plant::WaitIgnoresDeadline => Deadline::NEVER,
            Plant::None | Plant::SubdirKeepsParent | Plant::OpenCreates | Plant::LexicalParent | Plant::UntilWidens | Plant::WaitIgnoresCancel | Plant::ArenaUnlimited | Plant::SyncSkipped | Plant::EnvOnlyReplaces | Plant::CancelSkipsWait | Plant::CloseSkipsSync | Plant::JoinDropsFailure | Plant::OffersIgnored | Plant::OnlyWidens | Plant::KvPutUnsynced | Plant::LocalDropsChunk | Plant::HttpOnlyIgnored | Plant::BudgetCheckedAfter | Plant::KeyIgnoresLast | Plant::MemoNeverEvicts | Plant::MemoSharesPoints | Plant::NestedUnderGrandparent | Plant::PublishBeforeRename | Plant::DurableSkipsNameSync | Plant::UnsyncedRenameSyncs | Plant::FlushedBeforeFlush | Plant::StaleFlushedCloned | Plant::ParJoinsInRunOrder | Plant::FreezeLeaks | Plant::SwapFreesAtOnce | Plant::FlightComputesTwice | Plant::ChanPassesCapacity | Plant::PublishOnceOverwrites | Plant::OrderTokenTwice | Plant::TaskMigrates | Plant::AncestorsUnbounded => self.deadline(),
        };
        self.waits();
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
    pub(crate) fn wait_for<T>(&self, io: &Io, ready: impl FnMut() -> Option<T>) -> Result<T, Stopped> {
        self.charged(io, self.span, None, || self.wait_for_now(io, ready))
    }

    fn wait_for_now<T>(&self, io: &Io, mut ready: impl FnMut() -> Option<T>) -> Result<T, Stopped> {
        let deadline = match self.plant {
            Plant::WaitIgnoresDeadline => Deadline::NEVER,
            Plant::None | Plant::SubdirKeepsParent | Plant::OpenCreates | Plant::LexicalParent | Plant::UntilWidens | Plant::WaitIgnoresCancel | Plant::ArenaUnlimited | Plant::SyncSkipped | Plant::EnvOnlyReplaces | Plant::CancelSkipsWait | Plant::CloseSkipsSync | Plant::JoinDropsFailure | Plant::OffersIgnored | Plant::OnlyWidens | Plant::KvPutUnsynced | Plant::LocalDropsChunk | Plant::HttpOnlyIgnored | Plant::BudgetCheckedAfter | Plant::KeyIgnoresLast | Plant::MemoNeverEvicts | Plant::MemoSharesPoints | Plant::NestedUnderGrandparent | Plant::PublishBeforeRename | Plant::DurableSkipsNameSync | Plant::UnsyncedRenameSyncs | Plant::FlushedBeforeFlush | Plant::StaleFlushedCloned | Plant::ParJoinsInRunOrder | Plant::FreezeLeaks | Plant::SwapFreesAtOnce | Plant::FlightComputesTwice | Plant::ChanPassesCapacity | Plant::PublishOnceOverwrites | Plant::OrderTokenTwice | Plant::TaskMigrates | Plant::AncestorsUnbounded => self.deadline(),
        };
        self.waits();
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
    /// The task is about to wait: at its first, a debug build marks the thread it is on, which
    /// is the one it started on (it has not waited yet, so it has not moved).
    fn waits(&self) {
        #[cfg(all(debug_assertions, not(blaze)))]
        if self.placed().thread.get().is_none() {
            self.placed().thread.set(Some(crate::rt::thread_mark()));
        }
    }

    /// Where the placement is updated at a resume (D171): the next `on_worker` reads the seat
    /// again, so a task moved here finds the worker it is on now. A simulated move changes the
    /// model worker below; a fiber layout's scheduler sets the seat to the worker it resumes
    /// the fiber on, here.
    fn resumed(&self) {
        if self.plant == Plant::TaskMigrates {
            let placed = self.placed();
            placed.worker.set(placed.worker.get() + 1);
        }
        #[cfg(all(debug_assertions, not(blaze)))]
        if Some(crate::rt::thread_mark()) != self.placed().thread.get() {
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
        self.placed().worker.get()
    }

    /// Takes the task's lock-order token: `false` when it is out already.
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub(crate) fn take_order(&self) -> bool {
        let was_out = self.placed().order_out.replace(true);
        !was_out || self.plant == Plant::OrderTokenTwice
    }

    /// Gives the task's lock-order token back.
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub(crate) fn give_order(&self) {
        self.placed().order_out.set(false);
    }

    /// Whether this context stopped already at `now`: then a call asks nothing of the driver.
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub(crate) fn stopped_by(&self, now: core::time::Duration) -> Result<(), Stopped> {
        if self.plant != Plant::WaitIgnoresCancel {
            self.check()?;
        }
        if let Some(deadline) = self.deadline
            && self.plant != Plant::WaitIgnoresDeadline
            && deadline.passed(now)
        {
            return Err(Stopped::Deadline { span: self.span, at: deadline.get() });
        }
        Ok(())
    }

    /// [`Cx::stopped_by`] at `io`'s now, read only when there is a deadline to compare it
    /// with: a context with none (a hello's root) asks the clock nothing.
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub(crate) fn stopped(&self, io: &Io) -> Result<(), Stopped> {
        match self.deadline {
            Some(_) => self.stopped_by(io.now()),
            None => match self.plant {
                Plant::WaitIgnoresCancel => Ok(()),
                _ => self.check(),
            },
        }
    }

    /// The context of a scope inside this task: its own cancellation, under this one's.
    pub(crate) fn scoped(&self) -> Cx {
        let outer = self.cancel.as_ref().map(|c| Arc::clone(c.arc()));
        Cx { cancel: Some(LocalCancel::new(Cancel::new(outer))), ..self.child() }
    }

    /// The context of a task of the scope `self` is: the scope's deadline and cancellation,
    /// its own name and frame.
    /// It is placed on `worker` and stays there (std.md, "Layouts").
    pub(crate) fn task(&self, name: &'static str, frame: Frame, worker: u32) -> Cx {
        let io = self.io();
        io.record_trace(name, RecordKind::Opened { parent: self.span });
        let seat = self.placed().child_seat(io.workers(), self.span);
        let placed = Placed::on(worker, self.placed().pool.clone(), self.placed().here, seat, Some(frame), 0, io.clone());
        Cx { span: name, placed, ..self.child() }
    }

    /// What of the context of a task of the scope `self` is crosses to the thread that will
    /// run it (std.md, "A `Seed`"): as [`Cx::task`], with the scope's `class`; `worker` is the
    /// placement model's, used if the task is grown outside the pool. The task's opening is
    /// recorded here, on the spawner, so the trace's order of openings is the spawner's.
    pub(crate) fn seed(&self, name: &'static str, frame: Frame, class: Class, worker: u32) -> Seed {
        let io = self.io();
        io.record_trace(name, RecordKind::Opened { parent: self.span });
        Seed {
            deadline: self.deadline,
            cancel: self.cancel.as_ref().map(|c| Arc::clone(c.arc())),
            span: name,
            frame,
            world: io.clone(),
            plant: self.plant,
            pool: self.placed().pool.clone(),
            class,
            worker,
            path: self.path,
        }
    }

    /// The pool its tasks are spawned on: none under `Inline`.
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub(crate) fn pool(&self) -> Option<&Pool> {
        self.placed().pool.as_ref()
    }

    /// Where the code holding this context runs, as its pool knows it.
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub(crate) fn here(&self) -> Here {
        self.placed().here
    }

    /// How many workers its scopes run tasks on: the pool's count, one under `Inline`. What a
    /// [`crate::task::PerWorker`] is made with.
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn workers(&self) -> u32 {
        self.layout().workers()
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
        match &self.placed().pool {
            Some(pool) => Layout::Threads { workers: pool.workers() },
            None => Layout::Inline,
        }
    }

    /// This context's cancellation, to be set from another thread (a task's failure).
    ///
    /// # Panics
    ///
    /// On a context with no cancellation (an `Inline` root, which nothing cancels): only a
    /// scope's contexts, and a pool's root, are cancelled.
    pub(crate) fn canceller(&self) -> Canceller {
        Canceller(Arc::clone(self.cancellation()))
    }

    fn cancellation(&self) -> &Arc<Cancel> {
        match &self.cancel {
            Some(cancel) => cancel.arc(),
            None => panic!("bstd::cx {}: cancelled outside a scope (a root is never cancelled; std.md, \"Cancellation\")", self.span),
        }
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
        self.cancellation().set.store(true, Ordering::Release);
    }

    /// The telemetry hook of a task's start, on the context the task runs in: its worker's cell
    /// counts it and, from `Spans`, makes it the worker's current task (what its waits' records
    /// name) and takes its snapshot. `None` on a world with no cells.
    pub(crate) fn task_began(&self, name: &'static str) -> Option<Begun> {
        let io = self.io();
        io.workers().telemetry_level()?;
        let wall = ns_of(io.now());
        let path = self.path;
        self.on_worker(|w| {
            let (tally, cell) = w.telemetry()?;
            let at = cell.snapshot(tally);
            Some(cell.task_open(name, &path, wall, at))
        })
    }

    /// The telemetry hook of a task's end (`failed`: its panic or error failed it), on the
    /// same context as its [`Cx::task_began`]: the pool counts it; from `Spans` its use is
    /// charged under its path and its record kept for the chain.
    pub(crate) fn task_ended(&self, begun: Option<Begun>, failed: bool) {
        let Some(begun) = begun else { return };
        let wall = ns_of(self.io().now());
        self.on_worker(|w| {
            if let Some((tally, cell)) = w.telemetry() {
                let now = cell.snapshot(tally);
                // A task's slot is its own cell's (began on this context's worker, which a
                // started task never leaves): a refusal is counted by the cell.
                cell.task_close(begun, wall, now, failed);
            }
        });
    }

    fn child(&self) -> Cx {
        Cx {
            deadline: self.deadline,
            // Zero atomic RMWs: both `LocalCancel` and `Placement` are local `Rc` clones.
            cancel: self.cancel.clone(),
            span: self.span,
            plant: self.plant,
            class: self.class,
            placed: self.placed.clone(),
            path: self.path,
            // The span's end is its own context's alone.
            end: None,
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

/// Narrow `Send + Sync + Clone` capability that only spawns root [`Cx`] values on another thread
/// (no `Fs`, `Env`, `Net`, `Clock`, `Out`, `Args`, `Host`, or `Rng`). Obtained from [`Cx::spawner`]
/// or [`Io::spawner`].
#[derive(Clone)]
pub struct CxSpawn {
    world: Io,
}

impl CxSpawn {
    pub(crate) fn new(world: Io) -> CxSpawn {
        CxSpawn { world }
    }

    /// Creates a root [`Cx`] on the calling thread with `budget` and `scratch` frame bytes.
    pub fn cx(&self, budget: core::time::Duration, scratch: usize) -> Result<Cx, Stopped> {
        self.world.cx(budget, scratch)
    }

    /// Whether the underlying world is simulated.
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn is_simulated(&self) -> bool {
        self.world.with_sim(|_| ()).is_some()
    }
}

impl core::fmt::Debug for CxSpawn {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("CxSpawn").finish_non_exhaustive()
    }
}
