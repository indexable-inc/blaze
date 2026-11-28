//! Blaze's sans-IO machine and one-shot lexical algebraic effect ABI
//! (`docs/v2/plan/design/sans-io-machines.md`).
//!
//! Both **explicit state-graph machines** (protocols, durability journals, transport lifecycles)
//! and **compiled one-shot `suspend` coroutines** lower to this single `#![no_std]`, zero-global-heap,
//! zero-`Pin` representation:
//!
//! - [`Request`]: one linear type per operation, fixing its typed [`Request::Answer`] so no
//!   "out of step" completion arm can be written.
//! - [`Ask`]: a linear request paired with a disjoint [`Waiting`] continuation state. Because each
//!   transition consumes resources (e.g. moving a linear `File` into `Close`, or moving an owned
//!   DMA buffer into `ReadInto`), there are no self-borrows and no `Pin`.
//! - [`Race`] and [`Batch`]: first-class structured concurrency combinators (`select` and
//!   bounded-N `io_uring` fan-out) directly as requests.
//! - [`Call`]: structured sub-machine delegation without flattening the child's states or requests.
//! - [`TaskFrame`]: pairs an owned chunked bump arena with a paused machine state for zero-`Pin`
//!   cross-thread migration and enforces that cancelled kernel DMA buffers drain before arena reset.
//! - [`Handler`] + [`drive`]: the one execution loop shared by `IoUring`, `Kqueue`, `SimHandler`,
//!   and deterministic [`Script`] unit tests.

#![no_std]
#![feature(register_tool, custom_inner_attributes)]
#![register_tool(blaze)]
#![blaze::sans_io]
#![deny(blaze::sans_io_core)]
#![deny(blaze::implicit_allocator)]
#![deny(blaze::process_static)]
#![forbid(unsafe_code)]

#[cfg(test)]
extern crate std;

pub mod script;
pub mod task_frame;

#[cfg(test)]
mod tests;

use core::fmt;

use blaze_own::FixedVec;

pub use script::{Expect, Script, ScriptError};
pub use task_frame::{DmaTicket, FrameBusy, TaskFrame, TaskFramePlant};

/// Something a machine may ask a [`Handler`] to perform.
///
/// Each concrete request type is `#[blaze::linear]` and fixes the exact type of its answer,
/// eliminating the monolithic `Effect`/`Done` surface and preventing mismatched completion arms.
pub trait Request: Sized {
    /// The typed answer returned to the [`Waiting`] state when this request completes.
    ///
    /// `'s` borrows driver-owned scratch bytes for a single step (`Read`); requests that take
    /// caller-owned DMA buffers (`ReadInto`, `WriteFrom`, `ReadAll`) ignore `'s` and return the
    /// owned buffer directly.
    type Answer<'s>;
}

/// Terminal success outcome of a machine.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Done<T>(pub T);

/// Terminal failure outcome of a machine.
///
/// Normal `Fail` exits must close any linear handles (or transfer them inside `E` to the caller);
/// only external [`Stopped`] cancellation abandons handles to driver reclamation.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Fail<E>(pub E);

/// Why a running machine was stopped externally by its handler.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CancelReason {
    /// Explicitly cancelled by a parent scope or sibling `Race` winner.
    Cancelled,
    /// The task's monotonic deadline expired.
    Deadline,
}

/// Bounded cleanup budget granted to [`Waiting::on_cancel`] when a state overrides default
/// abandonment to perform graceful teardown (closing a handle, removing a temp file, killing a
/// child process; rule 18 `bounded-progress`).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct GraceBudget {
    /// Maximum nanoseconds of virtual/monotonic time allowed for cleanup.
    pub max_ns: u64,
    /// Maximum cleanup transitions allowed before forced abandonment.
    pub max_steps: u32,
}

impl GraceBudget {
    /// Zero grace budget: immediate abandonment with no further IO steps.
    pub const NONE: Self = Self { max_ns: 0, max_steps: 0 };

    /// Standard bounded cleanup budget (50 ms, at most 16 cleanup steps).
    pub const DEFAULT: Self = Self { max_ns: 50_000_000, max_steps: 16 };
}

/// External cancellation or deadline notice delivered to a waiting state.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Stopped {
    pub reason: CancelReason,
    pub grace: GraceBudget,
}

impl Stopped {
    #[inline]
    pub const fn cancelled(grace: GraceBudget) -> Self {
        Self { reason: CancelReason::Cancelled, grace }
    }

    #[inline]
    pub const fn deadline(grace: GraceBudget) -> Self {
        Self { reason: CancelReason::Deadline, grace }
    }
}

/// What a [`Waiting`] state chooses to do when externally cancelled.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CancelAction<Next> {
    /// Abandon immediately; the handler reclaims any task-owned handles in its table.
    Abandon,
    /// Run bounded graceful cleanup under [`Stopped::grace`] before terminating.
    Cleanup(Next),
}

/// A linear request paired with the disjoint [`Waiting`] continuation state that receives its
/// answer.
///
/// Producing `(request, waiting)` by value avoids borrowing `self` across IO, so linear handles
/// move cleanly into `Close`, owned DMA buffers move cleanly into `ReadInto`/`WriteFrom`, and
/// paused states stay plain movable values with zero `Pin`.
#[blaze::linear]
#[derive(Debug)]
pub struct Ask<R, W> {
    pub request: R,
    pub waiting: W,
}

impl<R, W> Ask<R, W> {
    #[inline]
    pub const fn new(request: R, waiting: W) -> Self {
        Self { request, waiting }
    }

    #[blaze::consumes]
    #[inline]
    pub fn split(self) -> (R, W) {
        (self.request, self.waiting)
    }
}

/// Structured delegation to a child machine `Child`, resuming continuation `K` with the child's
/// terminal `Result<Child::Out, Child::Err>`.
#[blaze::linear]
#[derive(Debug)]
pub struct Call<Child, K> {
    pub child: Child,
    pub then: K,
}

impl<Child, K> Call<Child, K> {
    #[inline]
    pub const fn new(child: Child, then: K) -> Self {
        Self { child, then }
    }

    #[blaze::consumes]
    #[inline]
    pub fn split(self) -> (Child, K) {
        (self.child, self.then)
    }
}

/// One waiting state of an explicit sans-IO state machine.
///
/// Its associated types (`Waits`, `Next`) state the node's incoming answer type and every outgoing
/// edge directly in the signature. `Ctx` carries explicit non-storable capabilities needed during
/// transitions (such as `&BrandedBump<'id, Frame>` from `drop-with.md`), keeping state structs
/// compact and `#![blaze::sans_io]`-clean.
pub trait Waiting<Ctx = ()>: Sized {
    /// The typed request whose answer resumes this state.
    type Waits: Request;
    /// Every state or terminal outcome this state may transition to.
    ///
    /// Under `#![blaze::error_unions]`, this is an anonymous union (`Ask<Send, Sending> | Done<T> | Fail<E>`);
    /// on standard Rust crates, [`Step`] or a named `<State>Next` enum.
    type Next;

    /// Consumes this state with the answer to [`Self::Waits`] and returns the next transition.
    #[blaze::consumes]
    fn resume(self, answer: <Self::Waits as Request>::Answer<'_>, cx: &mut Ctx) -> Self::Next;

    /// Handles external cancellation or deadline expiry under [`Stopped::grace`].
    ///
    /// Defaults to [`CancelAction::Abandon`], letting the handler reclaim any task-owned handles
    /// once in-flight DMA buffers drain.
    #[blaze::consumes]
    fn on_cancel(self, _stopped: Stopped, _cx: &mut Ctx) -> CancelAction<Self::Next> {
        CancelAction::Abandon
    }
}

/// Portable 3-way transition outcome for Rust-edition crates (interoperable with anonymous unions
/// `Ask<R, W> | Done<T> | Fail<E>` in Blaze-edition crates).
#[derive(Debug)]
pub enum Step<R, W, T, E> {
    /// Submit `R` to the handler and resume `W` with its answer.
    Ask(Ask<R, W>),
    /// Machine completed successfully.
    Done(Done<T>),
    /// Machine failed with a typed domain error (all owned handles closed or returned in `E`).
    Fail(Fail<E>),
}

impl<R, W, T, E> Step<R, W, T, E> {
    #[inline]
    pub const fn ask(request: R, waiting: W) -> Self {
        Self::Ask(Ask { request, waiting })
    }

    #[inline]
    pub const fn done(value: T) -> Self {
        Self::Done(Done(value))
    }

    #[inline]
    pub const fn fail(err: E) -> Self {
        Self::Fail(Fail(err))
    }
}

/// Product request that submits BOTH `first` and `second` concurrently and resumes when the first
/// completes (structured `select`, e.g. `Race<RecvInto, Timer>`).
///
/// The handler cancels the losing branch and guarantees any DMA buffer lent to the loser is
/// returned only after the cancelled operation completes, never before.
#[blaze::linear]
#[derive(Debug, PartialEq, Eq)]
pub struct Race<A, B> {
    pub first: A,
    pub second: B,
}

impl<A, B> Race<A, B> {
    #[inline]
    pub const fn new(first: A, second: B) -> Self {
        Self { first, second }
    }

    #[blaze::consumes]
    #[inline]
    pub fn split(self) -> (A, B) {
        (self.first, self.second)
    }
}

/// Which arm of a [`Race`] completed first.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RaceOutcome<A, B> {
    First(A),
    Second(B),
}

impl<A: Request, B: Request> Request for Race<A, B> {
    type Answer<'s> = RaceOutcome<A::Answer<'s>, B::Answer<'s>>;
}

/// Bounded-N fan-out request (`CAP` inline slots, zero heap allocation) for saturating `io_uring`
/// or handling concurrent duplex streams (e.g. `dir walk`, `tree copy`, `stdout` + `stderr` +
/// `wait` in `spawn-and-collect`).
#[blaze::linear]
#[derive(Debug)]
pub struct Batch<R, const CAP: usize> {
    items: FixedVec<R, CAP>,
}

impl<R, const CAP: usize> Batch<R, CAP> {
    /// Creates an empty batch with compile-time capacity `CAP`.
    #[inline]
    pub fn new() -> Self {
        Self { items: FixedVec::new() }
    }

    /// Appends one request to the batch; refuses cleanly with the request returned if full.
    #[inline]
    pub fn try_push(&mut self, req: R) -> Result<usize, R> {
        let idx = self.items.len();
        self.items.push(req).map(|()| idx).map_err(|full| full.value)
    }

    /// Number of queued requests.
    #[inline]
    pub fn len(&self) -> usize {
        self.items.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Maximum requests this batch holds.
    #[inline]
    pub const fn cap(&self) -> usize {
        CAP
    }

    /// Borrows the queued requests in submission order.
    #[inline]
    pub fn as_slice(&self) -> &[R] {
        self.items.as_slice()
    }

    /// Consumes the batch and yields its requests in submission order.
    #[blaze::consumes]
    #[inline]
    pub fn into_items(self) -> FixedVec<R, CAP> {
        self.items
    }
}

impl<R, const CAP: usize> Default for Batch<R, CAP> {
    #[inline]
    fn default() -> Self {
        Self::new()
    }
}

/// One completed entry from a [`Batch`] request, paired with its submission slot index.
#[derive(Debug, PartialEq, Eq)]
pub struct BatchReady<A> {
    pub index: u16,
    pub answer: A,
}

impl<R: Request, const CAP: usize> Request for Batch<R, CAP> {
    type Answer<'s> = FixedVec<BatchReady<R::Answer<'s>>, CAP>;
}

/// One uniform step of a runnable [`Machine`].
#[derive(Debug)]
pub enum Turn<M, R, T, E> {
    /// Perform `request` and resume `next` with its answer.
    Ask { request: R, next: M },
    /// Machine completed successfully.
    Done(T),
    /// Machine failed with a typed domain error.
    Fail(E),
}

/// Uniform executable machine ABI targeted by BOTH explicit state-graph machines and compiled
/// one-shot `suspend` coroutines.
pub trait Machine<Ctx = ()>: Sized {
    /// The typed request union this machine emits.
    type Req: Request;
    /// Output on [`Turn::Done`].
    type Out;
    /// Error on [`Turn::Fail`].
    type Err;

    /// Advances the machine into its first request or immediate terminal result.
    #[blaze::consumes]
    fn start(self, cx: &mut Ctx) -> Turn<Self, Self::Req, Self::Out, Self::Err>;

    /// Resumes the paused machine with the answer to its last emitted request.
    #[blaze::consumes]
    fn resume(
        self,
        answer: <Self::Req as Request>::Answer<'_>,
        cx: &mut Ctx,
    ) -> Turn<Self, Self::Req, Self::Out, Self::Err>;

    /// Notifies the paused machine of external cancellation or deadline expiry.
    #[blaze::consumes]
    fn on_cancel(
        self,
        _stopped: Stopped,
        _cx: &mut Ctx,
    ) -> CancelAction<Turn<Self, Self::Req, Self::Out, Self::Err>> {
        CancelAction::Abandon
    }
}

/// Result of handling one request inside [`Handler::perform`].
#[derive(Debug)]
pub enum Perform<A> {
    /// The request completed with its typed answer.
    Answered(A),
    /// External cancellation or deadline expired before the request completed.
    Stopped(Stopped),
}

/// One-shot algebraic effect handler / IO driver for a [`Request`] family.
///
/// Implemented by native drivers (`IoUring`, `Kqueue`), deterministic simulation (`SimHandler`),
/// and unit-test [`Script`]s.
pub trait Handler<R: Request> {
    /// Driver-level failure (e.g. script mismatch, handle leak on normal exit, watchdog trip).
    type Error;

    /// Performs `request` and invokes `deliver` with the typed [`Perform`] outcome (letting lent
    /// `&'s [u8]` answers live cleanly on the handler's stack/scratch frame without global alloc).
    fn perform<Out>(
        &mut self,
        request: R,
        deliver: impl FnOnce(Perform<R::Answer<'_>>) -> Out,
    ) -> Result<Out, Self::Error>;
}

/// Outcome of driving a [`Machine`] to completion through [`drive`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DriveError<E, H> {
    /// Machine returned [`Turn::Fail`].
    Machine(E),
    /// Machine was stopped externally (cancellation or deadline) and abandoned cleanly.
    Stopped(Stopped),
    /// Driver/handler itself failed (script mismatch, leaked handle, etc.).
    Handler(H),
    /// Machine exceeded its step fuel bound (rule 18 `bounded-progress`).
    StepBudgetExhausted { max_steps: u32 },
}

impl<E: fmt::Display, H: fmt::Display> fmt::Display for DriveError<E, H> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DriveError::Machine(e) => write!(f, "machine failed: {e}"),
            DriveError::Stopped(s) => write!(f, "machine stopped ({:?})", s.reason),
            DriveError::Handler(h) => write!(f, "handler failed: {h}"),
            DriveError::StepBudgetExhausted { max_steps } => {
                write!(f, "machine exceeded bounded progress budget of {max_steps} steps")
            }
        }
    }
}

/// Options for [`drive`] (`#[blaze::args]`, rule 83 `options-struct`).
#[blaze::args]
#[derive(Clone, Copy, Debug)]
pub struct DriveOpts {
    /// Maximum request/resume steps allowed before returning [`DriveError::StepBudgetExhausted`]
    /// (rule 18 `bounded-progress`).
    pub max_steps: u32,
}

/// The one driver loop: start, perform, resume, repeat—with bounded step fuel and bounded
/// graceful cancellation cleanup.
pub fn drive<M, Ctx, H>(
    handler: &mut H,
    cx: &mut Ctx,
    machine: M,
    opts: DriveOpts,
) -> Result<M::Out, DriveError<M::Err, H::Error>>
where
    M: Machine<Ctx>,
    H: Handler<M::Req>,
{
    let mut turn = machine.start(cx);
    let mut steps: u32 = 0;
    let mut cancelling: Option<(Stopped, u32)> = None;

    loop {
        match turn {
            Turn::Done(out) => return Ok(out),
            Turn::Fail(err) => return Err(DriveError::Machine(err)),
            Turn::Ask { request, next } => {
                if let Some((stopped, left)) = cancelling {
                    if left == 0 {
                        return Err(DriveError::Stopped(stopped));
                    }
                    cancelling = Some((stopped, left - 1));
                } else {
                    if steps >= opts.max_steps {
                        return Err(DriveError::StepBudgetExhausted { max_steps: opts.max_steps });
                    }
                    steps += 1;
                }

                enum StepOutcome<T> {
                    Continue(T),
                    EnterCleanup(Stopped, T),
                    Abandon(Stopped),
                }

                let outcome = handler
                    .perform(request, |perf| match perf {
                        Perform::Answered(ans) => StepOutcome::Continue(next.resume(ans, cx)),
                        Perform::Stopped(stopped) => match next.on_cancel(stopped, cx) {
                            CancelAction::Abandon => StepOutcome::Abandon(stopped),
                            CancelAction::Cleanup(cleanup_turn) => {
                                StepOutcome::EnterCleanup(stopped, cleanup_turn)
                            }
                        },
                    })
                    .map_err(DriveError::Handler)?;

                match outcome {
                    StepOutcome::Continue(next_turn) => turn = next_turn,
                    StepOutcome::EnterCleanup(stopped, cleanup_turn) => {
                        cancelling = Some((stopped, stopped.grace.max_steps));
                        turn = cleanup_turn;
                    }
                    StepOutcome::Abandon(stopped) => return Err(DriveError::Stopped(stopped)),
                }
            }
        }
    }
}

/// Helper for composing a child [`Machine`] inside a parent [`Machine`] via [`Call`]: runs `child`
/// to completion over the same `handler` (mapping `child`'s requests into `PReq` with `lift` and
/// answers back with `lower`) without flattening `child`'s internal state enum into the parent.
pub fn run_sub<Child, Ctx, H>(
    handler: &mut H,
    cx: &mut Ctx,
    child: Child,
    opts: DriveOpts,
) -> Result<Child::Out, DriveError<Child::Err, H::Error>>
where
    Child: Machine<Ctx>,
    H: Handler<Child::Req>,
{
    drive(handler, cx, child, opts)
}
