//@ expect: accept
use std::{
    future::Future,
    pin::pin,
    task::{Context, Poll, Waker},
};

struct D(&'static str);

impl Drop for D {
    fn drop(&mut self) {
        println!("drop {}", self.0);
    }
}

async fn ready(n: u32) -> u32 {
    let _inside = D("inside ready");
    n
}

async fn work() -> u32 {
    let _held = D("held across");
    let a = ready(1).await;
    let _between = D("between");
    let b = ready(2).await;
    {
        let _scoped = D("scoped");
        a + b
    }
}

fn block_on<F: Future>(future: F) -> F::Output {
    let mut future = pin!(future);
    let mut cx = Context::from_waker(Waker::noop());
    loop {
        if let Poll::Ready(value) = future.as_mut().poll(&mut cx) {
            return value;
        }
    }
}

fn main() {
    println!("{}", block_on(work()));
    let value = block_on(async {
        let _block = D("async block");
        ready(5).await + 1
    });
    println!("{value}");
}
