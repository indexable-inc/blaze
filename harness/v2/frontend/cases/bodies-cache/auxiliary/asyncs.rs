//@ edition: 2021
use std::future::Future;
use std::pin::pin;
use std::task::{Context, Poll, Waker};
pub async fn double(x: u32) -> u32 { x * 2 }
pub async fn chain(x: u32) -> u32 { let a = double(x).await; double(a).await + 1 }
pub fn block_on<F: Future>(f: F) -> F::Output {
    let mut f = pin!(f);
    let mut cx = Context::from_waker(Waker::noop());
    loop { if let Poll::Ready(v) = f.as_mut().poll(&mut cx) { return v; } }
}
