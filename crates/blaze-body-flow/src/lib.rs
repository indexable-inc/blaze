//! The body projected to the flow the borrow check reads today (docs/v2/plan/design/
//! backend-next.md, "The body: one lowering", D119 P3).
//!
//! - [`project`]: a verified [`blaze_body::Body`] to a [`blaze_flow::Flow`], built through the
//!   flow's own builder, with [`Projected`]'s map both ways between the body's statements,
//!   blocks, places, locals and loans and the flow's, so a finding of the check is reported at a
//!   body row and a drop kind of the move check is read at a body's `Drop`.
//! - [`canon`]: a flow's canonical text, the same for two flows that differ only in how their
//!   blocks, locals, loans and origins are numbered and in goto chains: what makes blaze-flow-lower
//!   the oracle of the one lowering (tests/equal_flows.rs).
//!
//! Where the body says more than a flow holds (a value's arithmetic, a constant's bytes, a
//! callee's identity, unwind edges and cleanup blocks), the projection abstracts it as
//! blaze-flow-lower does; the module doc of [`project`] gives each row's flow form. A row with no
//! flow form is a typed [`ProjectError`] naming it, never skipped.
//!
//! Every function is sans-IO and takes the allocator it allocates in.

#![no_std]
#![feature(allocator_api, register_tool, custom_inner_attributes)]
#![register_tool(blaze)]

#[cfg(test)]
extern crate std;

pub mod algebra;
pub mod canon;
pub mod project;

#[cfg(test)]
mod tests;

pub use canon::canon;
pub use project::{ProjectError, Projected, project};
