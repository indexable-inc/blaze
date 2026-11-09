//! Structured tasks (docs/v2/plan/design/std.md, "Tasks and waiting"):
//! `cx.scope(|s| s.spawn("name", |cx| ..))`. There is no `Tasks` capability: starting work is
//! something a [`Cx`] does. Each task gets its own `Cx`, borrowed for the task's run so it
//! cannot outlive the scope: its name, its own frame, the scope's deadline and cancellation.
//! Children may borrow from the parent. A task's error or panic cancels the scope; a task
//! learns it only as [`Stopped::Cancelled`] from its next wait, never by being dropped. The
//! scope returns its first failure, typed.
//!
//! **Layout.** S1 runs the `Inline` layout ([`LAYOUT`]): [`Scope::spawn`] runs its task to
//! completion before it returns, so spawn is eager and nothing else runs while a task waits.
//! [`Scope::spawn_concurrent`], which must overlap its task with what follows, cannot under
//! `Inline` and is refused at run time with [`TaskWhy::CannotOverlap`] (the spec makes it a
//! compile error; that check does not exist yet).

use std::{
    any::Any,
    fmt,
    marker::PhantomData,
    panic::{AssertUnwindSafe, catch_unwind, resume_unwind},
};

use blaze_mem::Frame;

use crate::{Stopped, cx::Cx, host::Exit, mem::TASK_FRAME};

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
        }
    }
}

impl std::error::Error for TaskError {}

/// A scope being run: where its tasks are spawned. It borrows the context it runs in, so
/// nothing of it outlives that.
pub struct Scope<'p> {
    cx: Cx,
    failed: Option<TaskError>,
    parent: PhantomData<&'p Cx>,
}

/// `body` run as the scope `cx` is the context of, from [`Cx::scope`].
pub(crate) fn run<'p, R>(cx: Cx, body: impl FnOnce(&mut Scope<'p>) -> R) -> Result<R, TaskError> {
    let mut scope = Scope { cx, failed: None, parent: PhantomData };
    let made = body(&mut scope);
    match scope.failed {
        Some(failed) => Err(failed),
        None => Ok(made),
    }
}

impl Scope<'_> {
    /// Runs `task` as the task `name` of this scope, in any order with its siblings (under
    /// `Inline`, now, to completion). An error it returns, or a panic, cancels the scope: the
    /// next wait of each task after it ends with [`Stopped::Cancelled`], and the scope fails
    /// with the first.
    pub fn spawn<E: fmt::Display>(&mut self, name: &'static str, task: impl FnOnce(&Cx) -> Result<(), E>) {
        let frame = match Frame::with_reserve(TASK_FRAME, blaze_mem::Tag::Frames) {
            Ok(frame) => frame,
            Err(_) => {
                let stopped = Stopped::OutOfMemory { arena: name, asked: TASK_FRAME };
                self.fail(name, TaskWhy::OutOfMemory(stopped));
                return;
            }
        };
        let cx = self.cx.task(name, frame);
        let ran = catch_unwind(AssertUnwindSafe(|| task(&cx)));
        drop(cx);
        match ran {
            Ok(Ok(())) => {}
            // alloc: an error's message.
            Ok(Err(error)) => self.fail(name, TaskWhy::Failed(error.to_string())),
            Err(payload) => match payload.downcast::<Exit>() {
                // The process exits: not this task's failure, so it is passed on.
                Ok(exit) => resume_unwind(exit),
                Err(payload) => self.fail(name, TaskWhy::Panicked(said(&*payload))),
            },
        }
    }

    /// Runs `task` overlapping what follows it in the scope. The `Inline` layout cannot: the
    /// task does not run, the scope fails with [`TaskWhy::CannotOverlap`], and so does this.
    pub fn spawn_concurrent<E: fmt::Display>(
        &mut self,
        name: &'static str,
        task: impl FnOnce(&Cx) -> Result<(), E>,
    ) -> Result<(), TaskError> {
        match LAYOUT {
            Layout::Inline => {
                drop(task);
                let why = TaskWhy::CannotOverlap { layout: LAYOUT };
                self.fail(name, why.clone());
                Err(TaskError { scope: self.cx.name(), task: name, why })
            }
        }
    }

    /// Cancels the scope: the next wait of each task under it ends with
    /// [`Stopped::Cancelled`].
    pub fn cancel(&self) {
        self.cx.cancel();
    }

    /// The scope's own context.
    pub fn cx(&self) -> &Cx {
        &self.cx
    }

    fn fail(&mut self, task: &'static str, why: TaskWhy) {
        self.cx.cancel();
        if self.failed.is_none() {
            self.failed = Some(TaskError { scope: self.cx.name(), task, why });
        }
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
        write!(f, "Scope(in {}, failed {:?})", self.cx.name(), self.failed)
    }
}
