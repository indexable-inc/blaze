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

/// The ordinal base of an async closure's coroutine (ASYNC.md D-A9): the coroutine an
/// `async |..| body` returns is `PNode::Coroutine { owner, ordinal: CLOSURE_BASE + closure
/// ordinal, args }`, the closure ordinal being typeck.md's (the number of `Expr::Closure`s
/// before it in `ExprId` order, rules/writeback.rs `closure_ordinal`). Block ordinals stay
/// below it, so the two never meet and `PNode::Coroutine` needs no second variant.
pub const CLOSURE_BASE: u32 = 1 << 31;

/// The coroutine ordinal of `e`: an async block's [`async_ordinal`], or for an async closure
/// [`CLOSURE_BASE`] plus its closure ordinal (the Reference, "Closure expressions": an async
/// closure's call returns a future of its body; that future's type is named by the closure).
/// `None` when `e` is past the body, is neither form, or its ordinal would reach the other
/// form's range (a caller refuses it by name, as for `async_ordinal`).
pub fn coroutine_ordinal(body: &BodyView<'_>, e: ExprId) -> Option<u32> {
    let at = e.0 as usize;
    match body.exprs.get(at)? {
        Expr::AsyncBlock { .. } => async_ordinal(body, e).filter(|&o| o < CLOSURE_BASE),
        Expr::Closure { is_async: true, .. } => {
            let before = body.exprs.get(..at)?;
            // A body's expression count fits a `u32` (an `ExprId` is one), so the count does too.
            let n = before.iter().filter(|x| matches!(x, Expr::Closure { .. })).count() as u32;
            if n >= CLOSURE_BASE { None } else { Some(CLOSURE_BASE + n) }
        }
        _ => None,
    }
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

/// The solver's def of closure `ordinal` of `owner` seen from outside its body (a body that
/// imports `PNode::Closure`, infer/mod.rs `TyView::ClosureOf`; an opaque's hidden closure,
/// infer/facts.rs `fill_opaque`): a hash of the identity alone, so every reader names one
/// closure with one def. The domain string is the one `TyView::ClosureOf` used inline.
pub fn closure_of_def(owner: ItemKey, ordinal: u32) -> u128 {
    let mut h = StableHasher::new("blaze-typeck closure of");
    h.u128(owner.0);
    h.u32(ordinal);
    u128::from(h.finish())
}
