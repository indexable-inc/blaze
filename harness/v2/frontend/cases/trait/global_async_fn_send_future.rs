//@ expect: accept
//@ edition: 2021
// An `async fn` future is Send when nothing non-Send lives across an await: the `Rc` here is
// dropped at the end of its block, before the await. The future is run with a no-op waker.
use std::future::{ready, Future};
use std::pin::pin;
use std::rc::Rc;
use std::task::{Context, Poll, Waker};

fn block_on<F: Future>(f: F) -> F::Output {
    let mut f = pin!(f);
    let mut cx = Context::from_waker(Waker::noop());
    loop {
        if let Poll::Ready(v) = f.as_mut().poll(&mut cx) {
            return v;
        }
    }
}

fn spawn_send<F: Future + Send>(f: F) -> F::Output {
    block_on(f)
}

async fn add(a: u32, b: u32) -> u32 {
    ready(()).await;
    a + b
}

async fn scoped() -> u32 {
    let n = {
        let rc = Rc::new(2u32);
        *rc
    };
    ready(()).await;
    n + add(1, 2).await
}

fn main() {
    println!("{}", spawn_send(scoped()));
}
