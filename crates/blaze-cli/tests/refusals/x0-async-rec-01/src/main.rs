//! x0-async-rec-01 (front2; REFUSED, not a ladder rung: the ladder's `expected` has no refusal
//! form): an `async fn` that awaits itself BY VALUE. rustc refuses it with E0733 ("recursion in
//! an async fn requires boxing"): `f`'s state would hold `f(n - 1)`'s future across the
//! `.await`, and so itself, without end.
//!
//! blaze refuses it by name when the state is laid out from types (run/lay_states.rs,
//! front2/coroutine_needs.rs `layout_order` meets `f`'s block on its own stack): the error line
//! names E0733 and the cycle item by item, `x0_async_rec_01::f -> x0_async_rec_01::f` (the
//! crate's path form as `FrontEnd::item_named` gives it).
//!
//! Expected: `blaze run` exits non-zero; its `error: ` line contains `E0733` and `::f -> `.

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll, Waker};

async fn f(n: u32) {
    if n > 0 {
        f(n - 1).await
    }
}

fn main() {
    let mut cx = Context::from_waker(Waker::noop());
    let mut fut = Box::pin(f(3));
    while fut.as_mut().poll(&mut cx).is_pending() {}
}
