//! One rule for an async block's ordinal, two loops (port27/tempo-run/ASYNC.md P3 step 3): the
//! checker's `blaze_typeck::coroutine_id::async_ordinal` (what writeback names a hidden
//! `PNode::Coroutine` by) and the builder's [`crate::coroutines_in`] (what names the state, the
//! poll instance and the drop) must give every async block of a body the same ordinal, or a
//! caller reading an `async fn`'s hidden type would lay out and poll ANOTHER block's state: a
//! silent wrong program.
//!
//! The rule (typeck.md, "A closure's identity", which the coroutine's copies): a block's
//! ordinal is its position among its owner body's `async` blocks in expression (`ExprId`)
//! order; nested blocks count in the same sequence.

use std::vec::Vec;

use blaze_typeck::fixture::Bb;
use blaze_typeck::{ExprId, ItemKey};

/// `{ let _ = async { async { 1u8 } }; async move { 2u8 } }` with an unrelated block between:
/// three async blocks (one nested in another), so the ordinals are 0, 1, 2 in `ExprId` order
/// and a block's ordinal is never its expression id. Each of the three gets the same ordinal
/// from both rules, and an expression that is not an async block has none from the checker's.
/// Should print: "test tests::coroutine_id_agree::both_ordinal_rules_agree_on_a_body_with_three_blocks ... ok".
#[test]
fn both_ordinal_rules_agree_on_a_body_with_three_blocks() {
    let mut b = Bb::new();
    let one = b.int(None);
    let inner_body = b.block(&[], Some(one));
    let inner = b.async_block(inner_body, blaze_typeck::Capture::Ref);
    let outer_body = b.block(&[], Some(inner));
    let outer = b.async_block(outer_body, blaze_typeck::Capture::Ref);
    let s = b.semi(outer);
    let two = b.int(None);
    let plain = b.block(&[], Some(two));
    let last_body = b.block(&[], Some(plain));
    let last = b.async_block(last_body, blaze_typeck::Capture::Move);
    let root = b.block(&[s], Some(last));
    let body = b.view(ItemKey(0x0c0f_0001), root);
    let builder: Vec<(u32, ExprId)> = crate::coroutines_in(&body).collect();
    assert_eq!(builder.len(), 3, "three async blocks: {builder:?}");
    assert_eq!(builder.iter().map(|&(_, e)| e).collect::<Vec<_>>(), std::vec![inner, outer, last]);
    for &(ordinal, e) in &builder {
        assert_eq!(blaze_typeck::coroutine_id::async_ordinal(&body, e), Some(ordinal), "block {e:?}");
    }
    assert_eq!(blaze_typeck::coroutine_id::async_ordinal(&body, plain), None, "a plain block has no ordinal");
}
