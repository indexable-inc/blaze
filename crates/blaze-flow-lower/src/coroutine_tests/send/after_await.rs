//! A temporary of a statement AFTER the `.await` is not stored (std census 3, class C: the
//! `fmt::Arguments` of a `format!` after the `.await`, `!Send` by core/src/fmt/mod.rs
//! `impl !Send for Arguments<'_> {}`, made the type check's superset interior `!Send`).
//!
//! The rule: the Reference, "Temporary scopes": a temporary lives to the end of its enclosing
//! statement; "Async blocks": the state stores "the data needed to resume" from each await
//! point. A temporary made after the `.await` is not storage-live at the suspension
//! (blaze-flow suspend.rs `saved`): the future is `Send`. The same temporary made before the
//! `.await` in the `.await`'s own statement lives across it: not `Send` (the control).

use std::vec;

use blaze_typeck::fixture::Bb;
use blaze_typeck::{DiagCode, ExprId, Mutbl};

use super::send_findings;

/// `ready().await; take(&make_rc(), ())`: the `Rc<i32>` temporary is made, borrowed and dropped
/// in the statement after the `.await` (the shape of `println!("{}", format!(..))` after it).
fn temp_after(b: &mut Bb) -> ExprId {
    let e0 = b.path(&["ready"]);
    let e1 = b.call(e0, &[]);
    let e2 = b.await_(e1);
    let s0 = b.semi(e2);
    let e3 = b.path(&["take"]);
    let e4 = b.path(&["make_rc"]);
    let e5 = b.call(e4, &[]);
    let e6 = b.borrow(Mutbl::Not, e5);
    let e7 = b.tuple(&[]);
    let e8 = b.call(e3, &[e6, e7]);
    let s1 = b.semi(e8);
    b.block(&[s0, s1], None)
}

/// `async { ready().await; take(&make_rc(), ()) }` given to `spawn<F: Send>`: the superset holds
/// `Rc<i32>` (the goal stalls), the exact interior does not: no finding. Control (rule 23): the
/// temporary made before the `.await` in its own statement (`take(&make_rc(), ready().await)`)
/// is stored: one E0277.
/// Prints "a temporary made after the await: Send", then "control: the same temporary before the
/// await (its statement holds the .await): E0277".
#[test]
fn a_temporary_made_after_the_await_is_not_stored() {
    let (stalled, got) = send_findings(temp_after);
    assert_eq!(stalled, 1, "the superset holds `Rc<i32>`: the goal stalls");
    assert!(got.is_empty(), "no finding on the exact interior: {got:?}");
    std::println!("a temporary made after the await: Send");
    let (stalled, got) = send_findings(crate::coroutine_tests::state_recipe::temp_across);
    assert_eq!(stalled, 1, "the control's superset holds `Rc<i32>` too");
    assert_eq!(got, vec![DiagCode::Unsatisfied], "the control's temporary lives across the `.await`");
    std::println!("control: the same temporary before the await (its statement holds the .await): E0277");
}
