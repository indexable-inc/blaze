//! The composer (docs/v2/plan/design/backend-next.md D97 point 2, D99, D100): the Fast tier,
//! which makes a function's code by copying templates from the template table and filling their
//! fields. Two passes: the analysis ([`analysis`]: order, loops, liveness, homes) and the
//! emission (emit.rs, with the `Fast` tier in fast.rs: piece K2, whose `mod` lines are its own
//! wiring).

pub(crate) mod analysis;
pub(crate) mod emit;
mod fast;
pub(crate) mod loop_map;
pub(crate) mod reserve;

pub use fast::Fast;

#[cfg(test)]
mod analysis_tests;
#[cfg(test)]
mod fast_tests;
#[cfg(test)]
mod loop_map_tests;
