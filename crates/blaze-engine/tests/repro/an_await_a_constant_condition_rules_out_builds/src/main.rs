//! An `.await` in a branch a constant condition rules out, beside one that runs. Each `.await`
//! that runs suspends once.

use std::future::Future;
use std::pin::{Pin, pin};
use std::task::{Context, Poll, Waker};

trait Flag {
    const ON: bool;
}

struct On;
struct Off;

impl Flag for On {
    const ON: bool = true;
}

impl Flag for Off {
    const ON: bool = false;
}

/// Pending once, then 7.
struct Once(bool);

impl Future for Once {
    type Output = u8;

    fn poll(mut self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<u8> {
        if self.0 {
            return Poll::Ready(7);
        }
        self.0 = true;
        Poll::Pending
    }
}

struct Loud(&'static str);

impl Drop for Loud {
    fn drop(&mut self) {
        println!("dropped {}", self.0);
    }
}

async fn early<F: Flag>() -> u8 {
    let _loud = Loud("early");
    if F::ON {
        return Once(false).await;
    }
    Once(false).await + 1
}

async fn only_after<F: Flag>() -> u8 {
    if F::ON {
        return 2;
    }
    Once(false).await
}

async fn on_a_const() -> u8 {
    const ON: bool = false;
    if ON {
        return Once(false).await;
    }
    Once(false).await + 1
}

/// Polls `fut` to its end: what it returns, and how many polls that took.
fn block_on<F: Future>(fut: F) -> (F::Output, u32) {
    let mut fut = pin!(fut);
    let mut cx = Context::from_waker(Waker::noop());
    let mut polls = 1;
    loop {
        if let Poll::Ready(out) = fut.as_mut().poll(&mut cx) {
            return (out, polls);
        }
        polls += 1;
    }
}

fn main() {
    println!("{:?} {:?}", block_on(early::<On>()), block_on(early::<Off>()));
    println!("{:?} {:?}", block_on(only_after::<On>()), block_on(only_after::<Off>()));
    println!("{:?}", block_on(on_a_const()));
    // Dropped while suspended at the `.await` that runs.
    let mut fut = Box::pin(early::<Off>());
    assert!(fut.as_mut().poll(&mut Context::from_waker(Waker::noop())).is_pending());
    drop(fut);
}
