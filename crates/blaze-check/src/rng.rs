//! The seeded generator of the random runs: blaze's one SplitMix64, so a seed means the same here
//! as in every other crate's tests. `tests.rs` holds it to SplitMix64's reference outputs.

pub(crate) use blaze_rng::SplitMix64 as Rng;
