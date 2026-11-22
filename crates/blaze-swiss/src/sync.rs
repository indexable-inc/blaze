//! The bottom of the tree's two synchronization cells, on `core`'s atomics alone: [`Once`], a
//! value written once and read with one acquire load, and [`Spin`], a named lock held for a
//! few instructions. They are here, beside the probe, because [`crate::Index`] needs the first
//! and every crate below `blaze-core`'s named locks (`blaze-ids`, `blaze-hash`) needs both,
//! and this crate depends on nothing (docs/v2/plan/design/bottom.md, "Also decided on
//! 2026-11-15").
//!
//! Neither parks a thread: `core` has no way to, and each wait here is on a few instructions
//! of another thread (an initializer that hashes or allocates once, a holder that probes and
//! inserts). A waiter backs off ([`Backoff`]): [`core::hint::spin_loop`] in rounds that double
//! up to [`BackoffLimits::spin_steps`], so 128 waiters on one lock do not all write its line the
//! instant it is released, then rounds of the longest spin. A wait past its bound is a hang, a
//! [`Hang`] naming the cell (rule bounded-progress): [`BackoffLimits::most_rounds`] rounds.
//!
//! The bounds are a value, [`BackoffLimits`], with the shipped one [`BackoffLimits::SHIPPED`]
//! (D165: a simulation draws it down to reach the hang). Nothing here is process state: the
//! process-wide waiting hook (`install`, `Waiting`, `Clock`, a `static` pointer read by every
//! backoff, with a clock read through a function pointer) was removed on 2026-11-20 (D170 C3);
//! nothing in the tree installed it, so every wait already took the spinning path it keeps.
//!
//! In a model build (`cfg(blaze_check)`) both cells are the model checker's, and a backoff
//! round is its `spin_loop`, a yield to every other thread, so a model explores their
//! interleavings (docs/v2/plan/design/model-checker.md, "The switch").

use core::fmt;

/// The atomics of the tables here: `core`'s, or the model checker's in a model build.
#[cfg(not(blaze_check))]
pub use core::sync::atomic;

#[cfg(blaze_check)]
pub use blaze_check::sync::atomic;

/// The model checker's once-cell in a model build: the API of [`Once`] it uses is the same.
#[cfg(blaze_check)]
pub use blaze_check::sync::OnceLock as Once;

/// Backoff rounds after which a wait on a [`Once`] or a [`Spin`] is a hang: the shipped
/// [`BackoffLimits::most_rounds`]. Sized for holders done in microseconds (a probe and an
/// insert, a hash or one allocation; never an O(n) copy, which `blaze-ids` keeps outside its
/// lock) with up to 128 waiters on one cell: a round past the first [`SPIN_STEPS`] is
/// `1 << SPIN_STEPS` spins, about 0.1 µs, so the bound is about two minutes of waiting, where a
/// waiter behind 127 others each holding 10 µs waits about a millisecond.
pub const SPIN_BOUND: u64 = 1 << 30;

/// Doublings of a waiter's spin: rounds of 1, 2, 4 .. 64 spins, then rounds of 64. The shipped
/// [`BackoffLimits::spin_steps`].
pub const SPIN_STEPS: u32 = 6;

/// A backoff's bounds as one value (D165, "a limit is a value a simulation draws"): the cells
/// here take [`BackoffLimits::SHIPPED`]; a simulation or a test makes a [`Backoff`] with
/// [`Backoff::limited`] and draws each field down to reach the [`Hang`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct BackoffLimits {
    /// Doublings of the spin before the rounds stop growing (at most 31): rounds of
    /// `1 << (round - 1)` spins up to `1 << spin_steps`.
    pub spin_steps: u32,
    /// Rounds after which the wait is a [`Hang`]: at least 1.
    pub most_rounds: u64,
}

impl BackoffLimits {
    /// The values shipped: [`SPIN_STEPS`] and [`SPIN_BOUND`].
    pub const SHIPPED: BackoffLimits =
        BackoffLimits { spin_steps: SPIN_STEPS, most_rounds: SPIN_BOUND };
}

/// A wait that went past its bound: the cell it was on and how long it waited.
#[derive(Clone, Copy, Debug)]
pub struct Hang {
    /// The cell's name (`"ids.sharded.claim"`).
    pub cell: &'static str,
    pub waited: Waited,
}

/// How long a [`Hang`] waited, in what its bound counts: backoff rounds (the installed clock's
/// ticks went with the waiting hook, 2026-11-20).
#[derive(Clone, Copy, Debug)]
pub enum Waited {
    Rounds(u64),
}

impl fmt::Display for Hang {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.waited {
            Waited::Rounds(rounds) => write!(
                f,
                "the wait on {} went past {rounds} backoff rounds, sized for holders of \
                 microseconds and 128 waiters: a hang",
                self.cell
            ),
        }
    }
}

/// One waiter's backoff on one cell: call [`Backoff::snooze`] once per failed look.
pub struct Backoff {
    cell: &'static str,
    rounds: u64,
    limits: BackoffLimits,
}

impl Backoff {
    /// A backoff on `cell` with the shipped bounds ([`BackoffLimits::SHIPPED`]).
    pub const fn new(cell: &'static str) -> Backoff {
        Backoff::limited(cell, BackoffLimits::SHIPPED)
    }

    /// A backoff on `cell` with the bounds `limits`: a simulation's drawn ones.
    ///
    /// # Panics
    ///
    /// When `limits` is outside its fields' ranges (`spin_steps` past 31, `most_rounds` 0).
    pub const fn limited(cell: &'static str, limits: BackoffLimits) -> Backoff {
        assert!(limits.spin_steps <= 31, "a backoff's spin_steps past 31 overflows a round");
        assert!(limits.most_rounds >= 1, "a backoff of no rounds");
        Backoff { cell, rounds: 0, limits }
    }

    /// Waits one round: `1 << (round - 1)` spins for the first `spin_steps` rounds, then
    /// `1 << spin_steps` spins a round. Panics with a [`Hang`] past `most_rounds`.
    #[inline]
    pub fn snooze(&mut self) {
        self.rounds += 1;
        if self.rounds <= u64::from(self.limits.spin_steps) && self.rounds < self.limits.most_rounds
        {
            for _ in 0..1_u32 << (self.rounds - 1) {
                spin_loop();
            }
            return;
        }
        self.long_round();
    }

    #[cold]
    fn long_round(&mut self) {
        if self.rounds >= self.limits.most_rounds {
            panic!("{}", Hang { cell: self.cell, waited: Waited::Rounds(self.rounds) });
        }
        // A model build's spin is already a yield to every other thread.
        for _ in 0..1_u32 << self.limits.spin_steps {
            spin_loop();
        }
    }
}

/// `core`'s spin hint, or the model checker's, which yields to every other thread.
#[inline]
fn spin_loop() {
    #[cfg(not(blaze_check))]
    core::hint::spin_loop();
    #[cfg(blaze_check)]
    blaze_check::hint::spin_loop();
}

#[cfg(not(blaze_check))]
pub use own::{Once, Spin, SpinGuard};

#[cfg(blaze_check)]
pub use model::{Spin, SpinGuard};

#[cfg(not(blaze_check))]
#[allow(
    unsafe_code,
    reason = "a cell written by one thread and read by others: each `unsafe` says what the state word orders"
)]
mod own {
    use core::{
        cell::UnsafeCell,
        fmt,
        mem::MaybeUninit,
        ops::{Deref, DerefMut},
        sync::atomic::{AtomicBool, AtomicU8, Ordering},
    };

    use super::Backoff;

    /// Nothing written, nobody writing.
    const EMPTY: u8 = 0;
    /// One thread is writing the value.
    const WRITING: u8 = 1;
    /// The value is written and never changes again.
    const READY: u8 = 2;

    /// A value written at most once, then read by any thread with one acquire load: std's
    /// `OnceLock` on `core` alone. A thread that asks while another writes backs off until it
    /// is done; an initializer that panics leaves the cell empty, and the next ask writes it.
    pub struct Once<T> {
        state: AtomicU8,
        value: UnsafeCell<MaybeUninit<T>>,
    }

    // SAFETY: a `&Once<T>` hands out `&T` to every thread (so `T: Sync`) and moves a `T` made
    // on one thread into the cell that another may drop (so `T: Send`), as std's `OnceLock`.
    unsafe impl<T: Send + Sync> Sync for Once<T> {}
    // SAFETY: moving the cell moves its `T`.
    unsafe impl<T: Send> Send for Once<T> {}

    impl<T> Default for Once<T> {
        fn default() -> Once<T> {
            Once::new()
        }
    }

    impl<T: fmt::Debug> fmt::Debug for Once<T> {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.debug_tuple("Once").field(&self.get()).finish()
        }
    }

    /// The right to write a [`Once`]'s value, held by the one thread that moved it from
    /// `EMPTY` to `WRITING`. Dropped unfilled (its writer panicked), it puts the cell back to
    /// `EMPTY`.
    struct Claim<'a, T> {
        cell: &'a Once<T>,
    }

    impl<'a, T> Claim<'a, T> {
        fn fill(self, value: T) -> &'a T {
            let cell = self.cell;
            core::mem::forget(self);
            // SAFETY: this thread moved the state to `WRITING`, so no other writes the value
            // and none reads it until the `READY` store below.
            unsafe { (*cell.value.get()).write(value) };
            // Release: a reader that loads `READY` sees the value written.
            cell.state.store(READY, Ordering::Release);
            // SAFETY: just written, and never written again.
            unsafe { (*cell.value.get()).assume_init_ref() }
        }
    }

    impl<T> Drop for Claim<'_, T> {
        fn drop(&mut self) {
            // Release: nothing was written; the next claimant starts from an empty cell.
            self.cell.state.store(EMPTY, Ordering::Release);
        }
    }

    impl<T> Once<T> {
        pub const fn new() -> Once<T> {
            Once { state: AtomicU8::new(EMPTY), value: UnsafeCell::new(MaybeUninit::uninit()) }
        }

        /// The value, if one was written.
        #[inline]
        pub fn get(&self) -> Option<&T> {
            // Acquire: pairs with the writer's `READY` release store.
            if self.state.load(Ordering::Acquire) == READY {
                // SAFETY: `READY` is stored only after the value is written, and the value is
                // never written again nor dropped while `&self` lives.
                Some(unsafe { (*self.value.get()).assume_init_ref() })
            } else {
                None
            }
        }

        /// The value, written by `make` now if none was: `make` runs on at most one thread at
        /// a time, and on none once a value is written.
        #[inline]
        pub fn get_or_init(&self, make: impl FnOnce() -> T) -> &T {
            if let Some(value) = self.get() {
                return value;
            }
            match self.claim() {
                Some(claim) => claim.fill(make()),
                None => self.get().expect("a claim refused only once the value is written"),
            }
        }

        /// Writes `value` if none was written; else gives it back.
        pub fn set(&self, value: T) -> Result<(), T> {
            match self.claim() {
                Some(claim) => {
                    claim.fill(value);
                    Ok(())
                }
                None => Err(value),
            }
        }

        /// The right to write the value, or `None` once another thread has written it: waits
        /// out a thread that is writing it now.
        fn claim(&self) -> Option<Claim<'_, T>> {
            let mut backoff = Backoff::new("a once-cell's initializer");
            loop {
                // Acquire on success: a claim after a panicked one sees its `EMPTY`; on
                // failure, a `READY` seen here orders the value before our read of it.
                match self.state.compare_exchange_weak(
                    EMPTY,
                    WRITING,
                    Ordering::Acquire,
                    Ordering::Acquire,
                ) {
                    Ok(_) => return Some(Claim { cell: self }),
                    Err(READY) => return None,
                    Err(_) => backoff.snooze(),
                }
            }
        }
    }

    impl<T> Drop for Once<T> {
        fn drop(&mut self) {
            if *self.state.get_mut() == READY {
                // SAFETY: `READY` with `&mut self`: the value is written and nobody reads it.
                unsafe { self.value.get_mut().assume_init_drop() };
            }
        }
    }

    /// A lock named for what it guards (`"ids.sharded.claim"`), held for a few instructions:
    /// the API of `blaze_core::lock::Lock` that the bottom of the tree uses, on one atomic
    /// flag. A waiter reads the flag between [`Backoff`] rounds, so it writes the holder's line
    /// only once the lock is free, and waiters that missed one release try again at spread-out
    /// times. Unfair: a waiter may be passed by later ones, so the bound, not an order, says
    /// when a wait is a hang. A holder that panics releases it with its value as its last write
    /// left it: there is no poisoning, as in `parking_lot`'s lock.
    pub struct Spin<T> {
        name: &'static str,
        held: AtomicBool,
        value: UnsafeCell<T>,
    }

    // SAFETY: the flag lets one thread at a time reach the value, which moves between threads
    // with the guard's hand-over, as a mutex's (`T: Send`).
    unsafe impl<T: Send> Sync for Spin<T> {}
    // SAFETY: moving the lock moves its `T`.
    unsafe impl<T: Send> Send for Spin<T> {}

    impl<T> Spin<T> {
        pub const fn new(name: &'static str, value: T) -> Spin<T> {
            Spin { name, held: AtomicBool::new(false), value: UnsafeCell::new(value) }
        }

        /// The guard, once no other thread holds one.
        #[inline]
        pub fn lock(&self) -> SpinGuard<'_, T> {
            // Acquire: the last holder's writes to the value, released by its unlock.
            if !self.held.swap(true, Ordering::Acquire) {
                return SpinGuard { lock: self };
            }
            self.lock_contended()
        }

        #[cold]
        fn lock_contended(&self) -> SpinGuard<'_, T> {
            let mut backoff = Backoff::new(self.name);
            loop {
                while self.held.load(Ordering::Relaxed) {
                    backoff.snooze();
                }
                // Acquire: as in `lock`.
                if !self.held.swap(true, Ordering::Acquire) {
                    return SpinGuard { lock: self };
                }
                backoff.snooze();
            }
        }
    }

    /// The holder's access to a [`Spin`]'s value; dropping it unlocks.
    pub struct SpinGuard<'a, T> {
        lock: &'a Spin<T>,
    }

    impl<T> Deref for SpinGuard<'_, T> {
        type Target = T;

        fn deref(&self) -> &T {
            // SAFETY: the guard exists only while this thread holds the flag.
            unsafe { &*self.lock.value.get() }
        }
    }

    impl<T> DerefMut for SpinGuard<'_, T> {
        fn deref_mut(&mut self) -> &mut T {
            // SAFETY: the guard exists only while this thread holds the flag, and `&mut self`
            // makes this the one reference through it.
            unsafe { &mut *self.lock.value.get() }
        }
    }

    impl<T> Drop for SpinGuard<'_, T> {
        fn drop(&mut self) {
            // Release: the next holder sees this one's writes.
            self.lock.held.store(false, Ordering::Release);
        }
    }
}

/// [`Spin`]'s API over the model checker's lock: the name is the real lock's, which a model has
/// no profile to record under.
#[cfg(blaze_check)]
mod model {
    pub struct Spin<T>(blaze_check::sync::lot::Mutex<T>);

    pub type SpinGuard<'a, T> = blaze_check::sync::lot::MutexGuard<'a, T>;

    impl<T> Spin<T> {
        pub const fn new(_name: &'static str, value: T) -> Spin<T> {
            Spin(blaze_check::sync::lot::Mutex::new(value))
        }

        pub fn lock(&self) -> SpinGuard<'_, T> {
            self.0.lock()
        }
    }
}

/// The cells' `unsafe` under real threads, sized for Miri (`cargo miri test -p blaze-swiss
/// sync`): each test reaches the blocks its name says.
#[cfg(all(test, not(blaze_check)))]
mod tests {
    use std::{
        sync::atomic::{AtomicUsize, Ordering::Relaxed},
        thread,
        vec::Vec,
    };

    use super::*;

    const THREADS: usize = if cfg!(miri) { 3 } else { 8 };
    const ROUNDS: usize = if cfg!(miri) { 50 } else { 10_000 };

    /// A value that counts its drops.
    struct Counted<'a>(&'a AtomicUsize);

    impl Drop for Counted<'_> {
        fn drop(&mut self) {
            self.0.fetch_add(1, Relaxed);
        }
    }

    /// `Claim::fill`'s write and `get`'s read across threads, `set` refused after, and the
    /// cell's `Drop` dropping the one value written.
    #[test]
    fn once_is_written_by_one_thread_and_dropped_once() {
        let (made, dropped) = (AtomicUsize::new(0), AtomicUsize::new(0));
        let cell = Once::new();
        thread::scope(|scope| {
            for _ in 0..THREADS {
                scope.spawn(|| {
                    let value = cell.get_or_init(|| {
                        made.fetch_add(1, Relaxed);
                        Counted(&dropped)
                    });
                    assert!(core::ptr::eq(value.0, &dropped));
                });
            }
        });
        assert_eq!(made.load(Relaxed), 1);
        assert!(cell.set(Counted(&dropped)).is_err());
        assert_eq!(dropped.load(Relaxed), 1, "the refused value dropped");
        drop(cell);
        assert_eq!(dropped.load(Relaxed), 2, "the written value dropped with the cell");
    }

    /// `Claim`'s `Drop` on a panicking initializer leaves the cell empty, and the next ask
    /// writes it; an empty cell's `Drop` drops nothing.
    #[test]
    fn a_panicking_initializer_leaves_the_cell_empty() {
        let cell: Once<Vec<u8>> = Once::new();
        let caught = std::panic::catch_unwind(core::panic::AssertUnwindSafe(|| {
            cell.get_or_init(|| panic!("the initializer fails"));
        }));
        assert!(caught.is_err());
        assert!(cell.get().is_none());
        assert_eq!(cell.get_or_init(|| std::vec![7]).as_slice(), [7]);
        drop(Once::<Vec<u8>>::new());
    }

    /// `Spin`'s guard (`Deref`, `DerefMut`, its release) under contention, through the backoff:
    /// every increment is counted exactly.
    #[test]
    fn a_spin_lock_counts_every_claim() {
        let lock = Spin::new("swiss.test.count", 0usize);
        thread::scope(|scope| {
            for _ in 0..THREADS {
                scope.spawn(|| {
                    for _ in 0..ROUNDS {
                        *lock.lock() += 1;
                    }
                });
            }
        });
        assert_eq!(*lock.lock(), THREADS * ROUNDS);
    }

    /// A backoff's bound is its limits' value: at each drawn `most_rounds` (the smallest legal
    /// ones and past the spin steps), the round that reaches it is a [`Hang`] naming the cell,
    /// and every round before it returns.
    #[test]
    fn a_backoff_hangs_at_its_drawn_bound() {
        for spin_steps in [0, 1, 3] {
            for most_rounds in [1, 2, 3, 9] {
                let limits = BackoffLimits { spin_steps, most_rounds };
                let mut backoff = Backoff::limited("swiss.test.bound", limits);
                for _ in 1..most_rounds {
                    backoff.snooze();
                }
                let hang =
                    std::panic::catch_unwind(core::panic::AssertUnwindSafe(|| backoff.snooze()));
                let message = hang.expect_err("the round at the bound: a hang");
                let message = message.downcast_ref::<std::string::String>().expect("a message");
                assert!(message.contains("swiss.test.bound"), "{message}");
                assert!(message.contains(&std::format!("past {most_rounds} backoff")), "{message}");
            }
        }
    }
}
