//! The atomics the allocator uses: `core`'s, or under `cfg(blaze_check)` the model checker's
//! (docs/v2/plan/design/model-checker.md "The switch"); and the tracking of a block's link word,
//! which a model checks for races.

#[cfg(not(blaze_check))]
pub(crate) use core::sync::atomic;

#[cfg(blaze_check)]
pub(crate) use blaze_check::sync::atomic;

/// A non-atomic write of the value at `at`: a model checks every earlier access of it happens
/// before. Nothing outside a model.
#[inline(always)]
pub(crate) fn wrote<T>(at: *const T) {
    #[cfg(blaze_check)]
    if size_of::<T>() != 0 {
        blaze_check::cell::write(at.cast(), size_of::<T>());
    }
    _ = at;
}

/// A non-atomic read of the value at `at`: a model checks its last write happens before.
#[inline(always)]
pub(crate) fn read<T>(at: *const T) {
    #[cfg(blaze_check)]
    if size_of::<T>() != 0 {
        blaze_check::cell::read(at.cast(), size_of::<T>());
    }
    _ = at;
}

/// A short lock for the two caches (segments, large mappings): held for a few loads and stores
/// of a fixed array, never across a system call, so a waiter spins ([`blaze_spin::SpinWait`],
/// bounded) rather than parks: parking would need the kernel's wait queue, and `std`'s mutex
/// may allocate its waiter state on some systems, which an allocator's callee must not.
pub(crate) struct Spin {
    held: atomic::AtomicBool,
}

impl Spin {
    pub(crate) const fn new() -> Spin {
        Spin { held: atomic::AtomicBool::new(false) }
    }

    /// Runs `body` holding the lock.
    pub(crate) fn with<R>(&self, body: impl FnOnce() -> R) -> R {
        use atomic::Ordering::{Acquire, Relaxed, Release};
        blaze_spin::SpinWait::until("blaze-malloc's cache lock, held for a few stores", || {
            self.held.compare_exchange_weak(false, true, Acquire, Relaxed).is_ok()
        });
        let got = body();
        self.held.store(false, Release);
        got
    }
}
