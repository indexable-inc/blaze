//! s6-async-01: `async fn` and `.await` driven by a hand-written `block_on` over the real
//! `core::task`: `Waker::noop()` (stable since 1.85: core/src/task/wake.rs), `Context::from_waker`,
//! `pin!`, a user `Future` that is `Pending` once and wakes itself, an async block capturing a
//! local by reference, `std::future::ready`, and an async closure body (`move |x| async move`).
//!
//! First to need: the coroutine transform of `async fn` and async blocks (their state machine,
//! resume and `Poll`), and `IntoFuture` in the desugaring of `.await`.

use std::future::Future;
use std::pin::{Pin, pin};
use std::task::{Context, Poll, Waker};

fn block_on<F: Future>(fut: F) -> F::Output {
    let mut fut = pin!(fut);
    let mut cx = Context::from_waker(Waker::noop());
    loop {
        if let Poll::Ready(v) = fut.as_mut().poll(&mut cx) {
            return v;
        }
    }
}

async fn add(a: i32, b: i32) -> i32 {
    a + b
}

async fn twice(x: i32) -> i32 {
    add(x, x).await
}

async fn greet(name: &str) -> String {
    format!("hello {name}")
}

struct YieldOnce(bool);

impl Future for YieldOnce {
    type Output = u32;
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<u32> {
        if self.0 {
            Poll::Ready(7)
        } else {
            self.0 = true;
            cx.waker().wake_by_ref();
            Poll::Pending
        }
    }
}

fn main() {
    println!("{}", block_on(add(2, 3)));
    println!("{}", block_on(twice(21)));
    println!("{}", block_on(greet("async")));
    let name = String::from("block");
    let r = block_on(async {
        let a = twice(1).await;
        let b = YieldOnce(false).await;
        format!("{name}: {a} {b}")
    });
    println!("{r}");
    let polls = {
        let mut f = pin!(YieldOnce(false));
        let mut cx = Context::from_waker(Waker::noop());
        let mut n = 0;
        loop {
            n += 1;
            if let Poll::Ready(_) = f.as_mut().poll(&mut cx) {
                break n;
            }
        }
    };
    println!("polls {polls}");
    println!("{}", block_on(std::future::ready(9)));
    let triple = |x: u8| async move { x as u32 * 3 };
    println!("{}", block_on(triple(5)));
}
