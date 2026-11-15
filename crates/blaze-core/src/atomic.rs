//! The atomics a core imports: `use blaze_core::atomic::{AtomicU32, Ordering}`, never std's
//! directly, so that its own tests can order them.
//!
//! Outside a test these are std's, and nothing more. In blaze-core's tests each is
//! [`crate::world::threads::scheduled`]'s: every operation is first a point where
//! [`crate::world::threads::two`] decides which thread goes, so a test runs a schedule of two
//! threads exactly. A proof reads the non-test build, where the translator models std's
//! atomics as cells of the world threads share (`lean/Blaze/Atomic.lean`).

#[cfg(not(test))]
pub use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize};

#[cfg(test)]
pub use crate::world::threads::scheduled::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize};

pub use std::sync::atomic::Ordering;
