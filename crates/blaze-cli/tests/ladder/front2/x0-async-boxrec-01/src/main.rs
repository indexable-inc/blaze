//! x0-async-boxrec-01 (front2; status 10): an `async fn` that awaits itself through a box, to
//! depth 10, driven by a loop written here (ASYNC.md P8: a state laid out from TYPES before any
//! LIR; a boxed recursion needs nothing special).
//!
//! The Reference, "Async functions": `async fn f(n: u32) -> u32 { body }` is roughly
//! `fn f(n: u32) -> impl Future<Output = u32> { async move { body } }`. Recursion is allowed when
//! the inner future is behind a pointer: `Box::pin(f(n - 1))` is `Pin<Box<F>>`, a pointer, so
//! `f`'s state holds a pointer to another `f` state, never one by value (rustc E0733 refuses only
//! the by-value form). The temporary `f(n - 1)` is moved into the box before the `.await`, so it
//! is not live across the suspension: the state holds the box, not the future.
//!
//! Each level is pending once before it awaits the next (`Yield`), so every state is suspended
//! and resumed: 11 levels (n = 10 down to 0), each adding 1 except the last: 10.
//!
//! What each wrong build exits with:
//! - 1: not 10 (a level's `1 +` lost, or the `n` captured wrong);
//! - 2: not ready after 1000 polls (a poll that starts a body again instead of resuming).
//!
//! The run prints: `front2/x0-async-boxrec-01: PASS (status 10, stdout byte-equal)`.

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll, Waker};

/// Pending once, then ready.
struct Yield(bool);

impl Future for Yield {
    type Output = ();
    fn poll(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<()> {
        if self.0 {
            Poll::Ready(())
        } else {
            self.0 = true;
            Poll::Pending
        }
    }
}

async fn f(n: u32) -> u32 {
    Yield(false).await;
    if n == 0 { 0 } else { 1 + Box::pin(f(n - 1)).await }
}

fn main() {
    let mut cx = Context::from_waker(Waker::noop());
    let mut fut = Box::pin(f(10));
    let mut polls = 0u32;
    let got = loop {
        polls += 1;
        if polls > 1000 {
            std::process::exit(2);
        }
        if let Poll::Ready(v) = fut.as_mut().poll(&mut cx) {
            break v;
        }
    };
    println!("{got} after {polls} polls");
    if got != 10 {
        std::process::exit(1);
    }
    std::process::exit(10);
}
