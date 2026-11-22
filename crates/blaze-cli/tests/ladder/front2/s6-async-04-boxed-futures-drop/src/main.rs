//! s6-async-04: what an async fn's state holds and when it is dropped. A local of an async fn
//! lives in the future's state across an `.await`: dropping the future while it is suspended
//! drops that local; when the body completes, its locals drop before `Ready` is returned. A
//! future never polled has run none of its body, so it holds only its captures: an async fn's
//! arguments, or an `async move` block's captured values, which drop with it. Then a `Vec` of
//! `Pin<Box<dyn Future<Output = String>>>` of three kinds, each polled to completion in order.

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll, Waker};

struct Noisy(&'static str);

impl Drop for Noisy {
    fn drop(&mut self) {
        println!("drop {}", self.0);
    }
}

struct PendingOnce(bool);

impl Future for PendingOnce {
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

async fn holds() -> u32 {
    let _n = Noisy("in-future");
    PendingOnce(false).await;
    5
}

fn main() {
    let mut cx = Context::from_waker(Waker::noop());
    {
        let mut f = Box::pin(holds());
        println!("{:?}", f.as_mut().poll(&mut cx));
        println!("dropping unfinished");
    }
    let mut g = Box::pin(holds());
    println!("{:?}", g.as_mut().poll(&mut cx));
    println!("{:?}", g.as_mut().poll(&mut cx));
    let never = holds();
    drop(never);
    println!("never polled: dropped quietly");
    let cap = Noisy("captured");
    let fut = async move { cap.0.len() };
    drop(fut);
    let futs: Vec<Pin<Box<dyn Future<Output = String>>>> = vec![
        Box::pin(async { String::from("one") }),
        Box::pin(async {
            PendingOnce(false).await;
            String::from("two")
        }),
        Box::pin(std::future::ready(String::from("three"))),
    ];
    let mut outs = Vec::new();
    for mut f in futs {
        loop {
            if let Poll::Ready(s) = f.as_mut().poll(&mut cx) {
                outs.push(s);
                break;
            }
        }
    }
    println!("{}", outs.join(" "));
    println!("end");
}
