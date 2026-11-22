//! [`Published`]: a value one owner fills once and every reader reads in place (D179's
//! PUBLISHED shape; docs/v2/plan/design/end-state.md).
//!
//! ```text
//! let table = Published::new("bstd.table");
//! let publisher = table.publisher()?;          // the one owner: handed out once
//! ... readers: table.get() (one atomic load), or table.need(cx)? (waits, named, bounded)
//! publisher.publish(built);                    // consumed: a second publish cannot be written
//! ```
//!
//! - **Why not [`Once`](super::Once), the claim table or [`PublishOnce`](super::PublishOnce)**
//!   (rule 30). `PublishOnce` is the storage: a value set once, read by one acquire load, with
//!   no owner and no waiting; `Published` is built on it and adds the two things it lacks, one
//!   owner and a reader that waits. `Once` is make-by-first-asker: whoever asks first builds the
//!   value, so it has no owner and a reader can become the maker. The claim table
//!   (blaze-core `claim_table`) publishes a record ordinal (`u64`) per key, one claim per key in
//!   a table; `Published` is one typed value with a known owner.
//! - **One owner, as a type.** [`Published::publisher`] hands out the [`Publisher`] once (an
//!   atomic flag: a second ask is [`PublishedError::PublisherTaken`]); the `Publisher` is linear
//!   (`#[blaze::linear]`, rule 76): it is not `Clone`, and it ends only by
//!   [`Publisher::publish`] or [`Publisher::abandon`], which consume it, so a second publish
//!   cannot be written and leaving it unended does not compile under blaze's linear check. An
//!   abandonment (the owner's making failed, or a panic unwound past the publisher) marks the
//!   value abandoned and wakes every waiter, who ends with [`PublishedError::Abandoned`]
//!   instead of waiting out its deadline.
//! - **A read is one atomic load.** [`Published::get`] is `PublishOnce::get`: an acquire load of
//!   its state, and the value in place.
//! - **A wait goes through `Cx`.** [`Published::need`] before publication waits with the
//!   context's deadline, in a span named for the value, charged to the worker the task is on as
//!   its wait returns (D171), like every other bstd wait, ending in a typed [`Stopped`].
//!
//! The publish and the look are a sans-IO core, [`PublishedCore`], whose waking is a type
//! parameter (`Wake`, Once's): `Published` wakes through its park, and the simulation
//! (`published/tests.rs`) drives the same core with a wake that lets readers step *during* the
//! publish, and plants [`PublishedPlant::WakeBeforeSet`].

#[cfg(blaze_check)]
use blaze_check::sync::atomic::{AtomicBool, Ordering};
#[cfg(not(blaze_check))]
use core::sync::atomic::{AtomicBool, Ordering};
use core::fmt;

use super::once::Wake;
use crate::{
    Stopped,
    cx::Cx,
    sync::{Lock, PublishOnce, Signal, wait_ready},
};

/// A planted bug for the simulation's control (rule 53).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum PublishedPlant {
    None,
    /// The publish wakes its waiters before it sets the value: a reader woken then looks,
    /// finds nothing, waits again, and is never woken (a lost wake-up).
    WakeBeforeSet,
}

impl blaze_own::planted::Plants for PublishedPlant {}

/// What one look at a [`PublishedCore`] found.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Look<'a, T> {
    Ready(&'a T),
    NotYet,
    /// Its publisher was dropped unpublished.
    Abandoned,
}

/// The sans-IO core of [`Published`]: the value, whether its publisher was handed out, and
/// whether it was abandoned.
pub(crate) struct PublishedCore<T> {
    value: PublishOnce<T>,
    handed: AtomicBool,
    abandoned: AtomicBool,
    plant: PublishedPlant,
}

impl<T> PublishedCore<T> {
    pub(crate) fn new(plant: PublishedPlant) -> PublishedCore<T> {
        PublishedCore { value: PublishOnce::new(), handed: AtomicBool::new(false), abandoned: AtomicBool::new(false), plant }
    }

    /// The value, once published: one acquire load.
    #[inline]
    pub(crate) fn get(&self) -> Option<&T> {
        self.value.get()
    }

    pub(crate) fn look(&self) -> Look<'_, T> {
        if let Some(value) = self.value.get() {
            return Look::Ready(value);
        }
        // Acquire: pairs with the abandonment's release.
        match self.abandoned.load(Ordering::Acquire) {
            true => Look::Abandoned,
            false => Look::NotYet,
        }
    }

    /// Whether this call is the one that hands the publisher out.
    pub(crate) fn hand(&self) -> bool {
        self.handed.compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed).is_ok()
    }

    /// `value` set, then `wake` told: the order is what makes a wake never lost (a waiter woken
    /// looks after the set). Only the one publisher calls it, once.
    pub(crate) fn publish<W: Wake>(&self, value: T, wake: &W) -> &T {
        if self.plant == PublishedPlant::WakeBeforeSet {
            wake.wake();
        }
        if self.value.set(value).is_err() {
            unreachable!("bstd::sync::Published: a second value published: its one publisher is linear and consumed by its publish");
        }
        if self.plant != PublishedPlant::WakeBeforeSet {
            wake.wake();
        }
        match self.value.get() {
            Some(value) => value,
            None => unreachable!("bstd::sync::Published: the value published above is not there"),
        }
    }

    /// The publisher was dropped unpublished: every waiter is woken to see it.
    pub(crate) fn abandon<W: Wake>(&self, wake: &W) {
        // Release: before any reader that sees it.
        self.abandoned.store(true, Ordering::Release);
        wake.wake();
    }
}

/// Where readers that need the value wait: a lock over nothing, held only to look and to wake,
/// and the signal a publish (or an abandonment) notifies. The same as `Once`'s park.
struct Park {
    lock: Lock<()>,
    ended: Signal,
}

impl Wake for Park {
    fn wake(&self) {
        // Taken before the notify: a reader that looked under the lock is in its wait by now,
        // so the notify reaches it.
        let held = self.lock.lock_briefly();
        self.ended.notify_all();
        drop(held);
    }
}

/// Why a [`Published`] gave no value or no publisher.
#[derive(Clone, PartialEq, Eq, Debug)] // clone: an error
pub enum PublishedError {
    /// The wait for the publish stopped so (its deadline, a cancel).
    Stopped(Stopped),
    /// Its publisher was dropped without publishing: nothing will be.
    Abandoned { published: &'static str },
    /// Its one publisher was already handed out.
    PublisherTaken { published: &'static str },
}

impl fmt::Display for PublishedError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PublishedError::Stopped(stopped) => write!(f, "bstd::sync::Published: waiting for its publish: {stopped}"),
            PublishedError::Abandoned { published } => {
                write!(f, "bstd::sync::Published {published}: its publisher was dropped without publishing")
            }
            PublishedError::PublisherTaken { published } => {
                write!(f, "bstd::sync::Published {published}: its one publisher was already handed out")
            }
        }
    }
}

impl core::error::Error for PublishedError {}

/// A value one owner fills once; see the module's doc.
pub struct Published<T> {
    core: PublishedCore<T>,
    park: Park,
}

impl<T> Published<T> {
    /// Empty, named `name`: the span its waits open and the name its errors give.
    pub fn new(name: &'static str) -> Published<T> {
        Published::planted(name, PublishedPlant::None)
    }

    pub(crate) fn planted(name: &'static str, plant: PublishedPlant) -> Published<T> {
        Published { core: PublishedCore::new(plant), park: Park { lock: Lock::new(name, ()), ended: Signal::new(name) } }
    }

    pub fn name(&self) -> &'static str {
        self.park.lock.name()
    }

    /// Its one publisher; a second ask is [`PublishedError::PublisherTaken`].
    pub fn publisher(&self) -> Result<Publisher<'_, T>, PublishedError> {
        match self.core.hand() {
            true => Ok(Publisher { arm: Arm { of: self, armed: true } }),
            false => Err(PublishedError::PublisherTaken { published: self.name() }),
        }
    }

    /// The value, once published: one atomic load; never waits.
    #[inline]
    pub fn get(&self) -> Option<&T> {
        self.core.get()
    }

    /// The value, waited for under `cx` (its deadline, in a span named for this value, charged
    /// to the worker the task is on) while it is not yet published.
    pub fn need(&self, cx: &Cx) -> Result<&T, PublishedError> {
        if let Some(value) = self.core.get() {
            return Ok(value);
        }
        let cx = cx.span(self.name());
        let looked = wait_ready(&cx, &self.park.lock, &self.park.ended, |_| match self.core.look() {
            Look::Ready(value) => Some(Ok(value)),
            Look::Abandoned => Some(Err(PublishedError::Abandoned { published: self.name() })),
            Look::NotYet => None,
        });
        looked.map_err(PublishedError::Stopped)?
    }

    /// The value out, through the only reference.
    pub fn into_inner(self) -> Option<T> {
        self.core.value.into_inner()
    }
}

impl<T> fmt::Debug for Published<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let state = match self.core.look() {
            Look::Ready(_) => "published",
            Look::Abandoned => "abandoned",
            Look::NotYet => "empty",
        };
        write!(f, "Published({}: {state})", self.name())
    }
}

/// The one owner of a [`Published`]'s value: consumed by [`Publisher::publish`]; dropped
/// unpublished, it abandons the value (its inner `Arm`'s drop, the abandonment path).
#[blaze::linear]
#[must_use = "a publisher publishes its value with publish(value); dropped, the value is abandoned"]
pub struct Publisher<'a, T> {
    arm: Arm<'a, T>,
}

/// The publisher's abandonment path.
struct Arm<'a, T> {
    of: &'a Published<T>,
    /// Whether the value is still this publisher's to publish: cleared by `publish`.
    armed: bool,
}

impl<T> Drop for Arm<'_, T> {
    fn drop(&mut self) {
        if self.armed {
            self.of.core.abandon(&self.of.park);
        }
    }
}

impl<'a, T> Publisher<'a, T> {
    /// `value` published, every waiter woken: the value every reader now gets.
    pub fn publish(self, value: T) -> &'a T {
        let Publisher { mut arm } = self;
        arm.armed = false;
        let of = arm.of;
        of.core.publish(value, &of.park)
    }

    /// Nothing will be published (the owner's making failed): every waiter, now and later,
    /// ends with [`PublishedError::Abandoned`] instead of waiting out its deadline.
    pub fn abandon(self) {
        let Publisher { arm } = self;
        // The arm's drop, armed, is the abandonment.
        drop(arm);
    }
}

#[cfg(test)]
mod tests;
