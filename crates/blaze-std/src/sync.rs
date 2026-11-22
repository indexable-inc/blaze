//! Shared mutation across threads, and its cheaper one-thread versions (docs/v2/plan/design/std.md,
//! "Shared mutation" and "Concurrency: what to reach for"): the plain name is safe across
//! threads, `Local` marks the version for one thread.
//!
//! What to reach for first is the primitive named for the pattern, not a lock and an atomic:
//!
//! | you want to | use |
//! |---|---|
//! | work on parts of one slice or table at once | [`crate::par`] |
//! | build a table, then share it read-only | [`Building`] then [`Frozen`] |
//! | readers that never block a writer who replaces the whole value | [`Swap`] |
//! | compute a key once while others asking for it wait | [`Flight`] |
//! | make one value once while others asking for it wait | [`Once`] |
//! | hand values from one task to another | [`Chan`] (bounded only) |
//! | count, flag, publish once, number events | [`Counter`], [`Flag`], [`PublishOnce`], [`Seq`] |
//! | hold two locks | [`RankedLock`], in rank order ([`ranks`]) |
//!
//! And the pieces under them, each the one structure its rule names:
//!
//! | bstd | the original |
//! |---|---|
//! | [`Shared`] / [`LocalShared`] | handles over `alloc::sync::Arc` / `alloc::rc::Rc` ([`crate::own`]) |
//! | [`Lock`], [`RwLock`] | `blaze_core::lock::{Lock, ReadLock}`, the named locks (rule `standard-structures`) |
//! | [`LocalLock`], [`LocalOnce`], [`LocalLazy`] | `core::cell::{RefCell, OnceCell, LazyCell}`, each checking in a debug build that it ends on the thread that made it ([`home`]) |
//! | [`Once`] | `OnceLock`, a value: its waiters wait through the `Cx`, and no lock is held while it is made |
//! | `Lazy` | `LazyLock`: not here; a [`Once`] beside the function that makes it |
//! | [`raw::Atomic`] | `core::sync::atomic::Atomic<T>`, the one generic atomic, under a longer name: prefer the named ones |
//! | [`SpinWait`] | `blaze_spin::SpinWait`, the one bounded spin |
//! | [`reclaim`] | `blaze_reclaim`: memory freed once every task that could read it ended |
//!
//! No poisoning: a guard is never a `Result` of a poisoned lock (the locks are `parking_lot`'s
//! underneath). [`Lock::lock`] takes the task's `Cx`: the wait has its deadline and a span
//! named for the lock, and ends in a [`Stopped`]. [`RwLock::read`] and [`RwLock::write`] do
//! not take one: prefer [`Swap`], whose readers never wait.
//!
//! Nothing here starts a thread or sleeps: bstd has no bare thread spawn (work starts as a
//! task of a [`crate::Scope`]) and no sleep but [`crate::Clock::sleep`], which takes a `Cx`.
//! bstd has no thread-local storage either.
//!
//! `PublishOnce`, `LocalOnce` and `LocalLazy` are values to own and pass: bstd puts none of
//! them in a `static` (no global mutable state), and `.clippy.toml` bans their originals by
//! name for that reason, so a caller outside bstd names why it holds one.

#![expect(
    clippy::disallowed_types,
    reason = "bstd's names for OnceLock, LazyLock, OnceCell and LazyCell: the re-export is their \
              one home, and the ban stays on every use of them"
)]

#[expect(unsafe_code, reason = "`PublishOnce`'s one slot, written once before it is published, with its SAFETY")]
mod atomics;
pub mod chan;
pub mod flight;
pub mod freeze;
pub mod home;
pub mod ranks;
#[expect(unsafe_code, reason = "`Swap`'s published pointer and its reclamation, with their SAFETY")]
pub mod swap;
// A value made once by its first asker while the others wait through their `Cx`.
mod once;
#[cfg(test)]
mod once_tests;
// A value one owner fills once while readers read it in place or wait through their `Cx`
// (D179's PUBLISHED).
mod published;

pub use atomics::{Counter, Flag, PublishOnce, Seq};
pub use chan::{Chan, Closed, RecvError, SendError};
pub use flight::{Flight, FlightError};
pub use freeze::{Building, Frozen};
pub use home::{LocalLazy, LocalLock, LocalOnce};
pub use ranks::{Order, Rank, RankedGuard, RankedLock, Unranked};
pub use once::{Once, OnceError};
pub use published::{Published, PublishedError, Publisher};
pub use swap::Swap;

pub use crate::own::{LocalShared, Released, Shared};

// `Once` is Rust std's `OnceLock` on bstd's own wait (the `Cx`'s); `Lazy` (`LazyLock`) stays out
// (S4's ban list, `crates/blaze-std/src/rt.rs`). A value published once with no waiting at all
// is [`PublishOnce`].

pub use blaze_core::lock::{LockGuard, ReadGuard, Signal, WriteGuard};

/// A lock many read and one writes: `blaze_core::lock::ReadLock`. Its `read` and `write` wait
/// with no `Cx` and no deadline, and a reader blocks a writer. Prefer [`Swap`]: its readers
/// never wait and never hold a writer back, and a writer publishes a whole new value. Reach for
/// this only where a writer changes a small part of a large value in place, and say why.
pub use blaze_core::lock::ReadLock as RwLock;

use crate::{Stopped, cx::Cx};

/// The generic atomic, under a longer name. Prefer the named ones ([`Counter`], [`Flag`],
/// [`PublishOnce`], [`Seq`]): each chooses its memory ordering inside and says why in one line,
/// where an `Atomic<T>` asks every caller to choose one at every call.
pub mod raw {
    pub use core::sync::atomic::{Atomic, Ordering};
}

/// The one lock, named. Taking it is a wait like any other (docs/v2/plan/design/std.md, "One
/// wait"): [`Lock::lock`] takes the task's `Cx`, so the wait has a deadline and a span, and
/// [`Lock::try_lock`] never waits. There is no `lock()` that waits for ever.
///
/// Over `blaze_core::lock::Lock`, which names the lock and records who waited behind whom.
/// The wait is that lock's bounded one (`try_lock_for`), on the wall clock: under the
/// `Inline` layout a lock is contended only by another thread, never by another task of the
/// same simulated world, so the simulation's virtual clock has no lock wait to model yet.
///
/// Holding two: [`RankedLock`], whose ranks make the order a type.
pub struct Lock<T>(blaze_core::lock::Lock<T>);

impl<T> Lock<T> {
    /// A lock named `name` over `value`.
    #[blaze::pure]
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub const fn new(name: &'static str, value: T) -> Lock<T> {
        Lock(blaze_core::lock::Lock::new(name, value))
    }

    /// The lock's name: the span its waits open.
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn name(&self) -> &'static str {
        self.0.name()
    }

    /// The lock if it is free now; never waits.
    pub fn try_lock(&self) -> Option<LockGuard<'_, T>> {
        self.0.try_lock()
    }

    /// The lock, waited for until `cx`'s deadline in a span named for the lock: the guard, or
    /// why the wait stopped ([`Stopped::Cancelled`] when `cx` was cancelled before the wait,
    /// [`Stopped::Deadline`] when the deadline passed with the lock still held). A context
    /// with no deadline ([`blaze_core::wait::Deadline::NEVER`]) said its waits have none.
    pub fn lock(&self, cx: &Cx) -> Result<LockGuard<'_, T>, Stopped> {
        cx.check()?;
        if let Some(guard) = self.0.try_lock() {
            return Ok(guard);
        }
        let cx = cx.span(self.0.name());
        let Some(deadline) = cx.deadline().finite() else {
            return Ok(self.0.lock());
        };
        match self.0.try_lock_for(deadline.left(cx.io().now())) {
            Some(guard) => Ok(guard),
            None => Err(Stopped::Deadline { span: cx.name(), at: deadline.get() }),
        }
    }

    /// The lock, for a lock this crate holds only for a few moves and never across code it
    /// does not own (a [`Flight`]'s table, a [`Chan`]'s queue): its wait is bounded by that,
    /// so a destructor, which has no `Cx`, may take it.
    pub(crate) fn lock_briefly(&self) -> LockGuard<'_, T> {
        self.0.lock()
    }

    /// The value, through the only reference to the lock: no wait.
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn get_mut(&mut self) -> &mut T {
        self.0.get_mut()
    }

    /// The value, the lock consumed.
    #[blaze::nonblocking]
    pub fn into_inner(self) -> T {
        self.0.into_inner()
    }
}

/// The one wait for a condition another task makes true on what `lock` guards: until `ready`
/// gives a value, or `cx`'s deadline or cancellation, with [`crate::Cx`]'s typed stops. The
/// condition's maker notifies `signal` after it changes the value.
///
/// - In a simulated world the wait is the world's ([`Cx`]'s `wait_for`): under `Inline`
///   nothing else of the world runs meanwhile, so a condition only another task could make
///   true ends the wait at its deadline on the virtual clock, and no wall time passes.
/// - On a native world the condition's maker is another thread: the wait is `signal`'s,
///   bounded by the time left to the deadline, as [`Lock::lock`]'s is. A cancellation from
///   another thread is seen when the wait wakes.
pub(crate) fn wait_ready<T, R>(
    cx: &Cx,
    lock: &Lock<T>,
    signal: &Signal,
    mut ready: impl FnMut(&mut T) -> Option<R>,
) -> Result<R, Stopped> {
    let io = cx.io();
    if io.with_sim(|_| ()).is_some() {
        return cx.wait_for(io, || lock.0.try_lock().and_then(|mut held| ready(&mut held)));
    }
    // The lock's own wait, if any, is the lock record's (blaze-core's slow path, the thread's
    // lock table), never charged here as well.
    let mut held = lock.lock(cx)?;
    if let Some(got) = ready(&mut held) {
        return Ok(got);
    }
    // Not ready: the wait on `signal` is charged to the worker this task is on when it returns
    // (D171), as the simulated branch's `wait_for` is; a ready look above charges nothing.
    let cx = cx.span(signal.name());
    cx.charged(io, signal.name(), None, || {
        loop {
            cx.check()?;
            let Some(deadline) = cx.deadline().finite() else {
                signal.wait(&mut held);
                if let Some(got) = ready(&mut held) {
                    return Ok(got);
                }
                continue;
            };
            let now = io.now();
            let stopped = Stopped::Deadline { span: cx.name(), at: deadline.get() };
            if deadline.passed(now) {
                return Err(stopped);
            }
            if signal.wait_for(&mut held, deadline.left(now)) {
                // The deadline passed: one last look, then the stop.
                return ready(&mut held).ok_or(stopped);
            }
            if let Some(got) = ready(&mut held) {
                return Ok(got);
            }
        }
    })
}

pub use blaze_spin::SpinWait;

/// Reclamation at task ends: a value unlinked from a shared table is dropped once every thread
/// that was in a task when it was unlinked has left that task (`blaze_reclaim`). Its epoch and
/// slots are blaze-reclaim's own process-wide state, not bstd's; bstd holds none.
pub mod reclaim {
    pub use blaze_reclaim::{Grace, Guard, Horizon, Limbo, Passed, enter, pinned};
}
