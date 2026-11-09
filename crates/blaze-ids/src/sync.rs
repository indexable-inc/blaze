//! The synchronization types of [`crate::Sharded`] and the domain keys: std's, or the model
//! checker's in a model build (docs/v2/plan/design/model-checker.md, "The switch"). Outside one the
//! lock is `blaze_core::lock`'s named lock.

#[cfg(not(blaze_check))]
#[allow(clippy::disallowed_types, reason = "named only by `Shard::lock`, which allows it")]
pub(crate) use blaze_core::lock::Lock;
#[cfg(not(blaze_check))]
pub(crate) use std::sync::{OnceLock, atomic};

#[cfg(blaze_check)]
pub(crate) use blaze_check::sync::{OnceLock, atomic};

/// `blaze_core::lock::Lock`'s API over the model checker's lock: the name is the real lock's,
/// which a model has no profile to record under.
#[cfg(blaze_check)]
pub(crate) struct Lock<T>(blaze_check::sync::lot::Mutex<T>);

#[cfg(blaze_check)]
impl<T> Lock<T> {
    pub(crate) const fn new(_name: &'static str, value: T) -> Lock<T> {
        Lock(blaze_check::sync::lot::Mutex::new(value))
    }

    pub(crate) fn lock(&self) -> blaze_check::sync::lot::MutexGuard<'_, T> {
        self.0.lock()
    }
}
