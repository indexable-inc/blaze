//! The flow, one mid-level form per body, and the move and borrow checks over it
//! (docs/v2/plan/design/flow.md, decision D61).
//!
//! - [`flow`]: typed ids, the body as flat tables (places interned by parent, statements in one
//!   array, blocks as ranges of it), a builder for hand-made flows and a printer with a stable text.
//! - [`moves`]: the move check: maybe-initialized and maybe-uninitialized as forward bitset
//!   dataflows, the three findings, and the drop kind the builder's drop elaboration reads.
//! - [`loans`]: the borrow check: accesses and their conflicts, liveness, two-phase activation,
//!   tier one (location-insensitive) and tier two (the Polonius rules, as stated).
//! - [`record`]: one body's record: findings, and the [`record::Checked`] proof the builder takes.
//!
//! Every analysis is a sans-IO function of a flow and an allocator the caller names; every loop
//! over a graph is bounded. Tier two counts deterministic work and refuses the body
//! with [`flow::Bound::TierTwo`] if its budget is exhausted.

#![no_std]
#![forbid(unsafe_code)]
#![feature(allocator_api, register_tool, custom_inner_attributes)]
#![register_tool(blaze)]

#[cfg(test)]
extern crate std;

pub mod bits;
pub mod flow;
pub mod loans;
pub mod moves;
pub mod record;
pub mod suspend;

#[cfg(test)]
mod operand_order_tests;
#[cfg(test)]
mod place_mark_tests;
#[cfg(test)]
mod suspend_tests;
#[cfg(test)]
mod tests;

pub use flow::{
    Block, BorrowKind, Bound, Flow, FlowBuilder, FlowError, Loan, Local, Operand, Origin, Place,
    PlaceMark, Point, Proj, Span, Stmt, StmtKind, Value,
};
pub use loans::{BorrowFinding, Kept, LoanParts, TierOne};
pub use moves::{DropKind, MoveFinding, MoveFindingKind, MoveOut};
pub use record::{Checked, Record, check, check_named};
pub use suspend::{Saved, saved};
