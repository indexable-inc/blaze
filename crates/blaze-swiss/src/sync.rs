//! The bottom of the tree's two synchronization cells, on `core`'s atomics alone: [`Once`], a
//! value written once and read with one acquire load, and [`Spin`], a named lock held for a
//! few instructions. They are here, beside the probe, because [`crate::Index`] needs the first
//! and every crate below `blaze-core`'s named locks (`blaze-ids`, `blaze-hash`) needs both,
//! and this crate depends on nothing (docs/v2/plan/design/bottom.md, "Also decided on
//! 2026-11-15").
//!
//! Neither parks a thread: `core` has no way to, and each wait here is on a few instructions
//! of another thread (an initializer that hashes or allocates once, a holder that probes and
//! inserts). A waiter spins with [`core::hint::spin_loop`], and a wait of [`SPIN_BOUND`]
//! rounds is a hang, a panic naming the cell (rule bounded-progress): far past any honest
//! initializer or holder, and never reached by a thread that was only descheduled for a while.
//!
//! In a model build (`cfg(blaze_check)`) both are the model checker's, so a model explores
//! their interleavings (docs/v2/plan/design/model-checker.md, "The switch").

/// The atomics of the tables here: `core`'s, or the model checker's in a model build.
#[cfg(not(blaze_check))]
pub use core::sync::atomic;

#[cfg(blaze_check)]
pub use blaze_check::sync::atomic;

/// The model checker's once-cell in a model build: the API of [`Once`] it uses is the same.
#[cfg(blaze_check)]
pub use blaze_check::sync::OnceLock as Once;

/// Spin rounds after which a wait on a [`Once`] or a [`Spin`] is a hang: at 1 to 50 ns a
/// round, between a minute and two days, where every holder here is done in microseconds.
pub const SPIN_BOUND: u64 = 1 << 36;

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
        hint::spin_loop,
        mem::MaybeUninit,
        ops::{Deref, DerefMut},
        sync::atomic::{AtomicBool, AtomicU8, Ordering},
    };

    use super::SPIN_BOUND;

    /// Nothing written, nobody writing.
    const EMPTY: u8 = 0;
    /// One thread is writing the value.
    const WRITING: u8 = 1;
    /// The value is written and never changes again.
    const READY: u8 = 2;

    /// A value written at most once, then read by any thread with one acquire load: std's
    /// `OnceLock` on `core` alone. A thread that asks while another writes spins until it is
    /// done; an initializer that panics leaves the cell empty, and the next ask writes it.
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
            let mut rounds = 0_u64;
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
                    Err(_) => {
                        rounds += 1;
                        assert!(
                            rounds < SPIN_BOUND,
                            "a once-cell's initializer ran past {SPIN_BOUND} spin rounds: a hang"
                        );
                        spin_loop();
                    }
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
    /// flag. A waiter spins on a read of the flag, so it writes the holder's line only once the
    /// lock is free. A holder that panics releases it with its value as its last write left
    /// it: there is no poisoning, as in `parking_lot`'s lock.
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
            let mut rounds = 0_u64;
            // Acquire: the last holder's writes to the value, released by its unlock.
            while self.held.swap(true, Ordering::Acquire) {
                while self.held.load(Ordering::Relaxed) {
                    rounds += 1;
                    assert!(
                        rounds < SPIN_BOUND,
                        "the lock {} was held past {SPIN_BOUND} spin rounds: a hang",
                        self.name
                    );
                    spin_loop();
                }
            }
            SpinGuard { lock: self }
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
