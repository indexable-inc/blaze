//! The checker's own plain lock: a mutex for state kept outside a model (a test's log, a
//! model thread's result slot, the one-model-at-a-time lock), taken as a lock with no
//! poisoning. blaze-check sits below blaze-core, so blaze's named locks cannot be used here;
//! and a planted bug panics inside a model by design, so a lock it held must stay usable.

use std::sync::{MutexGuard, PoisonError};

/// A mutex whose `lock` ignores poisoning (see the module docs).
#[expect(
    clippy::disallowed_types,
    reason = "below blaze-core: blaze-check is a dependency of blaze-core, so its locks are std's"
)]
pub(crate) struct Mutex<T: ?Sized>(std::sync::Mutex<T>);

impl<T> Mutex<T> {
    #[expect(
        clippy::disallowed_types,
        reason = "below blaze-core: blaze-check is a dependency of blaze-core, so its locks are std's"
    )]
    pub(crate) const fn new(value: T) -> Mutex<T> {
        Mutex(std::sync::Mutex::new(value))
    }
}

impl<T: ?Sized> Mutex<T> {
    /// Takes the lock; one a panicking holder left is taken as it was left.
    #[expect(
        clippy::disallowed_types,
        reason = "below blaze-core: blaze-check is a dependency of blaze-core, so its locks are std's"
    )]
    pub(crate) fn lock(&self) -> MutexGuard<'_, T> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}
