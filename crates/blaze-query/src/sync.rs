//! The synchronization types the tables, the store and the database use: `std`'s atomics and
//! `parking_lot`'s locks, or under `--cfg blaze_check` the model checker's, which have the same
//! API (docs/v2/plan/lanes/safety/model.md "The switch"). A normal build sees no checker.
//!
//! Also the tracked-bytes hooks: what the segments copy in, what readers read of it and what
//! a sweep frees. Under the model checker these are `blaze_check::cell`'s, so a read that the
//! store's atomics don't order after its bytes' copy, or before their segment's free, is a
//! finding. In a normal build they do nothing.

#[cfg(not(blaze_check))]
#[allow(clippy::disallowed_types)]
pub(crate) use parking_lot::{Mutex, MutexGuard};
#[cfg(not(blaze_check))]
pub(crate) use std::{
    sync::{OnceLock, atomic},
    thread_local,
};

#[cfg(blaze_check)]
pub(crate) use blaze_check::{
    cell::{free as freed_bytes, read as read_bytes, write as wrote_bytes},
    sync::{
        OnceLock, atomic,
        lot::{Mutex, MutexGuard},
    },
    thread_local,
};

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
