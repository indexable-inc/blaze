//! The borrow checker's fixpoint and analyses as they were before they were redone (the tree of
//! 2026-10-29 21:00), the reference the new ones are held to. Each file is the old one verbatim
//! but for: `pub(super)` written as the `pub(crate)` it meant at the crate's root, the tests
//! left out, `Cfg` with its lifetime, and `bitset`'s free functions and `Transfer` today's
//! (unchanged since), so one sequence of effects composes either side.
//!
//! The analyses keep their old results but read today's [`Ctx`], [`regions`] and region
//! values, which both sides share: `MoveData::trackable` is `old_trackable` (an inherent method
//! of `Ctx` either way), `regions_of` and `opaque_captures` fill the old vectors, `Regions` and
//! `RegionValues` have their frame's lifetime, and the Polonius scopes take today's live loans.
//! `maybe_initialized_at_drops` and what it returns are visible to the comparison.

mod bitset;
mod dataflow;
mod intervals;
pub(super) mod liveness;
pub(super) mod loans;
pub(super) mod moves;
mod relation;

use crate::{Ctx, cfg, compares, qualifs, regions, return_slot};

pub(super) use self::{
    bitset::{BitMatrix, BitSet},
    dataflow::{GenKill, backward, forward},
    intervals::IntervalSet,
};
