//! Structured tasks (docs/v2/plan/design/std.md, "Tasks and waiting"): a scope is a value.
//!
//! ```text
//! let scope = cx.scope();
//! let a = scope.spawn("a", TASK_FRAME, |cx| read(cx, &dir));
//! let b = scope.spawn("b", TASK_FRAME, |cx| read(cx, &dir));
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
//! its `Drop`: it cancels the scope, then waits, bounded by [`ABANDON_WAIT`], for its tasks to
//! end, and records how ([`Settled`]) in the trace. The scope holds its tasks' state itself, so
//! a `Task` is not put in a `Vec` or an `Option` (the checker refuses a linear value in a type
//! argument of a type that is not marked for it): each is joined where it was spawned.
//!
//! **Layout.** S1 runs the `Inline` layout ([`LAYOUT`]): [`Scope::spawn`] runs its task to
//! completion before it returns, so spawn is eager and nothing else runs while a task waits.
//! [`Scope::spawn_concurrent`], which must overlap its task with what follows, cannot under
//! `Inline` and is refused at run time with [`TaskWhy::CannotOverlap`] (the spec makes it a
//! compile error; that check does not exist yet).

use alloc::{borrow::ToOwned, string::{String, ToString}};
use core::{
    any::Any,
    cell::{Cell, RefCell},
    fmt,
    marker::PhantomData,
    panic::AssertUnwindSafe,
};

use blaze_core::wait::Deadline;
use blaze_mem::Frame;

use crate::{
    Plant, Stopped,
    rt::{catch_unwind, resume_unwind},
    cx::Cx,
    host::Exit,
    io::ABANDON_WAIT,
};

/// How a waiting task's frames are stored (std.md, "Layouts").
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Layout {
    /// A task runs to completion at its spawn; nothing else runs while it waits.
    Inline,
}

/// This build's layout.
pub const LAYOUT: Layout = Layout::Inline;

/// How a task failed.
#[derive(Clone, PartialEq, Eq, Debug)] // clone: an error
pub enum TaskWhy {
    /// It returned this error.
    Failed(String),
    /// It panicked, saying this.
    Panicked(String),
    /// Its frame could not be made.
    OutOfMemory(Stopped),
    /// It was spawned to overlap what follows, and the layout cannot overlap: it never ran.
    CannotOverlap { layout: Layout },
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
            TaskWhy::CannotOverlap { layout } => write!(
                f,
                "bstd::task {task} of a scope in {scope}: spawn_concurrent must overlap its task, and the \
                 {layout:?} layout runs each task to completion at its spawn"
            ),
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
    failed: RefCell<Option<TaskError>>,
    /// Tasks spawned and not yet ended.
    running: Cell<usize>,
    /// The task spawned last: who an [`TaskWhy::Unended`] names.
    last: Cell<&'static str>,
    /// Whether the scope is still this value's to end: cleared by [`Scope::join`].
    armed: bool,
}

impl Live {
    /// Waits until `until` for the scope's tasks to end. Under `Inline` each task ended at its
    /// spawn, so none is ever left; a layout that overlaps tasks parks here on its scheduler.
    fn settle(&self, until: Deadline) -> Settled {
        let running = self.running.get();
        match running {
            0 => Settled::All,
            running => {
                let stopped = Stopped::Deadline { span: self.cx.name(), at: until.get() };
                Settled::Left { running, stopped }
            }
        }
    }

    /// `task` failed so: the scope is cancelled, and the first failure kept.
    fn fail(&self, task: &'static str, why: TaskWhy) -> TaskError {
        self.cx.cancel();
        let error = TaskError { scope: self.cx.name(), task, why };
        let mut failed = self.failed.borrow_mut();
        if failed.is_none() {
            // clone: an error, kept for the scope and returned to its task.
            *failed = Some(error.clone());
        }
        error
    }
}

impl Drop for Live {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        self.cx.cancel();
        let until = Deadline::after(self.cx.io().now(), ABANDON_WAIT);
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
}

/// One task of a scope, from [`Scope::spawn`]: finished by [`Task::join`], before its scope
/// is. Linear for the scope's reason; dropping one in a panic drops its outcome, which its
/// scope has seen already.
#[blaze::linear]
#[must_use = "a task is finished with join(cx)"]
pub struct Task<'s, T> {
    name: &'static str,
    outcome: Result<T, TaskError>,
    scope: PhantomData<&'s ()>,
}

impl<'p> Scope<'p> {
    /// The scope whose context is `cx`, from [`Cx::scope`].
    pub(crate) fn new(cx: Cx) -> Scope<'p> {
        let live = Live {
            cx,
            failed: RefCell::new(None),
            running: Cell::new(0),
            last: Cell::new(""),
            armed: true,
        };
        Scope { live, parent: PhantomData }
    }

    /// Runs `task` as the task `name` of this scope, in any order with its siblings (under
    /// `Inline`, now, to completion). An error it returns, or a panic, cancels the scope: the
    /// next wait of each task after it ends with [`Stopped::Cancelled`], and the scope fails
    /// with the first. A `Task` value of a process's exit is never made: the exit unwinds on.
    ///
    /// `scratch` is the most bytes the task's frame (`cx.frame()`) hands out: an allocation
    /// past it is the task's [`Stopped::OutOfMemory`], naming the task. It is reserved as
    /// address space here and committed as it is used.
    pub fn spawn<T, E: fmt::Display>(
        &self,
        name: &'static str,
        scratch: usize,
        task: impl FnOnce(&Cx) -> Result<T, E>,
    ) -> Task<'_, T> {
        let live = &self.live;
        live.last.set(name);
        let frame = match Frame::with_reserve(scratch, blaze_mem::Tag::Frames) {
            Ok(frame) => frame,
            Err(_) => {
                let stopped = Stopped::OutOfMemory { arena: name, asked: scratch };
                let error = live.fail(name, TaskWhy::OutOfMemory(stopped));
                return Task { name, outcome: Err(error), scope: PhantomData };
            }
        };
        let cx = live.cx.task(name, frame);
        live.running.set(live.running.get() + 1);
        let ran = catch_unwind(AssertUnwindSafe(|| task(&cx)));
        live.running.set(live.running.get() - 1);
        drop(cx);
        let outcome = match ran {
            Ok(Ok(made)) => Ok(made),
            // alloc: an error's message.
            Ok(Err(error)) => Err(live.fail(name, TaskWhy::Failed(error.to_string()))),
            Err(payload) => match payload.downcast::<Exit>() {
                // The process exits: not this task's failure, so it is passed on.
                Ok(exit) => resume_unwind(exit),
                Err(payload) => Err(live.fail(name, TaskWhy::Panicked(said(&*payload)))),
            },
        };
        Task { name, outcome, scope: PhantomData }
    }

    /// Runs `task` overlapping what follows it in the scope. The `Inline` layout cannot: the
    /// task does not run, and it and the scope fail with [`TaskWhy::CannotOverlap`]. `scratch`
    /// is the task's frame, as [`Scope::spawn`]'s; no frame is made for a task that does not
    /// run.
    pub fn spawn_concurrent<T, E: fmt::Display>(
        &self,
        name: &'static str,
        scratch: usize,
        task: impl FnOnce(&Cx) -> Result<T, E>,
    ) -> Task<'_, T> {
        match LAYOUT {
            Layout::Inline => {
                let _: usize = scratch;
                drop(task);
                self.live.last.set(name);
                let error = self.live.fail(name, TaskWhy::CannotOverlap { layout: LAYOUT });
                Task { name, outcome: Err(error), scope: PhantomData }
            }
        }
    }

    /// Cancels the scope: the next wait of each task under it ends with
    /// [`Stopped::Cancelled`].
    pub fn cancel(&self) {
        self.live.cx.cancel();
    }

    /// The scope's own context.
    pub fn cx(&self) -> &Cx {
        &self.live.cx
    }

    /// How many of its tasks are running now: none, under `Inline`, outside a task.
    pub fn running(&self) -> usize {
        self.live.running.get()
    }

    /// The scope finished: once each of its tasks has ended (waited for until `cx`'s
    /// deadline), its first failure, or [`TaskWhy::Unended`] if tasks were still running when
    /// that wait stopped.
    #[blaze::consumes]
    pub fn join(mut self, cx: &Cx) -> Result<(), TaskError> {
        self.live.armed = false;
        let failed = self.live.failed.borrow_mut().take();
        match self.live.settle(cx.deadline()) {
            Settled::Left { running, stopped } => Err(TaskError {
                scope: self.live.cx.name(),
                task: self.live.last.get(),
                why: TaskWhy::Unended { running, stopped },
            }),
            Settled::All => match (failed, cx.plant()) {
                (Some(_), Plant::JoinDropsFailure) | (None, _) => Ok(()),
                (Some(failed), _) => Err(failed),
            },
        }
    }
}

impl<T> Task<'_, T> {
    /// Its name in its scope.
    pub fn name(&self) -> &'static str {
        self.name
    }

    /// The task finished: what it returned, or how it failed (named by its scope and its
    /// name). Under `Inline` it ended at its spawn, so this does not wait.
    #[blaze::consumes]
    pub fn join(self, _cx: &Cx) -> Result<T, TaskError> {
        self.outcome
    }
}

/// What a panic said.
fn said(payload: &(dyn Any + Send)) -> String {
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
        let outcome = match &self.outcome {
            Ok(_) => "ended",
            Err(_) => "failed",
        };
        write!(f, "Task({}, {outcome})", self.name)
    }
}
