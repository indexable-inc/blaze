//! uv resolves requirements through a trait whose implementation refines the future's error type.
#![allow(refining_impl_trait)]
use std::future::Future;
use std::task::{Context, Poll, Waker};

trait ErrorCode {
    fn code(&self) -> u64;
}
struct Payload([u64; 4]);
impl ErrorCode for Payload {
    fn code(&self) -> u64 {
        self.0[3]
    }
}

trait Build {
    fn resolve(&self, ok: bool) -> impl Future<Output = Result<u64, impl ErrorCode>>;
}
struct Dispatch;
impl Build for Dispatch {
    async fn resolve(&self, ok: bool) -> Result<u64, Payload> {
        if ok { Ok(42) } else { Err(Payload([1, 2, 3, 53])) }
    }
}
async fn caller(ok: bool) -> u64 {
    match Dispatch.resolve(ok).await {
        Ok(value) => value,
        Err(err) => err.code(),
    }
}
fn poll<T>(future: impl Future<Output = T>) -> T {
    let mut f = std::pin::pin!(future);
    match f.as_mut().poll(&mut Context::from_waker(Waker::noop())) {
        Poll::Ready(value) => value,
        Poll::Pending => panic!("unexpected suspension"),
    }
}
pub fn run() -> (u64, u64) {
    (poll(caller(true)), poll(caller(false)))
}
