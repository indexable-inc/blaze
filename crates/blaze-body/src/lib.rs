//! The body: one complete, typed, generic form per function body, written by the one lowering
//! and read by every backend (docs/v2/plan/design/backend-next.md, "The body", D119).
//!
//! - [`body`]: typed ids, each row's form as a Rust value, and the body as flat tables of
//!   fixed-width rows ([`Body`] owned, [`BodyRef`] read in place).
//! - [`code`]: the stable encoding of every row and enum: one home for every number.
//! - [`build`]: [`BodyBuilder`], what a lowering writes a body with.
//! - [`text`]: the text form, printed and parsed with an exact round trip.
//! - [`verify`]: the well-formedness check, returning the [`Verified`] proof later passes take.
//! - [`record`]: the body as one blaze-format record, written once and read in place.
//! - [`kept`]: the body with the portable type table it indexes, as the Bodies stage keeps
//!   it for a run to build instances from (run.md R4).
//!
//! Every function is sans-IO and takes the allocator it allocates in.

#![no_std]
#![forbid(unsafe_code)]
#![feature(allocator_api, register_tool, custom_inner_attributes)]
#![register_tool(blaze)]

#[cfg(test)]
extern crate std;

pub mod body;
pub mod build;
pub mod code;
pub mod kept;
pub mod record;
pub mod text;
pub mod verify;

#[cfg(test)]
mod tests;

pub use body::{
    AggregateKind, AssertKind, BadRow, BinOp, Block, BlockKind, BlockRow, Body, BodyRef, BorrowKind,
    Callee, CalleeId, CastKind, Const, ConstDecl, ConstKind, Edge, Loan, LoanRow, Local, LocalDecl,
    LocalKind, NONE, Operand, Ops, Origin, Place, PlaceRow, Point, Proj, Role, Span, Stmt, StmtKind,
    StmtRow, Subset, Table, TyError, TyListRef, UnOp, Unwind, Value,
};
pub use build::{BodyBuilder, BuildError};
pub use kept::{KEPT_VERSION, Kept, KeptColumn, KeptError, KeptRoot, TableId};
pub use record::{EncodeError, OpenError, VERSION, encode, open, try_encode};
pub use text::{ParseError, ParseWhat, PrintError, parse, print};
pub use verify::{Site, Verified, VerifyError, verify, verify_ref};
