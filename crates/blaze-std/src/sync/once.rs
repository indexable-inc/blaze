//! [`Once`]: a value made once, by its first asker, while every other asker waits for it under
//! its own `Cx` (std.md, "Concurrency: what to reach for"); Rust std's `OnceLock`, as a value.
//!
//! ```text
//! let table = Once::new("bstd.table");
//! let built = table.get_or_make(cx, |cx| build(cx, &inputs))?;   // made once, by one asker
//! ```
//!
//! - **A value, never a `static`.** A `Once` is owned and passed (in a struct a task holds, in a
//!   `Shared`): bstd keeps no global mutable state, so a program's "init once" is a field of
//!   what its entry made, and a test makes a fresh one.
//! - **One maker at a time; no lock across the making.** The first asker claims the making (one
//!   atomic flag, [`OnceCore`]) and runs `make` holding no lock, so `make` may wait (read a
//!   file, ask the network) under its own deadline. Every other asker waits for the claim to
//!   end through its `Cx`: a wait with the context's deadline, in a span named for the
//!   `Once`, ending in a typed [`Stopped`]. The lock under that wait is held only to look at the
//!   flag and to wake waiters, never while anything is made.
//! - **A failure publishes nothing.** A `make` that returns an error gives it to its asker
//!   ([`OnceError::Failed`]) and ends the claim: the next asker (a waiter included) makes it
//!   again. A `make` that panics ends the claim the same way as it unwinds (rule
//!   panics-fail-one-job). No poisoning, as the locks have none.
//! - Asking a `Once` from inside its own `make` waits on itself: it ends at the deadline.
//!
//! The claim and the publish are a sans-IO core, [`OnceCore`], whose waking is a type parameter
//! ([`Wake`]): `Once` wakes its waiters through its lock and signal, and the model test
//! (`once_tests.rs`, `--cfg blaze_check`) drives the same core from two model threads with a
//! wake that does nothing and a yield, and plants [`OncePlant::ClaimSkipsRecheck`].

#[cfg(blaze_check)]
use blaze_check::sync::atomic::{AtomicBool, Ordering};
#[cfg(not(blaze_check))]
use core::sync::atomic::{AtomicBool, Ordering};
use core::fmt;

use crate::{
    Stopped,
    cx::Cx,
    sync::{Lock, PublishOnce, Signal, wait_ready},
};

/// A planted bug for the model's control (rule 53).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum OncePlant {
    None,
    /// A claim does not look at the value again after it won the flag: an asker that looked,
    /// saw nothing, and won the flag only after the first maker published and let go, makes a
    /// second time.
    ClaimSkipsRecheck,
}

/// What a claim's end tells: the core calls it each time the making flag is let go, so every
/// asker waiting for the flag looks again.
pub(crate) trait Wake {
    fn wake(&self);
}

/// The sans-IO core of [`Once`]: the value, published once, and the flag of its one maker.
pub(crate) struct OnceCore<T> {
    done: PublishOnce<T>,
    making: AtomicBool,
    plant: OncePlant,
}

/// What one look at the core found.
pub(crate) enum Claim<'a, T, W: Wake> {
    /// The value, published.
    Done(&'a T),
    /// Another asker is making it.
    Busy,
    /// This asker makes it: the claim ends with [`Making::publish`], or when it is dropped
    /// (a failure, an unwind), which lets the flag go and wakes the waiters.
    Claimed(Making<'a, T, W>),
}

/// The making of a [`OnceCore`]'s value, held by its one maker.
pub(crate) struct Making<'a, T, W: Wake> {
    core: &'a OnceCore<T>,
    wake: &'a W,
}

impl<T> OnceCore<T> {
    pub(crate) fn new(plant: OncePlant) -> OnceCore<T> {
        OnceCore { done: PublishOnce::new(), making: AtomicBool::new(false), plant }
    }

    /// The value, once published.
    pub(crate) fn get(&self) -> Option<&T> {
        self.done.get()
    }

    /// Whether an asker holds the making now.
    pub(crate) fn busy(&self) -> bool {
        // Acquire: pairs with the release that lets the flag go, after the value's publish.
        self.making.load(Ordering::Acquire)
    }

    /// The value, or the making claimed for this asker, or that another holds it. `wake` is
    /// told whenever the flag this call or its [`Making`] took is let go.
    pub(crate) fn claim<'a, W: Wake>(&'a self, wake: &'a W) -> Claim<'a, T, W> {
        if let Some(value) = self.done.get() {
            return Claim::Done(value);
        }
        // Acquire on the win: a maker that published and let go before is seen, with its value.
        if self.making.compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed).is_err() {
            return Claim::Busy;
        }
        let making = Making { core: self, wake };
        // Between the look above and the win, a maker may have published and let go: look again,
        // under the flag, so a value is made at most once.
        if self.plant != OncePlant::ClaimSkipsRecheck
            && let Some(value) = self.done.get()
        {
            drop(making);
            return Claim::Done(value);
        }
        Claim::Claimed(making)
    }
}

impl<'a, T, W: Wake> Making<'a, T, W> {
    /// `value` published, the flag let go, the waiters woken: the value every asker now gets.
    pub(crate) fn publish(self, value: T) -> &'a T {
        let core = self.core;
        if core.done.set(value).is_err() {
            // The flag is held by one asker and only its holder publishes: a value already
            // there means two makers held it (the planted bug), never the code as it is.
            unreachable!("bstd::sync::Once: a second value published: the making was claimed twice");
        }
        drop(self);
        match core.done.get() {
            Some(value) => value,
            None => unreachable!("bstd::sync::Once: the value published above is not there"),
        }
    }
}

impl<T, W: Wake> Drop for Making<'_, T, W> {
    fn drop(&mut self) {
        // Release: the value's publish (if any) before an asker that sees the flag let go.
        self.core.making.store(false, Ordering::Release);
        self.wake.wake();
    }
}

/// Where waiters for a claim wait: a lock over nothing, held only to look at the flag and to
/// wake, and the signal a claim's end notifies.
struct Park {
    lock: Lock<()>,
    ended: Signal,
}

impl Wake for Park {
    fn wake(&self) {
        // Taken before the notify: a waiter that looked at the flag under the lock is in its
        // wait by now, so the notify reaches it (no lost wake-up).
        let held = self.lock.lock_briefly();
        self.ended.notify_all();
        drop(held);
    }
}

/// Why [`Once::get_or_make`] gave no value.
#[derive(Clone, PartialEq, Eq, Debug)] // clone: an error
pub enum OnceError<E> {
    /// The wait for another asker's making stopped so (its deadline, a cancel): that making
    /// goes on for the others.
    Stopped(Stopped),
    /// This asker's `make` failed with `error`: nothing was published, and the next asker
    /// makes it again.
    Failed { once: &'static str, error: E },
}

impl<E: fmt::Display> fmt::Display for OnceError<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            OnceError::Stopped(stopped) => write!(f, "bstd::sync::Once: waiting for its maker: {stopped}"),
            OnceError::Failed { once, error } => write!(f, "bstd::sync::Once {once}: making it failed: {error}"),
        }
    }
}

impl<E: fmt::Debug + fmt::Display> core::error::Error for OnceError<E> {}

/// A value made once, by its first asker; see the module's doc.
pub struct Once<T> {
    core: OnceCore<T>,
    park: Park,
}

impl<T> Once<T> {
    /// Empty, named `name`: the span its waits open and the name its errors give.
    pub fn new(name: &'static str) -> Once<T> {
        Once::planted(name, OncePlant::None)
    }

    pub(crate) fn planted(name: &'static str, plant: OncePlant) -> Once<T> {
        Once { core: OnceCore::new(plant), park: Park { lock: Lock::new(name, ()), ended: Signal::new(name) } }
    }

    /// Its name.
    pub fn name(&self) -> &'static str {
        self.park.lock.name()
    }

    /// The value, once made; never waits.
    pub fn get(&self) -> Option<&T> {
        self.core.get()
    }

    /// The value: the one made, or made now by `make` (under `cx`'s span named for the `Once`)
    /// when this asker is the first, or waited for under `cx` while another asker makes it.
    /// No lock is held while `make` runs.
    pub fn get_or_make<E>(&self, cx: &Cx, make: impl FnOnce(&Cx) -> Result<T, E>) -> Result<&T, OnceError<E>> {
        if let Some(value) = self.core.get() {
            return Ok(value);
        }
        let cx = cx.span(self.name());
        let mut make = Some(make);
        // Each turn either returns or waits, under `cx`'s deadline, for a claim to end: the
        // loop ends by the value, this asker's own making, or the deadline.
        loop {
            match self.core.claim(&self.park) {
                Claim::Done(value) => return Ok(value),
                Claim::Claimed(making) => {
                    let Some(make) = make.take() else {
                        unreachable!("bstd::sync::Once: an asker claims the making at most once: it returns after")
                    };
                    return match make(&cx) {
                        Ok(value) => Ok(making.publish(value)),
                        // `making` is dropped here: the flag let go, the waiters woken.
                        Err(error) => Err(OnceError::Failed { once: self.name(), error }),
                    };
                }
                Claim::Busy => {
                    wait_ready(&cx, &self.park.lock, &self.park.ended, |_| (!self.core.busy()).then_some(()))
                        .map_err(OnceError::Stopped)?;
                }
            }
        }
    }

    /// The value out, through the only reference.
    pub fn into_inner(self) -> Option<T> {
        self.core.done.into_inner()
    }

    /// The making claimed by a test, as another task's making in flight would hold it.
    #[cfg(test)]
    pub(crate) fn claim_for_test(&self) -> Option<Making<'_, T, impl Wake>> {
        match self.core.claim(&self.park) {
            Claim::Claimed(making) => Some(making),
            Claim::Done(_) | Claim::Busy => None,
        }
    }
}

impl<T> fmt::Debug for Once<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let state = match (self.core.get().is_some(), self.core.busy()) {
            (true, _) => "made",
            (false, true) => "being made",
            (false, false) => "empty",
        };
        write!(f, "Once({}: {state})", self.name())
    }
}
