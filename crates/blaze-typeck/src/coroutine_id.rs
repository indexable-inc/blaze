//! A coroutine's identity across bodies (docs: port27/tempo-run/ASYNC.md D-A3, prompt P3), the
//! sibling of a closure's (`PNode::Closure { owner, ordinal, args }`, typeck.md "A closure's
//! identity").
//!
//! The Reference, "Async blocks": "An async block is a variant of a block expression which
//! evaluates to a future", and "Closure types" for its sibling: each such expression has its own
//! anonymous type. Inside its owner's body that type is named by its expression
//! (`TyView::Future(e, output, interior)`); an expression id means nothing in another body, so
//! across bodies (an `async fn`'s hidden type read by a caller, a type another body imports) it
//! is named by `(owner, ordinal, args)`: the owner item whose body holds the block, the block's
//! ORDINAL among that body's `Expr::AsyncBlock`s in expression (`ExprId`) order, and the owner's
//! generic arguments.
//!
//! The ordinal rule here is the same rule as blaze-build `coroutines_in` (a block's position
//! among the body's async blocks in `ExprId` order); the builder keeps its own loop and a test
//! there asserts the two agree (blaze-build src/tests/coroutine_id_agree.rs).

use crate::body::{BodyView, Expr};
use crate::hash::StableHasher;
use crate::ids::{ExprId, ItemKey};

/// The ordinal of async block `e` in `body`: the number of `Expr::AsyncBlock`s before it in
/// `ExprId` order. `None` when `e` is past the body or is not an async block (a caller holding
/// such an id has a malformed type, and refuses it by name).
pub fn async_ordinal(body: &BodyView<'_>, e: ExprId) -> Option<u32> {
    let at = e.0 as usize;
    if !matches!(body.exprs.get(at)?, Expr::AsyncBlock { .. }) {
        return None;
    }
    let before = body.exprs.get(..at)?;
    // A body's expression count fits a `u32` (an `ExprId` is one), so the count does too.
    Some(before.iter().filter(|x| matches!(x, Expr::AsyncBlock { .. })).count() as u32)
}

/// The solver's def of coroutine `ordinal` of `owner`: a hash of the identity alone, so two
/// bodies that name the same coroutine (its owner, and a caller reading the owner's hidden type)
/// give the solver one def. Never the expression id (body-local: infer/mod.rs's
/// `"blaze-typeck future def"` is the owner's own form).
pub fn coroutine_def(owner: ItemKey, ordinal: u32) -> u128 {
    let mut h = StableHasher::new("blaze-typeck coroutine def");
    h.u128(owner.0);
    h.u32(ordinal);
    u128::from(h.finish())
}
