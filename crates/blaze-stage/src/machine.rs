//! THE DRIVER of one phase, as a sans-IO state machine.
//!
//! States: [`State::Planned`] → [`State::Running`] → [`State::Committing`] → [`State::Merged`],
//! or [`State::Stopped`] (a stop: what finished is committed by key, nothing is merged), or
//! [`State::Refused`] (the commit failed or missed its deadline: nothing is merged).
//!
//! The thin driver (the engine's, in blaze-cli) feeds [`Event`]s with the request clock's `now`
//! to [`Phase::step`] and carries out each [`Effect`] [`Phase::next_effect`] gives, in order:
//! start a task on a worker with a fresh [`crate::cx::TaskCx`], reset a worker's frame, commit
//! the puts [`Phase::commits`] lists, merge [`Phase::merge_order`] into the engine, report, drop
//! the phase's arena. Between events it waits, with a deadline (rule 18), until
//! [`Phase::next_deadline`], and then feeds [`Event::Deadline`].
//!
//! What the driver alone decides, so no stage can get it wrong:
//! - a task's end ([`End`]): a stage's [`Outcome`] is checked against what its context saw (a
//!   column refused, a read no key covers, a stop), and a `Made` with an `Unkept` input is
//!   `Unkept`; a task whose input failed is [`End::Waited`], naming the ROOT failure;
//! - a panic or a stall fails its own task with a typed [`Cause`] (rule 20), never the phase;
//! - after a stop no task starts;
//! - the merge and the commit are in task order whatever order tasks finished in (rule 25);
//! - the merge comes after the commit finished (nothing is readable before its bytes are kept);
//! - a worker's frame is reset after every task it ran.

use core::mem::size_of;

use blaze_int::{idx, len32};
use blaze_own::{AVec, Alloc, Vec};

use crate::cx::TaskRanges;
use crate::plant::{self, Plant};
use crate::proto::{Code, Column, Deps, FAIL_MOST, Halt, OUT_MOST, Outcome, Span, Stopped, TaskFail, TaskOut, TaskRef, WorkerRef};

/// The phase's deadlines, in the request clock's nanoseconds.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Limits {
    /// A task running longer than this fails as [`Cause::Stalled`] and its worker is lost.
    pub stall_after: u64,
    /// A commit not done this long after it was asked refuses the phase.
    pub commit_within: u64,
}

/// Why a task failed, as the driver keeps it: the stage's own [`TaskFail`], or one the driver saw.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Cause<F> {
    Stage(F),
    /// It panicked (the thin driver caught the unwind and said so).
    Panicked,
    /// It ran past the phase's [`Limits::stall_after`] (no time is kept in an end: an end is a
    /// function of the plan, never of the schedule; the row has the times).
    Stalled,
    /// A column of its lane was full.
    Full { column: Column, cap: u32 },
    /// Every worker was lost to a stall before it could start.
    NoWorker,
}

/// How a task ended: exactly one of these, once (law a).
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum End<O, F> {
    Made(O),
    Unkept(O),
    Failed(Cause<F>),
    /// Not run: its input `on` failed or waits in turn; `root` is the task that truly failed.
    Waited { on: TaskRef, root: TaskRef },
    /// It saw the stop and returned before its end.
    Stopped,
    /// Not started: the command stopped first.
    NotRun,
}

/// An [`End`]'s kind, for counts and rows.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EndKind {
    Made,
    Unkept,
    Failed,
    Waited,
    Stopped,
    NotRun,
}

impl<O, F> End<O, F> {
    pub fn kind(&self) -> EndKind {
        match self {
            End::Made(_) => EndKind::Made,
            End::Unkept(_) => EndKind::Unkept,
            End::Failed(_) => EndKind::Failed,
            End::Waited { .. } => EndKind::Waited,
            End::Stopped => EndKind::Stopped,
            End::NotRun => EndKind::NotRun,
        }
    }

    /// Whether the task ran on a worker (and so has a profile row).
    fn ran(&self) -> bool {
        match self {
            End::Made(_) | End::Unkept(_) | End::Stopped => true,
            End::Failed(cause) => !matches!(cause, Cause::NoWorker),
            End::Waited { .. } | End::NotRun => false,
        }
    }
}

/// What came back from a task's worker.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Done<O, F> {
    /// [`crate::proto::Stage::run`] returned.
    Returned(Result<Outcome<O, F>, Stopped>),
    /// It panicked; the thin driver caught the unwind on the worker.
    Panicked,
}

/// A store's refusal of a commit, by its code.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct CommitError {
    pub code: Code,
}

/// What happened.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Event<O, F> {
    /// `task` returned on the worker of `ranges.lane`.
    TaskDone { task: TaskRef, ranges: TaskRanges, done: Done<O, F> },
    /// The watchdog saw this worker's task stall.
    WorkerStalled(WorkerRef),
    /// The command was asked to stop.
    Stop,
    /// The commit [`Effect::Commit`] asked for finished.
    CommitDone(Result<(), CommitError>),
    /// [`Phase::next_deadline`] passed.
    Deadline,
}

/// An [`Event`]'s kind, for errors.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EventKind {
    Start,
    TaskDone,
    WorkerStalled,
    Stop,
    CommitDone,
    Deadline,
}

/// What the thin driver does next.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Effect {
    /// Run `task` on `worker`, with a context on that worker's lane and frame.
    Start { task: TaskRef, worker: WorkerRef },
    /// Reset this worker's frame: nothing its last task made in it is reachable after.
    ResetFrame(WorkerRef),
    /// Write [`Phase::commits`]: `kept` rows to the store, the others to the command's unstored
    /// overlay; then feed [`Event::CommitDone`].
    Commit,
    /// Merge [`Phase::merge_order`] into the engine.
    Merge,
    /// The phase's profile: [`Phase::rows`], [`Phase::waits`] and this status.
    Report(Status),
    /// Drop the phase's arena: nothing of the phase is live after (law g).
    DropArena,
    /// A worker is lost to a stall and may still write in its lane: the arena is leaked to the
    /// process's end instead of dropped (`Root::leak`), and the command ends with the stall.
    LeakArena { lost: u16 },
}

/// Where the phase is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum State {
    Planned,
    Running,
    Committing,
    Merged,
    Stopped,
    Refused(Refusal),
}

/// Why a phase was refused after its tasks ran.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Refusal {
    Commit(CommitError),
    CommitLate { limit: u64 },
}

/// The thin driver broke the protocol: a typed refusal, never a guess (rule 3).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MachineError {
    /// A phase needs at least one worker.
    NoWorkers,
    /// A plan's dependency is not an earlier task.
    Deps { task: TaskRef },
    /// More tasks than a `u32` counts.
    Tasks { len: usize },
    OutOfState { state: State, event: EventKind },
    UnknownTask { task: TaskRef },
    UnknownWorker { worker: WorkerRef },
    /// `task` came back but is not running on that worker.
    NotRunning { task: TaskRef, worker: WorkerRef },
    /// The watchdog named a worker with no task.
    NotBusy { worker: WorkerRef },
}

/// What the driver holds of a finished task for its commit and merge: its lane and ranges.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Held {
    pub lane: WorkerRef,
    pub reads: Span,
    pub puts: Span,
    pub says: Span,
}

impl Held {
    const NONE: Held = Held { lane: WorkerRef(0), reads: Span::EMPTY, puts: Span::EMPTY, says: Span::EMPTY };
}

/// One profile row: a task that ran, in the order tasks finished.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Row {
    pub task: TaskRef,
    pub worker: WorkerRef,
    pub start: u64,
    pub end: u64,
    pub kind: EndKind,
    pub reads: u32,
    pub puts: u32,
    pub says: u32,
    pub spans: Span,
}

/// One cause and how many tasks wait on it: printed once, "N tasks wait on <root>: <why>".
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct WaitGroup {
    pub root: TaskRef,
    pub tasks: u32,
}

/// The phase's counts and its memory law's figures.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Counts {
    pub tasks: u32,
    pub made: u32,
    pub unkept: u32,
    pub failed: u32,
    pub waited: u32,
    pub stopped: u32,
    pub not_run: u32,
    pub stalled: u32,
    pub late: u32,
}

impl Counts {
    pub fn ended(&self) -> u32 {
        self.made + self.unkept + self.failed + self.waited + self.stopped + self.not_run
    }
}

/// The status a report carries.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Status {
    pub state: State,
    pub counts: Counts,
    pub running: u16,
    pub lost: u16,
    /// THE MEMORY LAW (lib.rs): bytes the driver holds per task to the merge, measured from the
    /// slot's type, against the floor `size_of::<Out>() + size_of::<Held>()`.
    pub held_per_task: u32,
    pub floor_per_task: u32,
    /// Bytes of one profile row, held per task that ran.
    pub row_bytes: u32,
}

/// One task as the driver holds it: what scheduling needs while it waits, and only its end and
/// ranges once it has ended (the memory law's slot: its size is the measured figure).
#[derive(Clone, Copy, PartialEq, Debug)]
enum Slot<O, F> {
    Waiting {
        /// Inputs not yet ended.
        left: u32,
        /// The lowest failed (or waiting) input and its root.
        failed_on: Option<(TaskRef, TaskRef)>,
        /// An input ended `Unkept`.
        unkept_in: bool,
    },
    Ready { unkept_in: bool },
    Running { worker: WorkerRef, unkept_in: bool },
    Ended(End<O, F>, Held),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Worker {
    Idle,
    Busy { task: TaskRef, since: u64 },
    Lost(TaskRef),
}

/// One phase's driver.
pub struct Phase<'p, O, F> {
    slots: AVec<'p, Slot<O, F>>,
    /// Each task's dependents: `after[after_at[t]..after_at[t + 1]]`.
    after_at: AVec<'p, u32>,
    after: AVec<'p, TaskRef>,
    ready: AVec<'p, TaskRef>,
    workers: AVec<'p, Worker>,
    rows: AVec<'p, Row>,
    effects: AVec<'p, Effect>,
    effects_read: usize,
    work: AVec<'p, TaskRef>,
    state: State,
    stopping: bool,
    merged_early: bool,
    commit_at: u64,
    /// Since when every worker is lost with tasks still to start.
    starved_since: Option<u64>,
    limits: Limits,
    counts: Counts,
}

impl<'p, O: TaskOut, F: TaskFail> Phase<'p, O, F> {
    /// The driver of a phase planned as `deps`, on `workers` workers, everything in the phase's
    /// arena `alloc`.
    pub fn new(deps: Deps<'_>, workers: u16, limits: Limits, alloc: Alloc<'p>) -> Result<Phase<'p, O, F>, MachineError> {
        const {
            assert!(size_of::<O>() <= OUT_MOST, "a stage's `Out` is over OUT_MOST bytes: hold ranges, not values");
            assert!(size_of::<F>() <= FAIL_MOST, "a stage's `Fail` is over FAIL_MOST bytes: hold a code and fields");
        }
        if workers == 0 {
            return Err(MachineError::NoWorkers);
        }
        let n = deps.len();
        let tasks = len32(n).map_err(|_| MachineError::Tasks { len: n })?;
        let mut slots = Vec::with_capacity_in(n, alloc);
        let mut after_at = Vec::from_elem_in(0u32, n + 1, alloc);
        for t in 0..tasks {
            let task = TaskRef(t);
            let of = deps.of(task);
            for on in of {
                if *on >= task {
                    return Err(MachineError::Deps { task });
                }
                after_at[on.index() + 1] += 1;
            }
            let left = len32(of.len()).map_err(|_| MachineError::Deps { task })?;
            slots.push(if left == 0 { Slot::Ready { unkept_in: false } } else { Slot::Waiting { left, failed_on: None, unkept_in: false } });
        }
        for t in 0..n {
            after_at[t + 1] += after_at[t];
        }
        let mut fill = Vec::from_slice_in(&after_at, alloc);
        let mut after = Vec::from_elem_in(TaskRef(0), idx(after_at[n]), alloc);
        for t in 0..tasks {
            for on in deps.of(TaskRef(t)) {
                let at = &mut fill[on.index()];
                after[idx(*at)] = TaskRef(t);
                *at += 1;
            }
        }
        drop(fill);
        // Lowest first off the end: the start order is a function of the plan.
        let mut ready = Vec::new_in(alloc);
        for t in (0..tasks).rev() {
            if matches!(slots[idx(t)], Slot::Ready { .. }) {
                ready.push(TaskRef(t));
            }
        }
        Ok(Phase {
            slots,
            after_at,
            after,
            ready,
            workers: Vec::from_elem_in(Worker::Idle, usize::from(workers), alloc),
            rows: Vec::new_in(alloc),
            effects: Vec::new_in(alloc),
            effects_read: 0,
            work: Vec::new_in(alloc),
            state: State::Planned,
            stopping: false,
            merged_early: false,
            commit_at: 0,
            starved_since: None,
            limits,
            counts: Counts { tasks, ..Counts::default() },
        })
    }

    /// Begins running: the first tasks start.
    pub fn start(&mut self, now: u64) -> Result<(), MachineError> {
        if self.state != State::Planned {
            return Err(MachineError::OutOfState { state: self.state, event: EventKind::Start });
        }
        self.state = State::Running;
        self.fill(now);
        self.finish_if_done(now);
        Ok(())
    }

    /// Takes one event.
    pub fn step(&mut self, now: u64, event: Event<O, F>) -> Result<(), MachineError> {
        match event {
            Event::TaskDone { task, ranges, done } => self.done(now, task, ranges, done)?,
            Event::WorkerStalled(worker) => {
                let w = self.worker(worker)?;
                let current = self.workers[w];
                match current {
                    Worker::Busy { task, since } => self.stall(now, task, worker, since),
                    Worker::Idle | Worker::Lost(_) => return Err(MachineError::NotBusy { worker }),
                }
            }
            Event::Stop => self.begin_stop(),
            Event::CommitDone(result) => {
                if self.state != State::Committing {
                    return Err(MachineError::OutOfState { state: self.state, event: EventKind::CommitDone });
                }
                match result {
                    Ok(()) => self.committed(),
                    Err(why) => self.refuse(Refusal::Commit(why)),
                }
            }
            Event::Deadline => self.deadline(now),
        }
        self.fill(now);
        self.finish_if_done(now);
        Ok(())
    }

    /// The next effect to carry out, in the order they were made.
    pub fn next_effect(&mut self) -> Option<Effect> {
        if let Some(effect) = self.effects.get(self.effects_read).copied() {
            self.effects_read += 1;
            return Some(effect);
        }
        self.effects.clear();
        self.effects_read = 0;
        None
    }

    /// When the next deadline falls: a running task's stall, or the commit's.
    pub fn next_deadline(&self) -> Option<u64> {
        match self.state {
            State::Running => self
                .workers
                .iter()
                .filter_map(|worker| match *worker {
                    Worker::Busy { since, .. } => Some(since),
                    Worker::Idle | Worker::Lost(_) => None,
                })
                .chain(self.starved_since)
                .min()
                .map(|since| since.saturating_add(self.limits.stall_after)),
            State::Committing => Some(self.commit_at.saturating_add(self.limits.commit_within)),
            State::Planned | State::Merged | State::Stopped | State::Refused(_) => None,
        }
    }

    pub fn state(&self) -> State {
        self.state
    }

    pub fn len(&self) -> usize {
        self.slots.len()
    }

    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    /// `task`'s end, once it has one.
    pub fn end_of(&self, task: TaskRef) -> Option<End<O, F>> {
        match *self.slots.get(task.index())? {
            Slot::Ended(end, _) => Some(end),
            Slot::Waiting { .. } | Slot::Ready { .. } | Slot::Running { .. } => None,
        }
    }

    /// `task`'s ranges, once it has ended (empty for a task that did not run).
    fn held(&self, task: TaskRef) -> Held {
        match self.slots.get(task.index()) {
            Some(Slot::Ended(_, held)) => *held,
            Some(Slot::Waiting { .. } | Slot::Ready { .. } | Slot::Running { .. }) | None => Held::NONE,
        }
    }

    /// The profile's rows: every task that ran, in the order they finished.
    pub fn rows(&self) -> &[Row] {
        &self.rows
    }

    pub fn status(&self) -> Status {
        let busy = self.workers.iter().filter(|w| matches!(w, Worker::Busy { .. })).count();
        let lost = self.workers.iter().filter(|w| matches!(w, Worker::Lost(_))).count();
        Status {
            state: self.state,
            counts: self.counts,
            running: u16::try_from(busy).unwrap_or(u16::MAX),
            lost: u16::try_from(lost).unwrap_or(u16::MAX),
            held_per_task: len32(size_of::<Slot<O, F>>()).unwrap_or(u32::MAX),
            floor_per_task: len32(size_of::<O>() + size_of::<Held>()).unwrap_or(u32::MAX),
            row_bytes: len32(size_of::<Row>()).unwrap_or(u32::MAX),
        }
    }

    /// Every task with its end and ranges, in task order (rule 25): what the merge takes.
    pub fn merge_order(&self) -> impl Iterator<Item = (TaskRef, End<O, F>, Held)> + '_ {
        let by_rows = plant::on(Plant::CompletionOrder);
        let ran = self.rows.iter().filter(move |_| by_rows).map(|row| row.task);
        let rest = (0..self.counts.tasks).map(TaskRef).filter(move |task| !by_rows || !self.ended(*task).is_some_and(|end| end.ran()));
        ran.chain(rest).filter_map(move |task| self.ended(task).map(|end| (task, end, self.held(task))))
    }

    /// The puts to commit, in task order: each made or unkept task's ranges, and whether it is
    /// kept (stored) or only this command's.
    pub fn commits(&self) -> impl Iterator<Item = (TaskRef, Held, bool)> + '_ {
        self.merge_order().filter_map(|(task, end, held)| match end {
            End::Made(_) => Some((task, held, true)),
            End::Unkept(_) => Some((task, held, false)),
            End::Failed(_) | End::Waited { .. } | End::Stopped | End::NotRun => None,
        })
    }

    /// Each root failure and how many tasks wait on it, by root, in `alloc` (the printer's).
    pub fn waits<'g>(&self, alloc: Alloc<'g>) -> AVec<'g, WaitGroup> {
        let mut roots: AVec<'g, TaskRef> = Vec::new_in(alloc);
        for slot in self.slots.iter() {
            if let Slot::Ended(End::Waited { root, .. }, _) = *slot {
                roots.push(root);
            }
        }
        roots.sort_unstable();
        let mut groups: AVec<'g, WaitGroup> = Vec::new_in(alloc);
        for root in roots.iter().copied() {
            if let Some(group) = groups.last_mut()
                && group.root == root
            {
                group.tasks += 1;
                continue;
            }
            groups.push(WaitGroup { root, tasks: 1 });
        }
        groups
    }

    fn ended(&self, task: TaskRef) -> Option<End<O, F>> {
        self.end_of(task)
    }

    fn worker(&self, worker: WorkerRef) -> Result<usize, MachineError> {
        let w = worker.index();
        if w < self.workers.len() { Ok(w) } else { Err(MachineError::UnknownWorker { worker }) }
    }

    fn done(&mut self, now: u64, task: TaskRef, ranges: TaskRanges, done: Done<O, F>) -> Result<(), MachineError> {
        let worker = ranges.lane;
        let w = self.worker(worker)?;
        let Some(slot) = self.slots.get(task.index()).copied() else {
            return Err(MachineError::UnknownTask { task });
        };
        let current = self.workers[w];
        let since = match current {
            Worker::Busy { task: running, since } if running == task => since,
            Worker::Lost(lost) if lost == task => {
                // Late: its end stands (stalled); the worker is back once its frame is reset,
                // unless the phase has ended and its arena gone with it.
                self.counts.late += 1;
                if matches!(self.state, State::Running | State::Committing) {
                    self.workers[w] = Worker::Idle;
                    self.effects.push(Effect::ResetFrame(worker));
                }
                return Ok(());
            }
            Worker::Busy { .. } | Worker::Lost(_) | Worker::Idle => return Err(MachineError::NotRunning { task, worker }),
        };
        let Slot::Running { unkept_in, .. } = slot else {
            return Err(MachineError::NotRunning { task, worker });
        };
        self.workers[w] = Worker::Idle;
        if !plant::on(Plant::FrameNotReset) {
            self.effects.push(Effect::ResetFrame(worker));
        }
        let end = match done {
            Done::Panicked => End::Failed(Cause::Panicked),
            Done::Returned(Err(stopped)) => halted(stopped.why()),
            Done::Returned(Ok(outcome)) => match ranges.halted {
                Some(why) => halted(why),
                None => match outcome {
                    Outcome::Made(out) if ranges.uncovered > 0 || unkept_in => End::Unkept(out),
                    Outcome::Made(out) => End::Made(out),
                    Outcome::Unkept(out) => End::Unkept(out),
                    Outcome::Failed(why) => End::Failed(Cause::Stage(why)),
                },
            },
        };
        let held = Held { lane: worker, reads: ranges.reads, puts: ranges.puts, says: ranges.says };
        self.rows.push(Row {
            task,
            worker,
            start: since,
            end: now,
            kind: end.kind(),
            reads: ranges.reads.len,
            puts: ranges.puts.len,
            says: ranges.says.len,
            spans: ranges.spans,
        });
        if end == End::Stopped {
            self.begin_stop();
        }
        self.settle(task, end, held);
        Ok(())
    }

    fn stall(&mut self, now: u64, task: TaskRef, worker: WorkerRef, since: u64) {
        self.workers[worker.index()] = Worker::Lost(task);
        self.counts.stalled += 1;
        self.rows.push(Row {
            task,
            worker,
            start: since,
            end: now,
            kind: EndKind::Failed,
            reads: 0,
            puts: 0,
            says: 0,
            spans: Span::EMPTY,
        });
        self.settle(task, End::Failed(Cause::Stalled), Held::NONE);
    }

    fn count(&mut self, end: &End<O, F>) {
        let counts = &mut self.counts;
        match end.kind() {
            EndKind::Made => counts.made += 1,
            EndKind::Unkept => counts.unkept += 1,
            EndKind::Failed => counts.failed += 1,
            EndKind::Waited => counts.waited += 1,
            EndKind::Stopped => counts.stopped += 1,
            EndKind::NotRun => counts.not_run += 1,
        }
    }

    /// Ends `task` and carries its end to the tasks that wait on it, by a worklist.
    fn settle(&mut self, task: TaskRef, end: End<O, F>, held: Held) {
        self.slots[task.index()] = Slot::Ended(end, held);
        self.count(&end);
        self.work.clear();
        self.work.push(task);
        while let Some(t) = self.work.pop() {
            let Slot::Ended(end_t, _) = self.slots[t.index()] else {
                continue;
            };
            let (from, to) = (idx(self.after_at[t.index()]), idx(self.after_at[t.index() + 1]));
            for at in from..to {
                let d = self.after[at];
                let Slot::Waiting { mut left, mut failed_on, mut unkept_in } = self.slots[d.index()] else {
                    continue;
                };
                left -= 1;
                let failed = match end_t {
                    End::Failed(_) => Some((t, t)),
                    End::Waited { root, .. } => Some((t, if plant::on(Plant::WaitedOnDirect) { t } else { root })),
                    End::Made(_) | End::Unkept(_) | End::Stopped | End::NotRun => None,
                };
                if let Some((on, root)) = failed
                    && failed_on.is_none_or(|(was, _)| on < was)
                {
                    failed_on = Some((on, root));
                }
                if matches!(end_t, End::Unkept(_)) {
                    unkept_in = true;
                }
                // A stopped or unrun input leaves this task unrun; its `left` keeps counting so a
                // failed input arriving later still names the root.
                let halted_in = matches!(end_t, End::Stopped | End::NotRun);
                if left > 0 {
                    self.slots[d.index()] = Slot::Waiting { left, failed_on, unkept_in };
                    continue;
                }
                let next = match failed_on {
                    Some((on, root)) => Some(End::Waited { on, root }),
                    None if halted_in || self.stopping => Some(End::NotRun),
                    None => None,
                };
                match next {
                    Some(end_d) => {
                        self.slots[d.index()] = Slot::Ended(end_d, Held::NONE);
                        self.count(&end_d);
                        self.work.push(d);
                    }
                    None => {
                        self.slots[d.index()] = Slot::Ready { unkept_in };
                        self.ready.push(d);
                    }
                }
            }
        }
    }

    fn begin_stop(&mut self) {
        if self.stopping {
            return;
        }
        self.stopping = true;
        if plant::on(Plant::StartAfterStop) {
            return;
        }
        self.ready.clear();
        for t in 0..self.slots.len() {
            if matches!(self.slots[t], Slot::Waiting { .. } | Slot::Ready { .. }) {
                self.slots[t] = Slot::Ended(End::NotRun, Held::NONE);
                self.counts.not_run += 1;
            }
        }
    }

    fn deadline(&mut self, now: u64) {
        match self.state {
            State::Running => {
                for w in 0..self.workers.len() {
                    let Worker::Busy { task, since } = self.workers[w] else {
                        continue;
                    };
                    if now >= since.saturating_add(self.limits.stall_after) {
                        self.stall(now, task, WorkerRef(u16::try_from(w).unwrap_or(u16::MAX)), since);
                    }
                }
                if let Some(since) = self.starved_since
                    && now >= since.saturating_add(self.limits.stall_after)
                {
                    self.starve();
                }
            }
            State::Committing => {
                if now >= self.commit_at.saturating_add(self.limits.commit_within) {
                    self.refuse(Refusal::CommitLate { limit: self.limits.commit_within });
                }
            }
            State::Planned | State::Merged | State::Stopped | State::Refused(_) => {}
        }
    }

    /// Starts ready tasks on idle workers; with every worker lost, fails what cannot start.
    fn fill(&mut self, now: u64) {
        if self.state != State::Running || (self.stopping && !plant::on(Plant::StartAfterStop)) {
            return;
        }
        for w in 0..self.workers.len() {
            if self.workers[w] != Worker::Idle {
                continue;
            }
            let Some(task) = self.ready.pop() else {
                break;
            };
            let worker = WorkerRef(u16::try_from(w).unwrap_or(u16::MAX));
            self.workers[w] = Worker::Busy { task, since: now };
            let unkept_in = match self.slots[task.index()] {
                Slot::Ready { unkept_in } => unkept_in,
                Slot::Waiting { unkept_in, .. } | Slot::Running { unkept_in, .. } => unkept_in,
                Slot::Ended(..) => false,
            };
            self.slots[task.index()] = Slot::Running { worker, unkept_in };
            self.effects.push(Effect::Start { task, worker });
        }
        // Every worker lost with tasks still to start: wait one stall's length for a lost worker
        // to come back (its late return frees it), then fail what cannot start (rule 18).
        let usable = self.workers.iter().any(|w| matches!(w, Worker::Idle | Worker::Busy { .. }));
        if usable || self.ready.is_empty() {
            self.starved_since = None;
        } else if self.starved_since.is_none() {
            self.starved_since = Some(now);
        }
    }

    fn starve(&mut self) {
        self.starved_since = None;
        while let Some(task) = self.ready.pop() {
            self.settle(task, End::Failed(Cause::NoWorker), Held::NONE);
        }
    }

    fn finish_if_done(&mut self, now: u64) {
        let busy = self.workers.iter().any(|w| matches!(w, Worker::Busy { .. }));
        if self.state != State::Running || busy || self.counts.ended() < self.counts.tasks {
            return;
        }
        self.state = State::Committing;
        self.commit_at = now;
        self.effects.push(Effect::Commit);
        if plant::on(Plant::VisibleBeforeCommit) && !self.stopping {
            self.effects.push(Effect::Merge);
            self.merged_early = true;
        }
    }

    fn committed(&mut self) {
        if self.stopping {
            self.state = State::Stopped;
        } else {
            if !self.merged_early {
                self.effects.push(Effect::Merge);
            }
            self.state = State::Merged;
        }
        self.end_arena();
    }

    fn refuse(&mut self, why: Refusal) {
        self.state = State::Refused(why);
        self.end_arena();
    }

    fn end_arena(&mut self) {
        self.effects.push(Effect::Report(self.status()));
        let lost = self.workers.iter().filter(|w| matches!(w, Worker::Lost(_))).count();
        self.effects.push(if lost == 0 { Effect::DropArena } else { Effect::LeakArena { lost: u16::try_from(lost).unwrap_or(u16::MAX) } });
    }
}

/// The end of a task its context halted.
fn halted<O, F>(why: Halt) -> End<O, F> {
    match why {
        Halt::Asked => End::Stopped,
        Halt::Full { column, cap } => End::Failed(Cause::Full { column, cap }),
    }
}
