//! The derived impls of [`Acyclic`] and [`NoCapability`], each bounded by the walk's answer
//! for the instance (`where const`, `blaze_const_exprs`). Compiled by blaze only (`cfg(blaze)`,
//! std.md, "Two builders"): rustc cannot parse a `where const` bound, and its plain impls are
//! in `own.rs`.

use super::{AnyCapability, NoCapability, reach::{Acyclic, Cycle, none}};

// SAFETY: the walk for `Cycle` cleared `T`: from a `T`, through everything it owns, no
// counted pointer that can be set after the value exists leads back to `T`, and nothing on the
// way was a type the walk could not see into. A value owns itself only through such a pointer
// (reach.rs, `Cycle`).
unsafe impl<T: ?Sized> Acyclic for T where const { none::<T, Cycle>() } {}

// SAFETY: the walk for `AnyCapability` found no type that implements `Capability` in, owned
// by or borrowed by a `T`, and saw into every type on the way.
unsafe impl<T: ?Sized> NoCapability for T where const { none::<T, AnyCapability>() } {}
