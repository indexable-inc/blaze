//! The synchronization types of the store's lock-free code: std's, or the model checker's in a
//! model build (docs/v2/plan/lanes/safety/model.md), where nothing else changes. Outside one the
//! lock is `parking_lot`'s.

#[cfg(not(blaze_check))]
pub(crate) use parking_lot::Mutex;
#[cfg(not(blaze_check))]
pub(crate) use std::sync::{OnceLock, atomic};

#[cfg(blaze_check)]
pub(crate) use blaze_check::{
    sync::{OnceLock, atomic, lot::Mutex},
    thread,
};
