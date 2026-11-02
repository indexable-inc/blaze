//! blaze's model checker for lock-free code (docs/v2/plan/lanes/safety/model.md).
//!
//! [`model`] runs a closure under every interleaving of the threads it spawns, to a preemption
//! bound (loom's bounded DPOR), and under every value each atomic load may read under C11. Its
//! threads are coroutines on the calling OS thread, switched at each atomic operation, lock,
//! spawn, join, park and yield. The shims ([`sync`], [`thread`], [`cell`], [`hint`],
//! [`thread_local!`]) have `std`'s API, so a crate switches to them with one `cfg(blaze_check)`
//! module; outside a model they do what `std`'s do. A finding (a panic in any thread, a data race
//! on an [`cell::UnsafeCell`], a deadlock, an execution that never ends) stops the model with
//! the seed and the schedule, which `CHECK_REPLAY` runs again.

#![deny(unsafe_code)]

pub mod cell;
mod clock;
#[allow(unsafe_code)]
mod coro;
mod explore;
pub mod hint;
pub mod linear;
mod path;
mod rng;
mod rt;
pub mod sync;
pub mod thread;

#[cfg(test)]
mod tests;

pub use clock::MAX_THREADS;
pub use explore::{Builder, model};
