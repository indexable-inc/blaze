//! The synchronization types of the crate's lock-free code: `std`'s atomics and
//! `blaze_core::lock`'s named lock, or under `--cfg blaze_check` the model checker's, behind the
//! same API (docs/v2/plan/design/model-checker.md, "The switch"). `arena`'s allocator counts stay
//! `std`'s: the checker allocates, and what it allocates goes through them.

#[cfg(not(blaze_check))]
#[allow(clippy::disallowed_types, reason = "named only by `pump::Lock`, which allows it")]
pub(crate) use blaze_core::lock::{Lock, LockGuard};
#[cfg(not(blaze_check))]
#[allow(
    clippy::disallowed_types,
    reason = "named only by the crate's process-wide singletons, each of which expects it"
)]
pub(crate) use std::sync::OnceLock;
#[cfg(not(blaze_check))]
pub(crate) use std::{hint, sync::atomic, thread, thread_local};

#[cfg(blaze_check)]
pub(crate) use blaze_check::{
    hint,
    sync::{OnceLock, atomic, lot::MutexGuard as LockGuard},
    thread, thread_local,
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

    pub(crate) fn into_inner(self) -> T {
        self.0.into_inner()
    }
}
