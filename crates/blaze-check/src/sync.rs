//! `std::sync`: the atomics, locks, condition variables and once cells as model objects, and
//! `std`'s own types where a model needs nothing more (`Arc` and `Weak` are reference counts
//! whose orderings `std` gets right; the checker looks at what they point to).

pub mod atomic;
mod lock;
mod once;

pub use std::sync::{Arc, LockResult, PoisonError, TryLockError, TryLockResult, Weak};

pub use lock::{
    Condvar, Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard, WaitTimeoutResult, lot,
};
pub use once::OnceLock;
