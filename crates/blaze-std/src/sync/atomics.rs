//! The named atomics: one type per use, each with its memory ordering chosen inside and the
//! reason in its doc (std.md, "Concurrency: what to reach for"). The generic one is
//! [`crate::sync::raw::Atomic`], for what none of these says.
//!
//! | type | for | ordering |
//! |---|---|---|
//! | [`Counter`] | a count read for its total (statistics, a budget spent) | relaxed |
//! | [`Flag`] | "it happened", and what was written before it | release set, acquire read |
//! | [`PublishOnce`] | a value set once and read many times, never waiting | acquire/release on its state |
//! | [`Seq`] | a sequence number that publishes what came before each step | acquire/release |
//!
//! None of them waits: a reader that wants to wait for a flag or a value waits on a [`crate::sync::Chan`]
//! or a [`crate::sync::Flight`], whose waits take `cx`.

use core::{cell::UnsafeCell, fmt, mem::MaybeUninit};
// Under the model checker every named atomic is a schedule point, so a model over code built on
// them (`sync::once`'s) sees their loads and stores; outside a model the shims are `std`'s.
#[cfg(blaze_check)]
use blaze_check::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
#[cfg(not(blaze_check))]
use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};

use crate::Plant;

/// A count. Relaxed: a count publishes nothing; its readers want the total, never what was
/// written before an add. Wraps past `u64::MAX` (584 years at one a nanosecond).
pub struct Counter(AtomicU64);

impl Counter {
    #[blaze::pure]
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub const fn new() -> Counter {
        Counter(AtomicU64::new(0))
    }

    /// Adds `n`; the count before.
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn add(&self, n: u64) -> u64 {
        self.0.fetch_add(n, Ordering::Relaxed)
    }

    /// The count now.
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn get(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }

    /// The count, and zero in its place: one step, so no add between the two is lost.
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn take(&self) -> u64 {
        self.0.swap(0, Ordering::Relaxed)
    }
}

impl Default for Counter {
    fn default() -> Counter {
        Counter::new()
    }
}

/// "It happened", once. Release on set, acquire on read: a reader that sees it set sees every
/// write its setter made before setting it. One way: only the sole owner clears it.
pub struct Flag(AtomicBool);

impl Flag {
    #[blaze::pure]
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub const fn new() -> Flag {
        Flag(AtomicBool::new(false))
    }

    /// Sets it: whether it was set already.
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn set(&self) -> bool {
        self.0.swap(true, Ordering::AcqRel)
    }

    /// Whether it is set.
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn is_set(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }

    /// Clears it, through the only reference: no reader is left to be told.
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn reset(&mut self) {
        *self.0.get_mut() = false;
    }
}

impl Default for Flag {
    fn default() -> Flag {
        Flag::new()
    }
}

/// A sequence number. Acquire/release: a reader that sees step `n` sees every write made
/// before the advance to `n`, and each advance follows the ones before it. For "which version
/// is this"; for a total alone, [`Counter`] costs less.
pub struct Seq(AtomicU64);

impl Seq {
    /// At step 0.
    #[blaze::pure]
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub const fn new() -> Seq {
        Seq(AtomicU64::new(0))
    }

    /// The next step, taken: unique, and greater than every step taken before it.
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn advance(&self) -> u64 {
        self.0.fetch_add(1, Ordering::AcqRel).wrapping_add(1)
    }

    /// The last step taken.
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn now(&self) -> u64 {
        self.0.load(Ordering::Acquire)
    }
}

impl Default for Seq {
    fn default() -> Seq {
        Seq::new()
    }
}

const EMPTY: u8 = 0;
const WRITING: u8 = 1;
const READY: u8 = 2;

/// A value set once and then read by anyone, never waiting. Acquire/release on its state: a
/// reader that sees the value sees all of it. A second `set`, or one racing the first, gets
/// its value back; a read while the first is being written sees nothing yet.
pub struct PublishOnce<T> {
    state: AtomicU8,
    slot: UnsafeCell<MaybeUninit<T>>,
}

// SAFETY: the slot is written once, by the one `set` that moved the state from `EMPTY` to
// `WRITING`, and read only after the release store of `READY`, which an acquire load saw; so a
// value is shared between threads only as `&T` (needs `T: Sync`) and is dropped by whichever
// thread drops the `PublishOnce` (needs `T: Send`).
unsafe impl<T: Send + Sync> Sync for PublishOnce<T> {}

impl<T> PublishOnce<T> {
    /// Empty.
    #[blaze::pure]
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub const fn new() -> PublishOnce<T> {
        PublishOnce { state: AtomicU8::new(EMPTY), slot: UnsafeCell::new(MaybeUninit::uninit()) }
    }

    /// Publishes `value`, or gives it back when a value is set or being set.
    #[blaze::nonblocking]
    pub fn set(&self, value: T) -> Result<(), T> {
        self.set_as(value, Plant::None)
    }

    #[cfg(test)]
    pub(crate) fn set_planted(&self, value: T, plant: Plant) -> Result<(), T> {
        self.set_as(value, plant)
    }

    fn set_as(&self, value: T, plant: Plant) -> Result<(), T> {
        // Acquire on the win: nothing of the slot is read before it, but it orders this
        // writer after a racing loser's load, which has nothing to publish.
        match self.state.compare_exchange(EMPTY, WRITING, Ordering::Acquire, Ordering::Relaxed) {
            Ok(_) => {
                // SAFETY: the state moved from `EMPTY` to `WRITING` here, once: this call
                // alone writes the slot, and no reader reads it before `READY`.
                unsafe { (*self.slot.get()).write(value) };
                // Release: the value's bytes before any reader that sees `READY`.
                self.state.store(READY, Ordering::Release);
                Ok(())
            }
            Err(READY) if plant == Plant::PublishOnceOverwrites => {
                // The planted bug: the value readers saw is replaced (and the old one leaked).
                // SAFETY: a test's only: it holds no borrow of the slot across a `set`.
                unsafe { (*self.slot.get()).write(value) };
                Ok(())
            }
            Err(_) => Err(value),
        }
    }

    /// The value, once it is set.
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn get(&self) -> Option<&T> {
        match self.state.load(Ordering::Acquire) {
            // SAFETY: `READY` was stored, with release, after the slot was written, and this
            // load acquired it; the slot is never written again.
            READY => Some(unsafe { (*self.slot.get()).assume_init_ref() }),
            _ => None,
        }
    }

    /// The value out, through the only reference.
    pub fn into_inner(mut self) -> Option<T> {
        let ready = *self.state.get_mut() == READY;
        *self.state.get_mut() = EMPTY;
        // SAFETY: `READY`: the slot holds a value, now moved out once; the state says `EMPTY`,
        // so the drop below does not drop it again.
        ready.then(|| unsafe { self.slot.get_mut().assume_init_read() })
    }
}

impl<T> Default for PublishOnce<T> {
    fn default() -> PublishOnce<T> {
        PublishOnce::new()
    }
}

impl<T> Drop for PublishOnce<T> {
    fn drop(&mut self) {
        if *self.state.get_mut() == READY {
            // SAFETY: `READY`: the slot holds the one value, dropped once, here.
            unsafe { self.slot.get_mut().assume_init_drop() };
        }
    }
}

impl fmt::Debug for Counter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Counter").field(&self.get()).finish()
    }
}

impl fmt::Debug for Flag {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Flag").field(&self.is_set()).finish()
    }
}

impl fmt::Debug for Seq {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Seq").field(&self.now()).finish()
    }
}

impl<T: fmt::Debug> fmt::Debug for PublishOnce<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("PublishOnce").field(&self.get()).finish()
    }
}
