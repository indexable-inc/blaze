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

use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, Ordering};

use blaze_core::{Done, Tag, wait::Deadline};
use blaze_mem::Frame;

use crate::{
    Plant, Stopped,
    io::{Ends, Io},
    mem::Bytes,
    task::{Scope, TaskError},
    trace::RecordKind,
};

/// The task's context. Not `Send`: it shares its task's frame, which is not `Sync`; a fiber
/// that moves threads takes its whole task with it.
pub struct Cx {
    deadline: Deadline,
    cancel: Arc<Cancel>,
    span: &'static str,
    frame: Arc<Frame>,
    /// The world the root context came from: where its spans and events are recorded.
    world: Io,
    plant: Plant,
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
        Cx { deadline, cancel: Cancel::new(None), span: "main", frame: shared(frame), world, plant }
    }

    /// A child whose deadline is the earlier of this one's and `deadline`: never later.
    pub fn until(&self, deadline: Deadline) -> Cx {
        let deadline = match self.plant {
            Plant::UntilWidens => deadline,
            _ => self.deadline.min(deadline),
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
    pub fn frame(&self) -> &Frame {
        &self.frame
    }

    /// Empty bytes in the task's frame, named by the span when they run out of room.
    pub fn bytes(&self) -> Bytes<'_> {
        Bytes::new_in(&self.frame, self.span)
    }

    pub(crate) fn plant(&self) -> Plant {
        self.plant
    }

    /// The world it came from.
    pub(crate) fn io(&self) -> &Io {
        &self.world
    }

    /// A scope of tasks inside this one: spawn its tasks with [`Scope::spawn`], then
    /// [`Scope::join`] it, which returns once each has ended, with the first that failed. A
    /// task may borrow from the frame that holds the scope. The scope is linear: leaving it
    /// unjoined is a compile error (docs/v2/plan/design/std.md, "Tasks and waiting").
    pub fn scope(&self) -> Scope<'_> {
        Scope::new(self.scoped())
    }

    /// `body` run with a scope of tasks inside this one, which is then joined: its result,
    /// or the first of its tasks that failed. The convenience over [`Cx::scope`].
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
            _ => self.deadline,
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
            _ => self.deadline,
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
        }
    }

    /// Whether this context stopped already at `now`: then a call asks nothing of the driver.
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
    pub(crate) fn task(&self, name: &'static str, frame: Frame) -> Cx {
        self.world.record_trace(name, RecordKind::Opened { parent: self.span });
        Cx { span: name, frame: shared(frame), ..self.child() }
    }

    /// Cancels the scope this context's cancellation is: every context sharing it, and every
    /// scope inside it.
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
