//! What a stage says: how its tasks are planned ([`Stage::plan`] into a [`Plan`]), what one task
//! does ([`Stage::run`] through a [`TaskCx`]), and what a task hands back ([`Outcome`] of a small
//! [`TaskOut`] or a small [`TaskFail`]). Nothing else is the stage's: the driver
//! ([`crate::machine`]) owns the schedule, the frames, the arena, the commit, the merge, stop,
//! status, the profile and how a failure reaches the tasks that wait on it.

use core::fmt::Debug;
use core::ops::Range;

use blaze_int::{idx, len32};
use blaze_own::{AVec, Alloc, Vec};

use crate::cx::TaskCx;

/// A range of one of the phase's columns: `len` rows from `start`. What a task's result holds in
/// place of a collection (rule 51).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Span {
    pub start: u32,
    pub len: u32,
}

impl Span {
    pub const EMPTY: Span = Span { start: 0, len: 0 };

    /// The rows `from..to` of a column (`from <= to`, both within `u32`: the column's cap).
    pub(crate) fn between(from: u32, to: u32) -> Span {
        Span { start: from, len: to - from }
    }

    pub fn range(self) -> Range<usize> {
        let start = idx(self.start);
        start..start + idx(self.len)
    }
}

/// A task of one phase: its index in the plan's order. The merge is in this order (rule 25).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct TaskRef(pub u32);

impl TaskRef {
    pub fn index(self) -> usize {
        idx(self.0)
    }
}

/// A worker of the phase's pool; its lane of the phase's columns has the same index.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct WorkerRef(pub u16);

impl WorkerRef {
    pub fn index(self) -> usize {
        usize::from(self.0)
    }
}

/// A finding's, a refusal's or a failure's kind: a stage's own numbering, rendered to words only
/// when printed (rule 81).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Code(pub u16);

/// The most bytes a [`TaskOut`] may take: the driver holds one per task to the merge, so this is
/// the per-task floor's main term (the memory law, lib.rs). Checked at compile time where a
/// [`crate::machine::Phase`] is made.
pub const OUT_MOST: usize = 64;

/// The most bytes a [`TaskFail`] may take, held per failed task like an out.
pub const FAIL_MOST: usize = 48;

/// What one task made, as the driver holds it to the merge: ranges into the phase's columns and
/// fixed-size counts only. `Copy + 'static` shuts out a `Vec`, `String`, `Box` and any borrowed
/// payload; [`OUT_MOST`] bounds its size. The bytes a task made are in the records it put
/// ([`TaskCx::put`]); the driver holds their (key, digest) rows in the phase's arena, never the
/// bytes past the commit.
pub trait TaskOut: Copy + Send + Debug + PartialEq + 'static {}

/// Why one task failed, in a form the driver holds and the printer renders at print time: a code
/// and typed fields, no rendered text. `Copy + 'static`, at most [`FAIL_MOST`] bytes.
pub trait TaskFail: Copy + Send + Debug + PartialEq + 'static {
    /// The failure's kind, counted in the profile's row.
    fn code(&self) -> Code;
}

/// What a task says it did. The fourth state, a task whose input failed, only the driver makes
/// ([`crate::machine::End::Waited`]), naming the root cause's task.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Outcome<O, F> {
    /// Correct and kept: its records are committed and its row is stored.
    Made(O),
    /// Correct for THIS command and not to be stored: its inputs include a read no key covers.
    /// Dependents use it (and are unkept in turn); the task is owed again next command.
    Unkept(O),
    /// Failed, with why.
    Failed(F),
}

/// Why [`TaskCx`] halted a task: made only by the task's context, so a stage cannot claim one it
/// was not given, and a stop or a full column always reaches the driver.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Halt {
    /// The command was asked to stop.
    Asked,
    /// A column of the task's lane is at its cap (rule 18: every table has one).
    Full { column: Column, cap: u32 },
}

/// The columns of a lane.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Column {
    Reads,
    Puts,
    Bytes,
    Says,
    Args,
    Spans,
}

/// A task halted by its context: what [`TaskCx::stopped`] and every other context call return so
/// that `?` carries it out of [`Stage::run`]. Its field is private: only the context makes one.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Stopped {
    pub(crate) why: Halt,
}

impl Stopped {
    pub fn why(self) -> Halt {
        self.why
    }
}

/// One stage of the check engine in the one protocol.
pub trait Stage: Sync {
    /// One task's input, copied to the worker that runs it. It may borrow the phase's inputs.
    type Job: Copy + Send;
    /// What one task made: see [`TaskOut`].
    type Out: TaskOut;
    /// Why one task failed: see [`TaskFail`].
    type Fail: TaskFail;
    /// What one read names (the engine's cell and its digest): appended to the phase's reads
    /// column, sorted and deduplicated per task by the driver.
    type Read: Copy + Ord + Send;
    /// Which of the engine's stages this is.
    const KIND: blaze_engine::Stage;

    /// The phase's tasks, in the order the merge takes them: each job and the tasks it waits on
    /// (earlier ones only, so a plan has no cycle).
    fn plan(&self, plan: &mut Plan<'_, Self::Job>) -> Result<(), PlanError>;

    /// One task. Everything it keeps it puts through `task`; it returns what the driver holds.
    fn run(&self, job: Self::Job, task: &mut TaskCx<'_, '_, Self::Read>) -> Result<Outcome<Self::Out, Self::Fail>, Stopped>;
}

/// A plan being made: jobs in task order, each with the earlier tasks it waits on, in columns.
pub struct Plan<'p, J> {
    jobs: AVec<'p, J>,
    spans: AVec<'p, Span>,
    deps: AVec<'p, TaskRef>,
    caps: PlanCaps,
}

/// A plan's caps (rule 18).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PlanCaps {
    pub tasks: u32,
    pub deps: u32,
}

/// Why a plan was refused.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PlanError {
    /// More than `cap` tasks.
    Tasks { cap: u32 },
    /// More than `cap` dependency edges.
    Deps { cap: u32 },
    /// `task` named `on`, which is not an earlier task.
    Forward { task: TaskRef, on: TaskRef },
}

/// A plan's dependency columns, as the driver reads them.
#[derive(Clone, Copy)]
pub struct Deps<'a> {
    pub spans: &'a [Span],
    pub deps: &'a [TaskRef],
}

impl<'a> Deps<'a> {
    pub fn len(&self) -> usize {
        self.spans.len()
    }

    pub fn is_empty(&self) -> bool {
        self.spans.is_empty()
    }

    pub fn of(&self, task: TaskRef) -> &'a [TaskRef] {
        &self.deps[self.spans[task.index()].range()]
    }
}

impl<'p, J: Copy> Plan<'p, J> {
    pub fn new_in(caps: PlanCaps, alloc: Alloc<'p>) -> Plan<'p, J> {
        Plan { jobs: Vec::new_in(alloc), spans: Vec::new_in(alloc), deps: Vec::new_in(alloc), caps }
    }

    /// Adds `job` as the next task, waiting on `after` (each an earlier task).
    pub fn job(&mut self, job: J, after: &[TaskRef]) -> Result<TaskRef, PlanError> {
        let at = len32(self.jobs.len()).map_err(|_| PlanError::Tasks { cap: self.caps.tasks })?;
        if at >= self.caps.tasks {
            return Err(PlanError::Tasks { cap: self.caps.tasks });
        }
        let task = TaskRef(at);
        if let Some(on) = after.iter().copied().find(|on| *on >= task) {
            return Err(PlanError::Forward { task, on });
        }
        let start = len32(self.deps.len()).map_err(|_| PlanError::Deps { cap: self.caps.deps })?;
        let len = len32(after.len()).map_err(|_| PlanError::Deps { cap: self.caps.deps })?;
        if start.checked_add(len).is_none_or(|end| end > self.caps.deps) {
            return Err(PlanError::Deps { cap: self.caps.deps });
        }
        self.deps.extend_from_slice(after);
        self.spans.push(Span { start, len });
        self.jobs.push(job);
        Ok(task)
    }

    pub fn len(&self) -> usize {
        self.jobs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.jobs.is_empty()
    }

    pub fn job_of(&self, task: TaskRef) -> J {
        self.jobs[task.index()]
    }

    pub fn deps(&self) -> Deps<'_> {
        Deps { spans: &self.spans, deps: &self.deps }
    }
}
