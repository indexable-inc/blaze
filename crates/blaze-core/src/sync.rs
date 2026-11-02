//! The atomics of the crate's lock-free protocols ([`crate::handoff`]): `std`'s, or under
//! `--cfg blaze_check` the model checker's (docs/v2/plan/lanes/safety/model.md, "The switch").
//! Everything else in the crate uses `std`'s directly: counters, flags and the kernel's ring
//! words, which no model runs.

#[cfg(not(blaze_check))]
pub(crate) use std::sync::atomic;

#[cfg(blaze_check)]
pub(crate) use blaze_check::sync::atomic;
