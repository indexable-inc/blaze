//! The synchronization types the tables, the store and the database use: `std`'s atomics and
//! `blaze_core::lock`'s named lock, or under `--cfg blaze_check` the model checker's, behind the
//! same API (docs/v2/plan/design/model-checker.md "The switch"). A normal build sees no checker.
//!
//! Also the tracked-bytes hooks: what the segments copy in, what readers read of it and what
//! a sweep frees. Under the model checker these are `blaze_check::cell`'s, so a read that the
//! store's atomics don't order after its bytes' copy, or before their segment's free, is a
//! finding. In a normal build they do nothing.

#[cfg(not(blaze_check))]
#[allow(clippy::disallowed_types, reason = "named only by `ShortLock`, which allows it")]
pub(crate) use blaze_core::lock::{Lock, LockGuard};
#[cfg(not(blaze_check))]
pub(crate) use std::{
    sync::{OnceLock, atomic},
    thread_local,
};

#[cfg(blaze_check)]
pub(crate) use blaze_check::{
    cell::{free as freed_bytes, read as read_bytes, write as wrote_bytes},
    sync::{OnceLock, atomic, lot::MutexGuard as LockGuard},
    thread_local,
};

/// `blaze_core::lock::Lock`'s API over the model checker's lock: the name is the real lock's,
/// which a model has no profile to record under.
#[cfg(blaze_check)]
pub(crate) struct Lock<T>(blaze_check::sync::lot::Mutex<T>);

#[cfg(blaze_check)]
impl<T> Lock<T> {
    pub(crate) const fn new(_name: &'static str, value: T) -> Lock<T> {
        Lock(blaze_check::sync::lot::Mutex::new(value))
    }

    pub(crate) fn lock(&self) -> LockGuard<'_, T> {
        self.0.lock()
    }

    pub(crate) fn get_mut(&mut self) -> &mut T {
        self.0.get_mut()
    }
}

/// `len` bytes at `at` were copied in (a no-op outside the model checker).
#[cfg(not(blaze_check))]
#[inline(always)]
pub(crate) fn wrote_bytes(_at: *const u8, _len: usize) {}

/// `len` bytes at `at` are read (a no-op outside the model checker).
#[cfg(not(blaze_check))]
#[inline(always)]
pub(crate) fn read_bytes(_at: *const u8, _len: usize) {}

/// `len` bytes at `at` are freed (a no-op outside the model checker).
#[cfg(not(blaze_check))]
#[inline(always)]
pub(crate) fn freed_bytes(_at: *const u8, _len: usize) {}
