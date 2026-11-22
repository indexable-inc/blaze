//! The atomics the lock-free stacks, pools and counters use: `std`'s, or under
//! `cfg(blaze_check)` the model checker's (docs/v2/plan/design/model-checker.md "The switch").

#[cfg(not(blaze_check))]
pub(crate) use core::sync::atomic;

#[cfg(blaze_check)]
pub(crate) use blaze_check::sync::atomic;
