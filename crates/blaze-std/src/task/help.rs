//! A join that helps, and a batch join in completion order (docs/v2/plan/design/parallel.md,
//! "End state", points 1, 2 and 8).
//!
//! **What it replaces.** A join of a task some worker had started parked its thread on the
//! scope's `Parker` at once when that thread was not a worker ([`Pool::wait`] runs queued tasks
//! only for `Here::Worker`). A command's thread is not a worker: it spawned a batch, then sat
//! parked in the first join while the rest of the batch waited in the injection queues.
//!
//! **The helping join** ([`finish`], which [`Task::join`] calls). While its task has not ended,
//! the joining thread runs queued tasks; it parks only when there is none it may run. The rule
//! that keeps that safe and bounded:
//!
//! - *Which tasks.* A worker runs its own queues, as before ([`Pool::wait`]). A thread that is
//!   not a worker takes only from the injection queues of its class ([`Pool::lend`]): tasks
//!   spawned by threads outside the pool, which are the tasks a joiner outside the pool could
//!   already run inline (`Queued::run_here`). It never steals from a worker's deque: a task
//!   spawned on a worker may rest on running on one (`PerWorker`, a pinned visit), and a thief
//!   outside the pool has no worker's slot. It never takes a reserved (`must_overlap`) task:
//!   those are in a mailbox, and the claim refuses a joiner besides (task/claim.rs).
//! - *Exactly once.* A helper starts a task by the same compare-and-swap as a joiner
//!   ([`take`]); a worker that pops the same pointer later loses the claim and only releases
//!   its reference.
//! - *Time.* Between two helped tasks the joiner looks at its own task and at its deadline. A
//!   helped task runs under its own context (its scope's deadline and cancellation), so it is
//!   bounded as on a worker; a join returns late by at most one helped task's run, and past the
//!   deadline nothing more is helped: the join goes to its deadline's path (`outlast`).
//! - *Stack.* A helped task may itself join and help. The frames of helping on one thread are
//!   counted ([`Depth`]); at [`HELP_DEPTH`] the thread parks instead of helping. No recursion
//!   without that bound.
//! - *Failure.* A helped task's error or panic is caught in its own body and kept in its own
//!   scope's tally (`Scope::pool_job`): it fails that task, never the helper.
//!
//! **A seat** (task/seat.rs). A joiner outside the pool takes one of the pool's seats for its
//! join's length ([`Pool::adopt`]) and with it also steals from the workers' deques of its
//! class ([`Pool::steal_seated`]), as a worker does; it gives the seat back before it parks.
//! With no seat free it helps from the injection queues only.
//!
//! **The batch join** ([`Batch`]). The tasks of one scope, spawned through the batch, each with
//! the index of its spawn; the batch is given its capacity at its start. [`Batch::join_each`]
//! hands each outcome to a callback as it ends, with its index in the same value ([`Joined`]):
//! the caller merges by index (or by a key in the result), never by arrival, so its output is
//! the same bytes under any schedule (rule 25; parallel.md point 8). Each task ends into the
//! batch's ring (task/ring.rs), so the join reads only what ended, never the open tasks. The
//! loop is [`drive`]: it delivers what the ring holds, helps as above, and parks only when the
//! scope's count of ended tasks has not moved since before that read and nothing can be run
//! here. One wake serves every task that ended before it.
//!
//! **No wake is lost** because the count of ended tasks is read BEFORE the ring: a task that
//! ends after the read moves the count, so the park's condition is already false; one that
//! ended before it is in the ring (the runner writes its slot, stores `DONE`, then counts, then
//! unparks: task/ring.rs `end_task`). The control [`HelpPlant::CountAfterSweep`] reads it after,
//! and the model finds the joiner asleep with a result ready; task/ring.rs's model plants the
//! runner's wake before its slot.
//!
//! **Counted** ([`Joins`], read as [`JoinCounts`] by [`Scope::join_counts`]): tasks run by the
//! joining thread as their own join, tasks it ran as a helper, tasks ended in all (the rest ran
//! on workers), parks, the time parked, and helps refused by the depth cap. A full pool shows
//! as few parks and a small parked time; an idle one as the reverse.
//!
//! Checked by blaze-check under `cfg(blaze_check)` (the `model_help_*` tests below), over the
//! real [`drive`], [`take`], [`Depth`] and `Claim`.

use core::{
    alloc::Allocator,
    cell::Cell,
    fmt,
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

use blaze_core::wait::Clock;
use blaze_own::Vec;

use super::{
    Held, Pending, Scope, Task, TaskError,
    claim::Claim,
    outlast,
    ring::{self, EndRing},
    threads::{self, Header, Pool, Ran, Seat, Tally},
};
use crate::cx::{Cx, Here};

/// The most frames of helping on one thread's stack: a join inside a helped task inside a join
/// counts two. At the cap a join parks instead of helping.
pub(crate) const HELP_DEPTH: u32 = 8;


/// A planted bug for the model's controls (rule 53).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum HelpPlant {
    None,
    /// A helper starts any task that has not ended, without the claim: a worker and a helper
    /// both run one task.
    #[cfg_attr(not(blaze_check), expect(dead_code, reason = "the model's control"))]
    HelperSkipsClaim,
    /// The batch join reads the count of ended tasks after it delivers, not before: a task
    /// that ends between the two is never seen, and the joiner parks with its result ready.
    #[cfg_attr(not(blaze_check), expect(dead_code, reason = "the model's control"))]
    CountAfterSweep,
}

/// A helper takes a task to run it: `false` when another took it, and always for a reserved
/// task. The one compare-and-swap a joiner uses, so a task runs exactly once whoever starts it.
pub(crate) fn take(claim: &Claim, plant: HelpPlant) -> bool {
    match plant {
        HelpPlant::None | HelpPlant::CountAfterSweep => claim.by_joiner(),
        HelpPlant::HelperSkipsClaim => !claim.done(),
    }
}

/// How many frames of helping are on one thread's stack.
pub(crate) struct Depth(Cell<u32>);

impl Depth {
    pub(crate) const fn new() -> Depth {
        Depth(Cell::new(0))
    }

    /// One more frame, if fewer than `cap` are open: whether it was entered. Each entry is
    /// left once ([`Depth::leave`]).
    pub(crate) fn enter(&self, cap: u32) -> bool {
        let now = self.0.get();
        if now >= cap {
            return false;
        }
        self.0.set(now + 1);
        true
    }

    /// The frame entered last is left.
    pub(crate) fn leave(&self) {
        let now = self.0.get();
        assert!(now != 0, "bstd::task::help: a helping frame left that was never entered");
        self.0.set(now - 1);
    }

    /// The frames open now.
    pub(crate) fn open(&self) -> u32 {
        self.0.get()
    }
}

/// This thread's helping frames. A `Cell` read and written around a helped task's run: no
/// borrow of it is held across the run or a wait (rule 27), and a started task never changes
/// its thread.
#[thread_local]
static DEPTH: Depth = Depth::new();

/// Leaves this thread's helping frame when the helped task's run is over, however it ends.
struct Leave;

impl Drop for Leave {
    fn drop(&mut self) {
        DEPTH.leave();
    }
}

/// What a scope's joins did, counted where they did it: the scope's thread writes all but
/// `ended`, which each task's runner adds to as it ends.
pub(crate) struct Joins {
    /// The scope's pool tasks that ended (run anywhere, or dropped unrun): only grows, so a
    /// batch join's park waits for it to move.
    pub(crate) ended: AtomicU64,
    /// Tasks the joining thread ran as their own join, no worker having started them.
    pub(crate) by_joiner: AtomicU64,
    /// Tasks the joining thread ran as a helper while it waited for another.
    pub(crate) by_helper: AtomicU64,
    /// Times a join parked, having nothing to run.
    pub(crate) parks: AtomicU64,
    /// Nanoseconds joins spent parked (their spin before the park included).
    pub(crate) parked_ns: AtomicU64,
    /// Times a join did not help because [`HELP_DEPTH`] frames were open.
    pub(crate) capped: AtomicU64,
}

impl Joins {
    pub(crate) const fn new() -> Joins {
        Joins {
            ended: AtomicU64::new(0),
            by_joiner: AtomicU64::new(0),
            by_helper: AtomicU64::new(0),
            parks: AtomicU64::new(0),
            parked_ns: AtomicU64::new(0),
            capped: AtomicU64::new(0),
        }
    }

    fn counts(&self) -> JoinCounts {
        JoinCounts {
            ended: self.ended.load(Ordering::Acquire),
            by_joiner: self.by_joiner.load(Ordering::Relaxed),
            by_helper: self.by_helper.load(Ordering::Relaxed),
            parks: self.parks.load(Ordering::Relaxed),
            capped: self.capped.load(Ordering::Relaxed),
            parked: Duration::from_nanos(self.parked_ns.load(Ordering::Relaxed)),
        }
    }
}

/// What a scope's joins did so far ([`Scope::join_counts`]): for a profile's row, to show
/// whether the pool was full (few parks, most tasks by workers) or the joiner idle.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct JoinCounts {
    /// The scope's queued tasks that ended, on any thread (one dropped unrun counts).
    pub ended: u64,
    /// Of them, tasks the joining thread ran as their own join.
    pub by_joiner: u64,
    /// Tasks the joining thread ran as a helper while it waited. They are tasks of this thread's
    /// scopes: of this scope when it is the only one the thread has open.
    pub by_helper: u64,
    /// Times a join parked with nothing to run.
    pub parks: u64,
    /// Times a join parked instead of helping because the depth cap was reached.
    pub capped: u64,
    /// The time joins spent waiting with nothing to run.
    pub parked: Duration,
}

impl JoinCounts {
    /// The ended tasks the joining thread did not run: those that ran on workers. Exact when
    /// this scope is the only one its thread has open (see [`JoinCounts::by_helper`]).
    pub fn by_workers(&self) -> u64 {
        self.ended.saturating_sub(self.by_joiner).saturating_sub(self.by_helper)
    }
}

impl fmt::Display for JoinCounts {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "tasks ended {}: by workers {}, by their joiner {}, by a helping join {}; parks {} ({} ns), depth-capped {}",
            self.ended,
            self.by_workers(),
            self.by_joiner,
            self.by_helper,
            self.parks,
            self.parked.as_nanos(),
            self.capped,
        )
    }
}

/// What one try at helping did.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Helped {
    /// A queued task was run here to its end.
    Ran,
    /// Nothing this thread may run is queued (or its deadline passed).
    Nothing,
    /// [`HELP_DEPTH`] frames of helping are open on this thread: it parks instead.
    Capped,
}

/// Runs one queued task on `cx`'s thread, for a join of a scope whose tally is `tally`: from
/// the injection queues, then, with a `seat`, from the workers' deques.
fn help_one(pool: &Pool, tally: &Tally, cx: &Cx, seat: Option<&Seat<'_>>) -> Helped {
    let here = cx.here();
    // A worker's own queues are run by `Pool::wait`, as before; this is the thread outside.
    if !matches!(here, Here::Outside) {
        return Helped::Nothing;
    }
    if cx.deadline().passed(cx.io().now()) {
        return Helped::Nothing;
    }
    if DEPTH.open() >= HELP_DEPTH {
        tally.joins().capped.fetch_add(1, Ordering::Relaxed);
        return Helped::Capped;
    }
    // A bounded look: each queued pointer is taken out once, so the loop ends when the
    // injection queues of this class, and with a seat the deques, are empty.
    loop {
        let job = match pool.lend(cx.class()) {
            Some(job) => job,
            None => match seat.and_then(|seat| pool.steal_seated(seat, cx.class())) {
                Some(job) => job,
                None => return Helped::Nothing,
            },
        };
        assert!(DEPTH.enter(HELP_DEPTH), "bstd::task::help: the depth was checked above");
        let leave = Leave;
        let ran = pool.run_lent(here, job);
        drop(leave);
        if ran {
            tally.joins().by_helper.fetch_add(1, Ordering::Relaxed);
            return Helped::Ran;
        }
        // Another thread had started it: the pointer was stale. Look again.
    }
}

/// Waits on `cx`'s thread until `ready`, by the join's deadline path (`outlast`: the span
/// `bstd.task.join`, the scope's deadline, then the abandon wait), and counts the wait.
fn park(cx: &Cx, pool: &Pool, tally: &Tally, task: &'static str, ready: impl FnMut() -> bool) {
    let clock = *pool.clock();
    let from = clock.now();
    outlast(cx, pool, tally, task, cx.deadline(), ready);
    let waited = clock.now().saturating_sub(from);
    let joins = tally.joins();
    joins.parks.fetch_add(1, Ordering::Relaxed);
    joins.parked_ns.fetch_add(u64::try_from(waited.as_nanos()).unwrap_or(u64::MAX), Ordering::Relaxed);
}

/// A queued task's outcome for its join: run here if no worker started it; else, while it has
/// not ended, queued tasks are run here (the module's rule), and the thread parks only when
/// there is none.
pub(super) fn finish<T>(pending: &Pending<T>, cx: &Cx, name: &'static str) -> Ran<T> {
    let queued = &pending.queued;
    let tally = queued.tally();
    if let Some(ran) = queued.run_here(cx.here()) {
        tally.joins().by_joiner.fetch_add(1, Ordering::Relaxed);
        return ran;
    }
    let seat = if matches!(cx.here(), Here::Outside) { pending.pool.adopt() } else { None };
    while !queued.done() {
        match help_one(&pending.pool, tally, cx, seat.as_ref()) {
            Helped::Ran => {}
            Helped::Nothing | Helped::Capped => break,
        }
    }
    // Given back before the park: a parked joiner steals nothing.
    drop(seat);
    if !queued.done() {
        park(cx, &pending.pool, tally, name, || queued.done());
    }
    queued.take()
}

/// What a batch join needs of its tasks and its scope: the seam the model drives [`drive`]
/// through (rule 78).
pub(crate) trait Joining {
    /// The tasks not yet delivered.
    fn open(&self) -> usize;
    /// Hands on the outcome of every open task found ended now (the batch's ring): how many.
    fn deliver_ended(&mut self) -> usize;
    /// How many of the scope's tasks have ended: only grows.
    fn ended(&self) -> u64;
    /// Runs one queued task here.
    fn help(&mut self) -> Helped;
    /// Waits until [`Joining::ended`] is no longer `swept`.
    fn park(&mut self, swept: u64);
}

/// The batch join's loop (the module's doc): until no task is open.
pub(crate) fn drive<J: Joining>(joining: &mut J, plant: HelpPlant) {
    loop {
        // Read before the delivery: a task that ends after this read moves the count, so the
        // park below does not wait for it.
        let mut swept = joining.ended();
        joining.deliver_ended();
        if plant == HelpPlant::CountAfterSweep {
            swept = joining.ended();
        }
        if joining.open() == 0 {
            return;
        }
        match joining.help() {
            Helped::Ran => {}
            Helped::Nothing | Helped::Capped => {
                if joining.ended() == swept {
                    joining.park(swept);
                }
            }
        }
    }
}

/// One task's outcome from a batch join, with the index its spawn was given: the two travel
/// together, so a merge by index cannot lose it.
#[derive(Debug)]
pub struct Joined<T> {
    /// Which spawn of its batch it was, counted from 0 ([`Batch::may_overlap`]'s answer).
    pub index: u32,
    /// The task's name in its scope.
    pub name: &'static str,
    /// What it returned, or how it failed.
    pub result: Result<T, TaskError>,
}

/// Why a batch took no more tasks, or was not made.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BatchError {
    /// The batch's allocator had no room for its ring and handles (`Scope::batch`), or for one
    /// more handle: the task was not spawned.
    NoRoom { scope: &'static str, task: &'static str, held: u32 },
    /// The batch holds `u32::MAX` tasks: the task was not spawned.
    TooMany { scope: &'static str, task: &'static str },
    /// The batch holds the capacity it was made with: the task was not spawned (rule
    /// bounded-progress: its ring has that many slots).
    PastCapacity { scope: &'static str, task: &'static str, capacity: u32 },
}

impl fmt::Display for BatchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BatchError::NoRoom { scope, task, held } => {
                write!(f, "bstd::task::Batch of scope {scope}: no room for the handle of task {task} after {held} (the batch's allocator refused; the task was not spawned)")
            }
            BatchError::TooMany { scope, task } => {
                write!(f, "bstd::task::Batch of scope {scope}: task {task} would be past the last index, {} (the task was not spawned)", u32::MAX)
            }
            BatchError::PastCapacity { scope, task, capacity } => {
                write!(f, "bstd::task::Batch of scope {scope}: task {task} would be past the capacity {capacity} the batch was made with (rule bounded-progress; the task was not spawned)")
            }
        }
    }
}

impl core::error::Error for BatchError {}

/// One open task of a batch.
struct Entry<'s, T> {
    index: u32,
    task: Task<'s, T>,
}

/// Tasks of one scope to be joined together, in the order they end ([`Scope::batch`]). Every
/// task spawned through it must be finished by [`Batch::join_each`], before its scope is.
#[must_use = "a batch is finished with join_each(cx, ..)"]
pub struct Batch<'s, 'p, T, A: Allocator> {
    scope: &'s Scope<'p>,
    /// By spawn index; `None` once delivered.
    open: Vec<Option<Entry<'s, T>>, A>,
    /// Spawns that ended with no job (run inline at the spawn): delivered from here.
    inline: Vec<u32, A>,
    next: u32,
    /// Declared after `open`: dropped after every handle, so every task ends into a live ring.
    ring: EndRing<Header, A>,
}

impl<'p> Scope<'p> {
    /// An empty batch of this scope's tasks, of at most `capacity` (rule bounded-progress): its
    /// ring and handles are made now, in `alloc`, and none grows after.
    pub fn batch<T, A: Allocator + Clone>(&self, capacity: u32, alloc: A) -> Result<Batch<'_, 'p, T, A>, BatchError> {
        let no_room = BatchError::NoRoom { scope: self.live.cx.name(), task: "(the batch's ring and handles)", held: 0 };
        let ring = EndRing::try_new_in(capacity, alloc.clone()).map_err(|_| no_room)?;
        let mut open = Vec::new_in(alloc.clone());
        open.try_reserve(capacity as usize).map_err(|_| no_room)?;
        let mut inline = Vec::new_in(alloc);
        inline.try_reserve(capacity as usize).map_err(|_| no_room)?;
        Ok(Batch { scope: self, open, inline, next: 0, ring })
    }

    /// What this scope's joins did so far (the module's doc, "Counted").
    pub fn join_counts(&self) -> JoinCounts {
        self.live.shared.as_ref().map_or_else(JoinCounts::default, |tally| tally.joins().counts())
    }
}

impl<'s, 'p, T: Send, A: Allocator> Batch<'s, 'p, T, A> {
    /// Spawns `task` as [`Scope::may_overlap`] does and keeps its handle: its index in this
    /// batch, counted from 0 in spawn order, which [`Joined::index`] gives back.
    #[track_caller]
    pub fn may_overlap<E: fmt::Display>(
        &mut self,
        name: &'static str,
        scratch: usize,
        task: impl FnOnce(&Cx) -> Result<T, E> + Send,
    ) -> Result<u32, BatchError> {
        let scope: &'s Scope<'p> = self.scope;
        let index = self.next;
        let Some(next) = index.checked_add(1) else {
            return Err(BatchError::TooMany { scope: scope.live.cx.name(), task: name });
        };
        // Room first (made at the batch's start for `capacity`): a spawned task's handle is
        // linear, so nothing is spawned that cannot be kept, and its ring has its slot.
        if index >= self.ring.capacity() {
            return Err(BatchError::PastCapacity { scope: scope.live.cx.name(), task: name, capacity: self.ring.capacity() });
        }
        if self.open.try_reserve(1).is_err() || self.inline.try_reserve(1).is_err() {
            return Err(BatchError::NoRoom { scope: scope.live.cx.name(), task: name, held: index });
        }
        // The job this spawn makes takes the ring (task/ring.rs "Which tasks").
        let outer = ring::install(self.ring.end());
        let task = scope.may_overlap(name, scratch, task);
        let in_ring = ring::restore(outer);
        match &task.held {
            Held::Queued(pending) => {
                assert!(in_ring, "bstd::task::Batch of scope {}: task {name}'s job did not take the batch's ring", scope.live.cx.name());
                pending.queued.set_tag(index);
            }
            Held::Ended(_) => {
                assert!(!in_ring, "bstd::task::Batch of scope {}: task {name} ended inline but a job took the batch's ring", scope.live.cx.name());
                self.inline.push(index);
            }
        }
        self.open.push(Some(Entry { index, task }));
        self.next = next;
        Ok(index)
    }

    /// The tasks spawned through it.
    pub fn len(&self) -> u32 {
        self.next
    }

    /// Whether no task was spawned through it.
    pub fn is_empty(&self) -> bool {
        self.next == 0
    }

    /// Finishes every task of the batch: `each` gets each outcome once, in the order they are
    /// found ended (NOT spawn order: merge by [`Joined::index`]). The calling thread, the
    /// scope's, runs queued tasks while it waits and parks only with nothing to run (the
    /// module's doc), until `cx`'s deadline and the abandon wait, as [`Task::join`].
    pub fn join_each(self, cx: &Cx, each: impl FnMut(Joined<T>)) {
        let Batch { scope, open, inline, next, ring } = self;
        let seat = match (cx.here(), scope.live.cx.pool()) {
            (Here::Outside, Some(pool)) => pool.adopt(),
            _ => None,
        };
        let mut run = Run { scope, cx, open, left: next as usize, inline, read: 0, seat, each, ring };
        drive(&mut run, HelpPlant::None);
    }
}

/// A batch being joined on the scope's thread.
struct Run<'r, 's, 'p, T, A: Allocator, F> {
    scope: &'s Scope<'p>,
    cx: &'r Cx,
    open: Vec<Option<Entry<'s, T>>, A>,
    /// The tasks not yet delivered.
    left: usize,
    inline: Vec<u32, A>,
    /// The ring's next slot to read.
    read: u32,
    /// The joiner's seat, if it is outside the pool and one was free.
    seat: Option<Seat<'s>>,
    each: F,
    /// After `open`, as in `Batch`.
    ring: EndRing<Header, A>,
}

impl<T, A: Allocator, F: FnMut(Joined<T>)> Run<'_, '_, '_, T, A, F> {
    /// Hands on task `index`'s outcome: it ended.
    fn deliver(&mut self, index: u32) {
        let scope = self.scope.live.cx.name();
        let Some(Entry { index, task }) = self.open.get_mut(index as usize).and_then(Option::take) else {
            panic!("bstd::task::Batch of scope {scope}: the ring gave task {index}, which is not open in this batch");
        };
        let Task { name, held, unfinished, .. } = task;
        unfinished.finished();
        let result = match held {
            Held::Ended(outcome) => outcome,
            // Ended (it was in the ring): its result is taken, and the handle's drop finds it so.
            Held::Queued(pending) => Scope::outcome(pending.queued.take()),
        };
        self.left -= 1;
        (self.each)(Joined { index, name, result });
    }
}

impl<T, A: Allocator, F: FnMut(Joined<T>)> Joining for Run<'_, '_, '_, T, A, F> {
    fn open(&self) -> usize {
        self.left
    }

    fn deliver_ended(&mut self) -> usize {
        let mut delivered = 0;
        while let Some(index) = self.inline.pop() {
            self.deliver(index);
            delivered += 1;
        }
        // A slot is taken only once its task is done (task/ring.rs `EndRing::next`).
        // SAFETY (in the closure): the job's handle is in `open` until delivered, so it lives.
        #[expect(unsafe_code, reason = "reading a live job's DONE: the SAFETY above")]
        let done = |header| unsafe { threads::done_of(header) };
        while let Some(header) = self.ring.next(&mut self.read, done) {
            // SAFETY: the job's handle is in `open` until it is delivered, so its header lives.
            #[expect(unsafe_code, reason = "reading a live job's batch index: the SAFETY above")]
            let index = unsafe { threads::tag_of(header) };
            self.deliver(index);
            delivered += 1;
        }
        delivered
    }

    fn ended(&self) -> u64 {
        self.scope.live.shared.as_ref().map_or(0, |tally| tally.joins().ended.load(Ordering::Acquire))
    }

    fn help(&mut self) -> Helped {
        let live = &self.scope.live;
        match (live.cx.pool(), &live.shared) {
            (Some(pool), Some(tally)) => help_one(pool, tally, self.cx, self.seat.as_ref()),
            // No pool: every task ran inline at its spawn, so none is open to wait for.
            (None, _) | (_, None) => Helped::Nothing,
        }
    }

    fn park(&mut self, swept: u64) {
        let live = &self.scope.live;
        let (Some(pool), Some(tally)) = (live.cx.pool(), &live.shared) else {
            panic!("bstd::task::Batch of scope {}: a task is open with no pool to wait on", live.cx.name());
        };
        // The seat is given back for the park and taken again after: a parked joiner steals
        // nothing.
        let seated = self.seat.take().is_some();
        park(self.cx, pool, tally, live.last.get(), || tally.joins().ended.load(Ordering::Acquire) != swept);
        if seated {
            self.seat = pool.adopt();
        }
    }
}

impl<T, A: Allocator> fmt::Debug for Batch<'_, '_, T, A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Batch").field("scope", &self.scope.live.cx.name()).field("spawned", &self.next).field("capacity", &self.ring.capacity()).finish()
    }
}

/// On one thread, with no pool: the depth cap and the loop's order.
#[cfg(all(test, not(blaze_check)))]
mod tests {
    use super::{Depth, HelpPlant, Helped, Joining, drive};
    use crate::task::Global;
    use blaze_own::Vec;

    /// Prints `test task::help::tests::the_depth_cap_refuses_the_frame_past_it ... ok`.
    #[test]
    fn the_depth_cap_refuses_the_frame_past_it() {
        let depth = Depth::new();
        assert!(depth.enter(2) && depth.enter(2), "two frames under a cap of two");
        assert!(!depth.enter(2), "a third frame was entered past the cap");
        assert_eq!(depth.open(), 2, "a refused frame was counted");
        depth.leave();
        assert!(depth.enter(2), "a left frame's room was not given back");
    }

    /// Tasks nobody else runs: `help` runs the last unstarted one unless the cap is reached.
    struct Alone {
        open: Vec<u32, Global>,
        ran: Vec<bool, Global>,
        delivered: Vec<u32, Global>,
        depth: Depth,
        cap: u32,
        parks: u32,
        helped: u32,
    }

    impl Alone {
        fn new(tasks: u32, cap: u32) -> Alone {
            let mut open = Vec::new_in(Global);
            let mut ran = Vec::new_in(Global);
            for index in 0..tasks {
                open.push(index);
                ran.push(false);
            }
            Alone { open, ran, delivered: Vec::new_in(Global), depth: Depth::new(), cap, parks: 0, helped: 0 }
        }
    }

    impl Joining for Alone {
        fn open(&self) -> usize {
            self.open.len()
        }

        /// A sweep: this test has no ring.
        fn deliver_ended(&mut self) -> usize {
            let mut delivered = 0;
            let mut at = 0;
            while at < self.open.len() {
                if self.ran[self.open[at] as usize] {
                    let index = self.open.swap_remove(at);
                    self.delivered.push(index);
                    delivered += 1;
                } else {
                    at += 1;
                }
            }
            delivered
        }

        fn ended(&self) -> u64 {
            self.ran.iter().filter(|ran| **ran).count() as u64
        }

        fn help(&mut self) -> Helped {
            if !self.depth.enter(self.cap) {
                return Helped::Capped;
            }
            let next = self.ran.iter().rposition(|ran| !*ran);
            self.depth.leave();
            match next {
                Some(at) => {
                    self.ran[at] = true;
                    self.helped += 1;
                    Helped::Ran
                }
                None => Helped::Nothing,
            }
        }

        /// Nobody else will end a task: the "worker" of this test runs the first unstarted one
        /// at each park, so a park is where a capped joiner makes progress.
        fn park(&mut self, _swept: u64) {
            self.parks += 1;
            let at = self.ran.iter().position(|ran| !*ran).expect("a park with every task ended is a hang");
            self.ran[at] = true;
        }
    }

    /// Prints `test task::help::tests::a_batch_delivers_every_index_once_and_helps_without_parking ... ok`.
    #[test]
    fn a_batch_delivers_every_index_once_and_helps_without_parking() {
        let mut alone = Alone::new(200, 8);
        drive(&mut alone, HelpPlant::None);
        let mut seen = alone.delivered.to_vec();
        seen.sort_unstable();
        assert!(seen.iter().copied().eq(0..200), "every index exactly once");
        assert_eq!((alone.helped, alone.parks), (200, 0), "the joiner ran every task and never parked");
    }

    /// Prints `test task::help::tests::at_the_depth_cap_the_join_parks_instead_of_helping ... ok`.
    #[test]
    fn at_the_depth_cap_the_join_parks_instead_of_helping() {
        let mut alone = Alone::new(5, 0);
        drive(&mut alone, HelpPlant::None);
        let mut seen = alone.delivered.to_vec();
        seen.sort_unstable();
        assert!(seen.iter().copied().eq(0..5), "every index exactly once");
        assert_eq!((alone.helped, alone.parks), (0, 5), "a capped joiner helped, or did not park once a task");
    }
}

/// Under the model checker (`RUSTFLAGS="--cfg blaze_check" cargo test -p blaze-std --release
/// model_help`): a batch of two tasks, one worker's thread taking them in order while the
/// scope's thread joins the batch and helps. Every task runs exactly once, every index is
/// delivered exactly once and only after its task ended, and the joiner never sleeps with a
/// result ready (the model reports a deadlock if it does).
#[cfg(all(test, blaze_check))]
mod model {
    use core::panic::AssertUnwindSafe;

    use blaze_check::{
        sync::atomic::{AtomicU32, AtomicU64, Ordering},
        thread,
    };
    use blaze_own::{Arc, Vec};

    use super::{Depth, HelpPlant, Helped, Joining, drive, take};
    use crate::{
        rt::catch_unwind,
        task::{
            Global,
            claim::{Claim, ClaimPlant, Start},
        },
    };

    const TASKS: usize = 2;

    struct World {
        claims: [Claim; TASKS],
        ran: [AtomicU32; TASKS],
        /// `Joins::ended`: added to after the claim's `DONE`, before the unpark (`Tally::ended`).
        ended: AtomicU64,
    }

    impl World {
        /// The runner's end of task `at`, in `threads::run`'s order.
        fn run(&self, at: usize) {
            self.ran[at].fetch_add(1, Ordering::Relaxed);
            self.claims[at].finish();
            self.ended.fetch_add(1, Ordering::AcqRel);
        }
    }

    struct Sim {
        world: Arc<World, Global>,
        open: Vec<u32, Global>,
        delivered: Vec<u32, Global>,
        /// The helper looks from the newest task down; each position once.
        next: usize,
        depth: Depth,
        cap: u32,
        helped: u32,
        plant: HelpPlant,
    }

    impl Joining for Sim {
        fn open(&self) -> usize {
            self.open.len()
        }

        /// A sweep over the claims: this model is of the helper and the count (task/ring.rs's
        /// model is of the ring).
        fn deliver_ended(&mut self) -> usize {
            let mut delivered = 0;
            let mut at = 0;
            while at < self.open.len() {
                if self.world.claims[self.open[at] as usize].done() {
                    let index = self.open.swap_remove(at);
                    assert!(self.world.claims[index as usize].done(), "a result was delivered before its task ended");
                    self.delivered.push(index);
                    delivered += 1;
                } else {
                    at += 1;
                }
            }
            delivered
        }

        fn ended(&self) -> u64 {
            self.world.ended.load(Ordering::Acquire)
        }

        fn help(&mut self) -> Helped {
            if !self.depth.enter(self.cap) {
                return Helped::Capped;
            }
            let mut helped = Helped::Nothing;
            while self.next > 0 {
                self.next -= 1;
                if take(&self.world.claims[self.next], self.plant) {
                    self.world.run(self.next);
                    self.helped += 1;
                    helped = Helped::Ran;
                    break;
                }
            }
            self.depth.leave();
            helped
        }

        /// `Pool::wait`'s loop on the scope's `Parker`: a token, so a wake before it is kept.
        fn park(&mut self, swept: u64) {
            while self.world.ended.load(Ordering::Acquire) == swept {
                thread::park();
            }
        }
    }

    /// The batch joined beside one worker: how many tasks the joiner ran as a helper.
    fn batch(plant: HelpPlant, cap: u32) -> u32 {
        let claim = || Claim::new(Start::Open, ClaimPlant::None);
        let world = Arc::new_in(World { claims: [claim(), claim()], ran: [AtomicU32::new(0), AtomicU32::new(0)], ended: AtomicU64::new(0) }, Global);
        let joiner = thread::current();
        let worker = {
            let world = Arc::clone(&world);
            thread::spawn(move || {
                for at in 0..TASKS {
                    if world.claims[at].by_worker() {
                        world.run(at);
                        // `Tally::ended`: the scope's thread woken after the count.
                        joiner.unpark();
                    }
                }
            })
        };
        let mut open = Vec::new_in(Global);
        for index in 0..TASKS {
            open.push(u32::try_from(index).expect("two tasks"));
        }
        let mut sim = Sim { world: Arc::clone(&world), open, delivered: Vec::new_in(Global), next: TASKS, depth: Depth::new(), cap, helped: 0, plant };
        drive(&mut sim, plant);
        worker.join().expect("the worker ends");
        for at in 0..TASKS {
            assert_eq!(world.ran[at].load(Ordering::Relaxed), 1, "a task ran other than once");
        }
        let mut seen = [0u32; TASKS];
        for index in sim.delivered.iter() {
            seen[*index as usize] += 1;
        }
        assert_eq!(seen, [1; TASKS], "an index was delivered other than once");
        sim.helped
    }

    fn said(report: &(dyn core::any::Any + Send)) -> &str {
        report
            .downcast_ref::<alloc::string::String>()
            .map(alloc::string::String::as_str)
            .or_else(|| report.downcast_ref::<&'static str>().copied())
            .expect("the model's report is text")
    }

    /// (i), (ii), (iii). Prints
    /// `test task::help::model::model_help_every_task_runs_once_and_every_index_is_delivered_once ... ok`.
    #[test]
    fn model_help_every_task_runs_once_and_every_index_is_delivered_once() {
        blaze_check::model(|| {
            batch(HelpPlant::None, 8);
        });
    }

    /// (i)'s control: the interleaving where the worker claims a task and the helper, not
    /// asking the claim, runs it again. Prints
    /// `test task::help::model::model_help_catches_a_helper_and_a_worker_both_running_one_task ... ok`.
    #[test]
    fn model_help_catches_a_helper_and_a_worker_both_running_one_task() {
        let found = catch_unwind(AssertUnwindSafe(|| {
            blaze_check::model(|| {
                batch(HelpPlant::HelperSkipsClaim, 8);
            });
        }));
        let report = found.err().expect("the model must find the task run twice");
        let text = said(&*report);
        assert!(text.contains("a task ran other than once"), "the model found something else: {text}");
    }

    /// (ii)'s control: a task ends between the sweep and the count's read, so its wake is for
    /// a count the joiner already holds, and the joiner parks for good with the result ready: a
    /// deadlock to the model. Prints
    /// `test task::help::model::model_help_catches_a_wake_lost_between_the_sweep_and_the_park ... ok`.
    #[test]
    fn model_help_catches_a_wake_lost_between_the_sweep_and_the_park() {
        let found = catch_unwind(AssertUnwindSafe(|| {
            blaze_check::model(|| {
                batch(HelpPlant::CountAfterSweep, 8);
            });
        }));
        assert!(found.is_err(), "the model must find the joiner asleep with a result ready");
    }

    /// (iv). With no helping frame allowed the joiner runs nothing and parks for the worker,
    /// and the batch still ends with every index delivered once. Prints
    /// `test task::help::model::model_help_at_the_depth_cap_the_joiner_parks_and_runs_nothing ... ok`.
    #[test]
    fn model_help_at_the_depth_cap_the_joiner_parks_and_runs_nothing() {
        blaze_check::model(|| {
            let helped = batch(HelpPlant::None, 0);
            assert_eq!(helped, 0, "a joiner at the depth cap ran a task");
        });
    }
}
