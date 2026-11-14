//! The atomics the lock-free stacks, pools, counters and [`crate::swiss::Index`] use: `std`'s,
//! or under `cfg(blaze_check)` the model checker's (docs/v2/plan/design/model-checker.md "The
//! switch").

#[cfg(not(blaze_check))]
pub(crate) use std::sync::{OnceLock, atomic};

#[cfg(blaze_check)]
pub(crate) use blaze_check::sync::{OnceLock, atomic};
