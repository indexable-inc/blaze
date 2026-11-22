//! Drop elaboration over the body (docs/v2/plan/design/backend-next.md, "The body", D119 rule 3).
//!
//! The lowering writes every drop un-elaborated: `drop P` means "drop `P` if it is initialized"
//! (the Reference, "Destructors": a variable or temporary is dropped when it goes out of scope
//! only if it is still initialized; a partly moved value drops its remaining fields; unwinding
//! runs the same drops on the cleanup path). The move check knows, at each such row, whether
//! `P` is initialized on every path, on none, on some, or with parts moved out. [`elaborate`]
//! reads that answer through [`DropFacts`] and writes a body in which every `Drop` row is
//! unconditional: once a body, not once an instance.
//!
//! - [`facts`]: the seam, what the move check and the type table answer.
//! - [`elaborate`]: the pass, body to body.
//!
//! Every function is sans-IO and takes the allocator it allocates in.

#![no_std]
#![feature(allocator_api, register_tool, custom_inner_attributes)]
#![register_tool(blaze)]

#[cfg(test)]
extern crate std;

pub mod elaborate;
pub mod facts;

#[cfg(test)]
mod tests;

pub use elaborate::{ElabError, Plant, elaborate, elaborate_planted};
pub use facts::{DropFacts, DropState, Field, NeedsDrop, Part, PartState};
