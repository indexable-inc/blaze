use std::future::Future;
use std::pin::pin;
use std::sync::Arc;
use std::task::{Context, Poll, Wake, Waker};

struct Idle;

impl Wake for Idle {
    fn wake(self: Arc<Self>) {}
}

pub fn block_on<F: Future>(future: F) -> F::Output {
    let waker = Waker::from(Arc::new(Idle));
    let mut cx = Context::from_waker(&waker);
    let mut future = pin!(future);
    loop {
        if let Poll::Ready(out) = future.as_mut().poll(&mut cx) {
            return out;
        }
    }
}

async fn step(x: u32) -> u32 {
    x + 10
}

pub async fn pipeline(x: u32) -> u32 {
    let a = step(x).await;
    let b = step(a).await;
    a + b
}
