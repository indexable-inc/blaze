//! The synchronization types of the crate's lock-free code: `std`'s atomics and `parking_lot`'s
//! locks, or under `--cfg blaze_check` the model checker's, whose locks have `parking_lot`'s API
//! (docs/v2/plan/lanes/safety/model.md, "The switch"). `arena`'s allocator counts stay `std`'s:
//! the checker allocates, and what it allocates goes through them.

#[cfg(not(blaze_check))]
#[allow(clippy::disallowed_types, reason = "named only by `pump::Lock`, which allows it")]
pub(crate) use parking_lot::{Condvar, Mutex, MutexGuard};
#[cfg(not(blaze_check))]
pub(crate) use std::{
    hint,
    sync::{OnceLock, atomic},
    thread, thread_local,
};

#[cfg(blaze_check)]
pub(crate) use blaze_check::{
    hint,
    sync::{
        OnceLock, atomic,
        lot::{Condvar, Mutex, MutexGuard},
    },
    thread, thread_local,
};
