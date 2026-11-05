//! The atomics and thread-local this crate's slots and epoch use: `std`'s, or under
//! `cfg(blaze_check)` the model checker's (docs/v2/plan/lanes/safety/model.md "The switch").

#[cfg(not(blaze_check))]
pub(crate) use std::{sync::atomic, thread_local};

#[cfg(blaze_check)]
pub(crate) use blaze_check::{sync::atomic, thread_local};
