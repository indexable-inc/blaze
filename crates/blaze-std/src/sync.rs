//! Shared mutation across threads, and its cheaper one-thread versions (docs/v2/plan/design/std.md,
//! "Shared mutation"): the plain name is safe across threads, `Local` marks the version for one
//! thread. Each is the one structure its rule names, re-exported under bstd's name, so each is
//! the same type as the original and code moves to the new name with no conversion.
//!
//! | bstd | the original |
//! |---|---|
//! | [`Shared`] / [`LocalShared`] | `std::sync::Arc` / `std::rc::Rc` |
//! | [`Lock`], [`RwLock`] | `blaze_core::lock::{Lock, ReadLock}`, the named locks (rule `standard-structures`) |
//! | [`LocalLock`] | `std::cell::RefCell` |
//! | [`Once`], [`Lazy`] / [`LocalOnce`], [`LocalLazy`] | `OnceLock`, `LazyLock` / `OnceCell`, `LazyCell` |
//! | [`Atomic`] | `core::sync::atomic::Atomic<T>`, the one generic atomic (`AtomicU32` is `Atomic<u32>`) |
//! | [`SpinWait`] | `blaze_spin::SpinWait`, the one bounded spin |
//! | [`reclaim`] | `blaze_reclaim`: memory freed once every task that could read it ended |
//!
//! No poisoning: [`Lock::lock`] and [`RwLock::read`]/[`RwLock::write`] return the guard, not a
//! `Result` (they are `parking_lot`'s underneath). They do not take a `Cx` yet: a contended
//! acquire waits in the lock's own park with the profiler's span, not through the task's one
//! wait with its deadline; that is the lock's change, in blaze-core, not this facade's.
//!
//! `Once`, `Lazy`, `LocalOnce` and `LocalLazy` are values to own and pass: bstd puts none of
//! them in a `static` (no global mutable state), and `.clippy.toml` bans their originals by
//! name for that reason, so a caller outside bstd names why it holds one.

#![expect(
    clippy::disallowed_types,
    reason = "bstd's names for OnceLock, LazyLock, OnceCell and LazyCell: the re-export is their \
              one home, and the ban stays on every use of them"
)]

pub use alloc::{rc::Rc as LocalShared, sync::Arc as Shared};
pub use core::{
    cell::{LazyCell as LocalLazy, OnceCell as LocalOnce, RefCell as LocalLock},
    sync::atomic::{Atomic, Ordering},
};

// The two that wait on first use are Rust std's until bstd's own wait is under them (S4's
// ban list, `crates/blaze-std/src/rt.rs`).
pub use crate::rt::{LazyLock as Lazy, OnceLock as Once};

pub use blaze_core::lock::{LockGuard, ReadGuard, ReadLock as RwLock, Signal, WriteGuard};

use crate::{Stopped, cx::Cx};

/// The one lock, named. Taking it is a wait like any other (docs/v2/plan/design/std.md, "One
/// wait"): [`Lock::lock`] takes the task's `Cx`, so the wait has a deadline and a span, and
/// [`Lock::try_lock`] never waits. There is no `lock()` that waits for ever.
///
/// Over `blaze_core::lock::Lock`, which names the lock and records who waited behind whom.
/// The wait is that lock's bounded one (`try_lock_for`), on the wall clock: under the
/// `Inline` layout a lock is contended only by another thread, never by another task of the
/// same simulated world, so the simulation's virtual clock has no lock wait to model yet.
pub struct Lock<T>(blaze_core::lock::Lock<T>);

impl<T> Lock<T> {
    /// A lock named `name` over `value`.
    pub const fn new(name: &'static str, value: T) -> Lock<T> {
        Lock(blaze_core::lock::Lock::new(name, value))
    }

    /// The lock's name: the span its waits open.
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

    /// The value, through the only reference to the lock: no wait.
    pub fn get_mut(&mut self) -> &mut T {
        self.0.get_mut()
    }

    /// The value, the lock consumed.
    pub fn into_inner(self) -> T {
        self.0.into_inner()
    }
}
pub use blaze_spin::SpinWait;

/// Reclamation at task ends: a value unlinked from a shared table is dropped once every thread
/// that was in a task when it was unlinked has left that task (`blaze_reclaim`). Its epoch and
/// slots are blaze-reclaim's own process-wide state, not bstd's; bstd holds none.
pub mod reclaim {
    pub use blaze_reclaim::{Grace, Guard, Horizon, Limbo, Passed, enter, pinned};
}
