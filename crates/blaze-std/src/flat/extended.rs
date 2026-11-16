//! The derived impl of [`Flat`], bounded by the walk's answer for the instance (`where const`,
//! `blaze_const_exprs`). Compiled by blaze only (`cfg(blaze)`, std.md, "Two builders"): rustc
//! cannot parse a `where const` bound, and the plain impl is in `flat.rs`.

use super::{Flat, is_flat};

// SAFETY: the walk for `AnyAddress` found no type that implements `Address` in a `T`'s own
// bytes and saw into every type on the way, and `T` has no drop glue: a `T` is its bytes, and
// none of them names memory, a cell, a capability or a thing of this machine.
unsafe impl<T: ?Sized> Flat for T where const { is_flat::<T>() } {}
