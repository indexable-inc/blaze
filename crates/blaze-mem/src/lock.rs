//! A lock that never allocates, for tables made fresh and locked a few times each (a solver
//! cache's shards and writers): `std`'s mutex on macOS boxes a pthread mutex on its first lock,
//! which a table made per interner pays once per lock it has.
//!
//! It spins a little, then yields ([`blaze_spin::SpinWait::until`], which panics naming it once
//! a wait runs past any section that short), so it is for sections a few instructions long: a
//! holder that is preempted makes waiters give up their time slice, not burn it. It does not poison:
//! a holder that panics releases it, and what it guarded is as the panic left it.

use std::{
    cell::UnsafeCell,
    fmt,
    ops::{Deref, DerefMut},
};

use blaze_spin::SpinWait;

use crate::sync::atomic::{AtomicBool, Ordering};

pub struct SpinLock<T> {
    held: AtomicBool,
    value: UnsafeCell<T>,
}

// SAFETY: `value` is reached only through a `SpinGuard`, and at most one exists at a time
// (`lock` sets `held` with an acquire compare-and-swap, the guard's drop clears it with a
// release store), so the lock hands `T` from thread to thread like a mutex: `T: Send` is enough.
unsafe impl<T: Send> Sync for SpinLock<T> {}
// SAFETY: moving the lock moves the `T` it owns.
unsafe impl<T: Send> Send for SpinLock<T> {}

impl<T> SpinLock<T> {
    pub const fn new(value: T) -> SpinLock<T> {
        SpinLock { held: AtomicBool::new(false), value: UnsafeCell::new(value) }
    }

    #[inline]
    pub fn lock(&self) -> SpinGuard<'_, T> {
        if self.held.compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed).is_err()
        {
            self.wait();
        }
        SpinGuard { lock: self }
    }

    #[cold]
    fn wait(&self) {
        SpinWait::until("a `SpinLock`, held for a few instructions", || {
            !self.held.load(Ordering::Relaxed)
                && self.held.compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed).is_ok()
        });
    }

    pub fn get_mut(&mut self) -> &mut T {
        self.value.get_mut()
    }

    pub fn into_inner(self) -> T {
        self.value.into_inner()
    }
}

impl<T: Default> Default for SpinLock<T> {
    fn default() -> SpinLock<T> {
        SpinLock::new(T::default())
    }
}

impl<T> fmt::Debug for SpinLock<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SpinLock").field("held", &self.held.load(Ordering::Relaxed)).finish()
    }
}

/// The lock, held until this drops.
pub struct SpinGuard<'a, T> {
    lock: &'a SpinLock<T>,
}

impl<T> Deref for SpinGuard<'_, T> {
    type Target = T;

    #[inline]
    fn deref(&self) -> &T {
        // SAFETY: this guard is the one that exists (see the `Sync` impl), so nothing else
        // reaches `value` while it lives.
        unsafe { &*self.lock.value.get() }
    }
}

impl<T> DerefMut for SpinGuard<'_, T> {
    #[inline]
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: as for `deref`, and `&mut self` makes this the guard's one live borrow.
        unsafe { &mut *self.lock.value.get() }
    }
}

impl<T> Drop for SpinGuard<'_, T> {
    #[inline]
    fn drop(&mut self) {
        self.lock.held.store(false, Ordering::Release);
    }
}

#[cfg(all(test, not(blaze_check)))]
mod tests {
    use std::sync::Barrier;

    use super::SpinLock;

    /// A plain read-modify-write under the lock from many threads loses no update.
    #[test]
    fn threads_under_the_lock_lose_no_update() {
        let lock = SpinLock::new(0u64);
        let threads = 8;
        let barrier = Barrier::new(threads);
        std::thread::scope(|scope| {
            for _ in 0..threads {
                let (lock, barrier) = (&lock, &barrier);
                scope.spawn(move || {
                    barrier.wait();
                    for _ in 0..20_000 {
                        let mut value = lock.lock();
                        let read = *value;
                        std::hint::black_box(&read);
                        *value = read + 1;
                    }
                });
            }
        });
        assert_eq!(lock.into_inner(), threads as u64 * 20_000);
    }

    #[test]
    fn a_panicking_holder_releases_the_lock() {
        let lock = SpinLock::new(1u32);
        let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut value = lock.lock();
            *value = 2;
            panic!("while holding the lock");
        }));
        assert!(caught.is_err());
        assert_eq!(*lock.lock(), 2);
    }

    #[test]
    fn a_new_lock_and_its_first_locks_allocate_nothing() {
        blaze_alloc_count::assert_at_most(0, "a fresh spin lock locked", || {
            let lock = SpinLock::new([0u32; 4]);
            for at in 0..4 {
                lock.lock()[at] += 1;
            }
            std::hint::black_box(&lock);
        });
    }
}
