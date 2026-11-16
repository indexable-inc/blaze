//! The coroutine transform (docs/v2/plan/design/coroutines.md): what turns the body of an
//! `async fn` or an `async` block into a state and one `resume` function the code builder
//! builds as ordinary code.
//!
//! - [`ir`]: the small form it works on: typed ids, a body as flat tables, `Suspend` as the
//!   only suspension, a builder for hand-made bodies and a printer with a stable text.
//! - [`live`]: which locals are held across each suspension: live by use, or maybe borrowed.
//! - [`layout`]: the state: a slot per upvar and per saved local, and the tags.
//! - [`transform`]: the rewrite to a [`Machine`]: `Park` for `Suspend`, the entry table of
//!   `resume` and the entry table of the state's drop glue.
//!
//! Every analysis is a sans-IO function of a body and an allocator the caller names; every loop
//! over the graph counts its passes against a stated bound or marks what it has seen. The
//! adapter from a checked body, and the builder's use of a machine, are the next leg
//! (coroutines.md, "From a checked body and back").

#![no_std]
#![feature(allocator_api, register_tool, custom_inner_attributes)]
#![register_tool(blaze)]

#[cfg(test)]
extern crate std;

pub mod ir;
pub mod layout;
pub mod live;
pub mod transform;

#[cfg(test)]
mod interp;
#[cfg(test)]
mod tests;

pub use ir::{
    Access, BlockId, BlockRow, Body, BodyBuilder, IrError, LocalId, LocalRow, Operand, PointId, SlotId, Span32, Stmt,
    Term, validate,
};
pub use layout::{Home, Layout, Tag, layout};
pub use live::{Across, across};
pub use transform::{DropEntry, Entry, Machine, TransformError, transform, transform_planted};

/// A planted bug, for the simulation's controls. Only test builds act on one.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Plant {
    None,
    /// The last saved local that is not an upvar gets no slot: it stays in the frame.
    LiveLeftOut,
    /// A local kept alive only by a borrow gets no slot.
    BorrowsNotCounted,
    /// Cancelling at any point drops every saved local that needs drop, initialised or not.
    CancelDropsAll,
    /// The tag of point `k` resumes at point `k + 1`.
    ResumeNext,
    /// A borrowed saved local lives in the frame and is copied to its slot at each suspension
    /// and back at each resume: a saved reference to it points at a frame that is gone.
    SavedByCopy,
}
