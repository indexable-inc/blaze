//! `super let` (async plan P11, D-A11): which `let` statements of a body were written `super let`.
//!
//! The library's `pin!` (core/src/pin.rs) is
//! `$crate::pin::Pin::<&mut _> { pointer: &mut { super let mut pinned = PinMacroHelper { value: $value }; .. } }`
//! in shape: a binding that must outlive the block it is written in. Lowered as a plain `let`, that
//! binding is dropped at its block's end and the pointer the block returns names a dead slot.
//!
//! The record keeps the keyword as one bit of the `let` statement's row ([`LET_SUPER`] in the
//! row's `flags`; the statement's other fields are unchanged), so the record's columns and its
//! schema stay as they are; [`super_lets`] reads the bits back as the sorted column
//! `blaze_typeck::BodyView::super_lets`.

use blaze_own::{AVec, Alloc, Vec};

use crate::columns::View;
use crate::form::{Node, StmtId};

/// The bit of a `let` statement's row (`Stmt::Let`, tag 0) that says it was written `super let`.
pub(crate) const LET_SUPER: u16 = 1;

/// The tag of a `let` statement's row (`Stmt::encode`).
const LET_TAG: u16 = 0;

/// Whether row `n` of the `stmts` column is a `let` written `super let`.
pub(crate) fn is_super_let(n: &Node) -> bool {
    n.tag == LET_TAG && n.flags & LET_SUPER != 0
}

/// Every `super let` statement of `view`, in ascending order of its id (one pass over the
/// column, so sorted by construction).
pub(crate) fn super_lets<'a>(view: &View<'_>, alloc: Alloc<'a>) -> AVec<'a, StmtId> {
    let mut out = Vec::new_in(alloc);
    for (i, n) in view.stmts.iter().enumerate() {
        if is_super_let(n) {
            out.push(StmtId(i as u32));
        }
    }
    out
}
