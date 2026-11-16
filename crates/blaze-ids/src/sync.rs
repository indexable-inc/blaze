//! The synchronization types of [`crate::Sharded`] and the domain keys: std's, or the model
//! checker's in a model build (docs/v2/plan/design/model-checker.md, "The switch").
//!
//! The lock is a named one over `std::sync::Mutex`, not `blaze_core::lock`'s: this crate is at
//! the bottom of the tree and depends on no crate from outside the workspace
//! (docs/v2/plan/design/bottom.md, "Also decided on 2026-11-15"), and `blaze_core::lock` is
//! built on `parking_lot` with the rest of blaze-core above it. Its name is kept for when the
//! workspace's own lock is below this crate; until then a wait on it records no span (the shard
//! lock is held only to probe again, insert and claim a block, never across a call).

#[cfg(not(blaze_check))]
pub(crate) use std::sync::{OnceLock, atomic};

#[cfg(blaze_check)]
pub(crate) use blaze_check::sync::{OnceLock, atomic};

/// A named mutex, the API of `blaze_core::lock::Lock` that [`crate::Sharded`] uses.
#[cfg(not(blaze_check))]
#[expect(
    clippy::disallowed_types,
    reason = "the bottom of the tree has no named lock below it (see the module's documentation); held only to probe again, insert and claim a block, never across a call"
)]
pub(crate) struct Lock<T>(std::sync::Mutex<T>);

#[cfg(not(blaze_check))]
#[expect(
    clippy::disallowed_types,
    reason = "the bottom of the tree has no named lock below it (see the module's documentation)"
)]
impl<T> Lock<T> {
    /// A lock named `_name` (`"ids.sharded.claim"`): the name the profile will record its
    /// waits under once the workspace's own lock is below this crate.
    pub(crate) const fn new(_name: &'static str, value: T) -> Lock<T> {
        Lock(std::sync::Mutex::new(value))
    }

    /// The guard. A holder that panicked leaves the value as its last write left it, as
    /// `parking_lot`'s lock (what this replaces) does, which has no poisoning: the one lock
    /// here guards a claimed block of indices, each field written whole, and a panic under it
    /// (an index handed out twice, an interner past 2^32 values) leaves at worst an index
    /// never handed out, which `Sharded`'s indices already allow (dense but for gaps).
    pub(crate) fn lock(&self) -> std::sync::MutexGuard<'_, T> {
        self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

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
