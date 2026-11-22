//! s6-async-06: a bound on an `async fn`'s output through its opaque future. The Reference,
//! "Async functions": `async fn seven() -> u32` is "roughly equivalent to" `fn seven() -> impl
//! Future<Output = u32>`, so a caller normalizes `<F as Future>::Output` to `u32` from the
//! opaque's own bound, and `u32: Unpin` holds (auto trait, no fields). Split out of s6-async-02
//! (std census 1, class C2): its `impl<A: Future, B: Future> Future for Join2<A, B> where
//! A::Output: Unpin` at `A` an `async fn`'s future; here with no `String` in it.

use std::future::Future;
use std::pin::pin;
use std::task::{Context, Poll, Waker};

async fn seven() -> u32 {
    7
}

fn output_unpin<F: Future>(f: F) -> F
where
    F::Output: Unpin,
{
    f
}

fn main() {
    let f = pin!(output_unpin(seven()));
    let mut cx = Context::from_waker(Waker::noop());
    match f.poll(&mut cx) {
        Poll::Ready(v) => println!("ready {v}"),
        Poll::Pending => println!("pending"),
    }
}
