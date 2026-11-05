//@ expect: accept
use std::{
    future::Future,
    panic,
    pin::pin,
    task::{Context, Poll, Waker},
};

struct D(&'static str);

impl Drop for D {
    fn drop(&mut self) {
        println!("drop {}", self.0);
    }
}

async fn step(n: u32) -> u32 {
    if std::hint::black_box(n) == 2 {
        panic!("step {n}");
    }
    n
}

async fn run() -> u32 {
    let _a = D("a");
    let x = step(1).await;
    let _b = D("b");
    let y = step(2).await;
    x + y
}

fn main() {
    let caught = panic::catch_unwind(|| {
        let mut future = pin!(run());
        let mut cx = Context::from_waker(Waker::noop());
        match future.as_mut().poll(&mut cx) {
            Poll::Ready(v) => println!("ready {v}"),
            Poll::Pending => println!("pending"),
        }
    });
    println!("caught: {}", caught.is_err());
}
