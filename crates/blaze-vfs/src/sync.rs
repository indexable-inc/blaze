//! The synchronization types of the store's lock-free code: std's, or the model checker's in a
//! model build (docs/v2/plan/design/model-checker.md), where nothing else changes. Outside one the
//! lock is blaze's named one (`blaze_core::lock::Lock`); in one, [`Lock`] is the model's mutex
//! with the same constructor, the name dropped.

#[cfg(not(blaze_check))]
pub(crate) use blaze_core::lock::Lock;
#[cfg(not(blaze_check))]
pub(crate) use std::sync::{OnceLock, atomic};

#[cfg(blaze_check)]
pub(crate) use blaze_check::{
    sync::{OnceLock, atomic},
    thread,
};

/// The model's mutex behind `blaze_core::lock::Lock`'s constructor: a model build checks the
/// interleavings, so the name, which only the profiler reads, is dropped.
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
