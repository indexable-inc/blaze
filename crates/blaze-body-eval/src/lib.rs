//! The constant evaluator over the one body (docs/v2/plan/design/backend-next.md, "The body:
//! one lowering", D119, rule 7 and step P5): an interpreter of a [`blaze_body::Verified`] body
//! at ground type arguments, over blaze-consteval's memory with provenance, layouts, integer
//! and float rules and typed refusals (rule 30: one machine, two walks until P7 deletes the
//! old one).
//!
//! - [`eval_body`]: the entry. A constant's body to its value as a data record, or what the
//!   world must supply first.
//! - [`BodyWorld`]: the facts seam, [`ConstWorld`](blaze_consteval::ConstWorld) plus the
//!   verified body of an item and the drop glue of a ground type.
//! - [`EvalError`]: the row it stopped at and why.
//!
//! Every function is sans-IO and takes the allocator it allocates in.

#![no_std]
#![feature(allocator_api)]
#![forbid(unsafe_code)]

// blaze-build's data record (`DataRecord`, `DataAlloc`) holds the standard allocator-aware vector,
// so `Machine::intern` builds it in `alloc::vec::Vec` directly.
extern crate alloc;
#[cfg(test)]
extern crate std;

mod error;
mod eval;
mod world;

#[cfg(test)]
mod tests;

pub use error::{EvalError, Fault, Row};
pub use eval::eval_body;
pub use world::{Bodied, BodyWorld, DropGlue};
