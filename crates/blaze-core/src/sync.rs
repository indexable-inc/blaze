//! The atomics of the crate's lock-free protocols ([`crate::handoff`]): the crate's own
//! ([`crate::atomic`]: `std`'s, and in the crate's tests the scheduled ones, which
//! `crate::world::threads::two_weak` orders for the protocols' theorems), or under
//! `--cfg blaze_check` the model checker's (docs/v2/plan/design/model-checker.md, "The switch").

#[cfg(not(blaze_check))]
pub(crate) use crate::atomic;

#[cfg(blaze_check)]
pub(crate) use blaze_check::sync::atomic;
