//@ expect: accept
use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll, Waker},
};

struct D(&'static str);

impl Drop for D {
    fn drop(&mut self) {
        println!("drop {}", self.0);
    }
}

/// Pending the first time it is polled.
struct Later(bool);

impl Future for Later {
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

async fn waits() -> u32 {
    let _a = D("a");
    {
        let _b = D("b");
        let _temp_arg = (D("operand"), Later(false).await);
    }
    let _c = D("c");
    Later(false).await;
    3
}

fn main() {
    let mut cx = Context::from_waker(Waker::noop());
    let mut first = Box::pin(waits());
    println!("{:?}", first.as_mut().poll(&mut cx).is_pending());
    drop(first);
    println!("dropped at the first await");
    let mut second = Box::pin(waits());
    println!("{:?}", second.as_mut().poll(&mut cx).is_pending());
    println!("{:?}", second.as_mut().poll(&mut cx).is_pending());
    drop(second);
    println!("dropped at the second await");
}
