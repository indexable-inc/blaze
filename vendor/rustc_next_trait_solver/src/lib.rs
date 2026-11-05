//! Crate containing the implementation of the next-generation trait solver.
//!
//! This crate may also contain things that are used by the old trait solver,
//! but were uplifted in the process of making the new trait solver generic.
//! So if you got to this crate from the old solver, it's totally normal.

// tidy-alphabetical-start
#![allow(rustc::direct_use_of_rustc_type_ir)]
#![allow(rustc::usage_of_type_ir_inherent)]
#![allow(rustc::usage_of_type_ir_traits)]
// tidy-alphabetical-end
// docs/v2/plan/design/unsafe.md: the crate has no `unsafe` of its own. `deny`, not `forbid`:
// `TrivialTypeTraversalImpls!` (in `coherence`) expands to `rustc_type_ir`'s allowed
// `unsafe impl GenericTypeVisitable`, which a `forbid` would refuse.
#![deny(unsafe_code)]

#[cfg(test)]
#[global_allocator]
static ALLOC: blaze_alloc_count::Counting = blaze_alloc_count::Counting;

pub mod canonical;
pub mod coherence;
pub mod delegate;
pub mod normalize;
pub mod placeholder;
pub mod solve;
