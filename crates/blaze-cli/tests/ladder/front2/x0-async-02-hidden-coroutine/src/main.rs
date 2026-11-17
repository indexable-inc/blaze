//! x0-async-02-hidden-coroutine (front2, no_core; success 42, else the failed check's number):
//! one `async fn` with no `.await`, polled ONCE by a hand-written `poll` call with a context
//! nothing wakes through (this program's no-op waker: x0-async-01's `future.rs` gives the
//! context a wake counter in the waker's place; no future here touches it). Its hidden type is
//! the future of the fn's root block, which the checker writes by identity
//! (`PNode::Coroutine`, async P3) and the run path grounds through front2/hidden_coroutine.rs.
//!
//! The Reference, "Async functions": `async fn f(x: A) -> T { body }` "is roughly equivalent
//! to" `fn f(x: A) -> impl Future<Output = T> { async move { body } }`: calling one runs none
//! of its body and returns a future that holds its arguments. Core's future/future.rs, `poll`:
//! "`Poll::Ready(val)` with the result `val` of this future if it finished successfully". A
//! body with no `.await` has no point to suspend at, so its first poll completes it.
//!
//! So `add(13, 29)`'s future, polled once, is `Ready(42)`, and nothing woke.
//!
//! What each wrong build exits with:
//! - 1: the first poll is `Pending` (a body that suspends where it has no `.await`);
//! - 2: ready, but not with 42 (an argument not moved into the future, or the output read
//!   from the wrong type: a hidden type's output not the declared `u8`);
//! - 3: a wake counted (the context's counter written by a future that never pends).
//!
//! The run prints: `front2/x0-async-02-hidden-coroutine: PASS (status 42, stdout byte-equal)`.
//! Before the hidden-coroutine arm the build stops with the builder's refusal "an async block's
//! future by identity (`PNode::Coroutine`): the builder grounds the `Future(ordinal, output,
//! FnDef)` form (coroutine.rs) until it reads `World::coroutine`".
#![feature(no_core, lang_items, intrinsics, rustc_attrs, unboxed_closures, arbitrary_self_types)]
#![no_core]
#![no_main]
#![allow(internal_features)]

mod future;
mod prelude;
use future::*;
use prelude::*;

async fn add(a: u8, b: u8) -> u8 {
    a + b
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    let mut wakes = 0u32;
    let mut fut = add(13, 29);
    let mut cx = Context { wakes: &mut wakes };
    // The future is a local that is not moved again: pinned where it stands.
    let pinned = Pin { pointer: &mut fut };
    let got = match Future::poll(pinned, &mut cx) {
        Poll::Ready(v) => v,
        Poll::Pending => return 1,
    };
    if got != 42 {
        2
    } else if wakes != 0 {
        3
    } else {
        42
    }
}
