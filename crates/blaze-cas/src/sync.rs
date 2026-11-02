//! The synchronization types this crate's tables, segments and flags use: `std`'s, or under
//! `cfg(blaze_check)` the model checker's (docs/v2/plan/lanes/safety/model.md "The switch"),
//! and [`track`], a segment chunk's raw bytes as a model sees them.

#[cfg(not(blaze_check))]
pub(crate) use std::sync::{OnceLock, atomic};

#[cfg(blaze_check)]
pub(crate) use blaze_check::sync::{OnceLock, atomic};

/// Tracked reads, writes and frees of raw bytes (a segment chunk's), exact to the byte: in a
/// model, a read not ordered after the write of its bytes, a write or free not ordered after
/// another thread's read, and a read of freed bytes are findings.
#[cfg(blaze_check)]
pub(crate) use blaze_check::cell as track;

/// Outside a model, nothing.
#[cfg(not(blaze_check))]
pub(crate) mod track {
    #[inline(always)]
    pub(crate) fn read(_ptr: *const u8, _len: usize) {}

    #[inline(always)]
    pub(crate) fn write(_ptr: *const u8, _len: usize) {}

    #[inline(always)]
    pub(crate) fn free(_ptr: *const u8, _len: usize) {}
}
