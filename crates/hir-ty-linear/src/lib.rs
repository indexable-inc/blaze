//! `#[blaze::linear]`: a value of a marked type is never dropped implicitly. Every path out of
//! the code that holds one consumes it: moves it into a function that takes it by value (which
//! destructures it, or is marked `#[blaze::consumes]`), or destructures it in place. A drop at
//! the end of a scope, on an early exit (`return`, `?`, `break`, `continue`) or of an
//! overwritten place is an error naming the value and the path, at the line of the drop.
//!
//! The check reads the MIR lowering builds ([`hir_ty_mir::mir::MirPhase::Built`]), where every
//! value that may be dropped has a `Drop` at each place it may be, and the move paths and
//! maybe-init analysis drop elaboration runs ([`hir_ty_mir::mir::drop_states`]): a `Drop` of a
//! path that may hold a linear value is one the program may run. Cleanup blocks are not read:
//! unwinding from a panic still drops what is in scope, which runs the type's `Drop` impl if it
//! has one (its abandonment path, a rollback) or its fields' drop glue, and with `panic=abort`
//! runs nothing. Every call may unwind, so a rule over unwind edges would leave no call legal.
//!
//! Generic code drops what it holds unseen, so a linear type is an argument only of a type
//! parameter marked `#[blaze::linear]` (Swift's `~Copyable`, the `?Drop` of Rust's must-move
//! discussions): every function a body names, with the type arguments it names it with, is
//! checked here ([`generic`]). The design, the prior art and what is not checked yet are in
//! docs/v2/plan/design/linear.md.
//!
//! [`check_body`] is called by the borrow check for each body it checks
//! (`hir_ty_borrowck::check_owner`), so its errors refuse the body's code as a borrow check
//! error does (`hir_ty_borrowck::checked`), its verdict is in what the item store files, and
//! hir reports them with the borrow checker's. A program with no `#[blaze::linear]` gets no
//! error and no change: nothing here writes MIR, and every type it is asked about holds no
//! linear value.

#![forbid(unsafe_code)]
#![feature(allocator_api)]

mod drops;
mod generic;
mod names;
mod ty;

#[cfg(test)]
mod tests;

use base_db::Db;
use hir_def::DefWithBodyId;
use hir_ty_mir::mir::{MirBody, MirSpan};

pub use ty::{contains_linear, is_linear_adt};

/// An error of the check: a linear value dropped implicitly, or a linear type given where
/// code may drop it.
#[derive(Debug, Clone, PartialEq, Eq, blaze_query::Value)]
pub struct LinearDiagnostic {
    /// What the error is about, as the drop or the call is written.
    pub span: MirSpan,
    /// The error, naming the value, its type and the path that drops it.
    pub message: String,
    /// Where the value is declared, where it leaves early.
    pub labels: Vec<(MirSpan, String)>,
    /// How to consume it.
    pub notes: Vec<String>,
}

impl LinearDiagnostic {
    /// The code the error is reported with.
    pub const CODE: &'static str = "LINEAR";
}

/// What one body is, as the borrow check walks the bodies of an item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BodyKind {
    /// The item's own body: a `#[blaze::consumes]` function's by-value parameters are linear
    /// nowhere in it.
    Item(DefWithBodyId),
    /// A closure, an `async` block or a `const { .. }` block written in the item.
    Nested,
}

/// The errors of `body`, one of the bodies of an item, in the order of its blocks.
pub fn check_body<'db>(
    db: &'db Db,
    body: &MirBody<'db>,
    kind: BodyKind,
    out: &mut Vec<LinearDiagnostic>,
) {
    let _p = tracing::info_span!(target: "blaze", "linear_check", owner = ?body.owner).entered();
    blaze_mem::frame::task(blaze_mem::Tag::Mir, |frame| {
        let types = ty::Types::new(db, body, kind, frame);
        generic::check_calls(&types, body, out);
        if let BodyKind::Item(def) = kind {
            generic::check_impl_bindings(&types, def, out);
        }
        if body.coroutine.is_some() {
            drops::check_coroutine(&types, body, out);
        } else {
            drops::check_drops(&types, body, out);
        }
    });
}
