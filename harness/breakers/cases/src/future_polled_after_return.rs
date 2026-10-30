//! A future polled again after it returned panics with "`async fn` resumed after completion",
//! as rustc's coroutine transform has it, at the `async` block, or at the body of an `async fn`
//! or closure. ra-jit trapped.

use std::future::Future;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::pin::Pin;
use std::task::{Context, Poll, Waker};

/// Pending once, then ready.
struct Once(bool);

impl Future for Once {
    type Output = ();
    fn poll(mut self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<()> {
        if std::mem::replace(&mut self.0, true) { Poll::Ready(()) } else { Poll::Pending }
    }
}

async fn twice(x: u32) -> u32 {
    Once(false).await;
    x * 2
}

fn poll_three_times<F: Future>(future: F) -> Vec<String> {
    let mut future = Box::pin(future);
    let mut cx = Context::from_waker(Waker::noop());
    (0..3)
        .map(|_| match catch_unwind(AssertUnwindSafe(|| future.as_mut().poll(&mut cx))) {
            Ok(Poll::Ready(_)) => "ready".to_string(),
            Ok(Poll::Pending) => "pending".to_string(),
            Err(payload) => payload.downcast_ref::<&str>().copied().unwrap_or("?").to_string(),
        })
        .collect()
}

pub fn run() -> Vec<Vec<String>> {
    let closure = async |x: u32| {
        Once(false).await;
        x + 1
    };
    vec![
        poll_three_times(async { Once(false).await }),
        poll_three_times(twice(3)),
        poll_three_times(closure(1)),
    ]
}
