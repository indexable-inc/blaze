//! s6-async-02: two futures joined by hand. `Join2` polls each unfinished future once per poll,
//! `a` before `b`, and is ready when both are; each `Countdown` logs every poll and is `Pending`
//! `left` times. The poll order is fixed by this program alone, so the log is exact:
//! join(a: 1, b: 3) polls 4 times; join(async x: 2, async y: 0) polls 3 times. Then a `Vec` of
//! `Pin<Box<dyn Future>>` from async blocks, each ready at its first poll.

use std::cell::RefCell;
use std::future::Future;
use std::pin::{Pin, pin};
use std::rc::Rc;
use std::task::{Context, Poll, Waker};

type Log = Rc<RefCell<Vec<String>>>;

struct Countdown {
    name: &'static str,
    left: u32,
    log: Log,
}

impl Future for Countdown {
    type Output = &'static str;
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<&'static str> {
        let me = &mut *self;
        me.log.borrow_mut().push(format!("{} {}", me.name, me.left));
        if me.left == 0 {
            Poll::Ready(me.name)
        } else {
            me.left -= 1;
            cx.waker().wake_by_ref();
            Poll::Pending
        }
    }
}

struct Join2<A: Future, B: Future> {
    a: Pin<Box<A>>,
    b: Pin<Box<B>>,
    ra: Option<A::Output>,
    rb: Option<B::Output>,
}

impl<A: Future, B: Future> Future for Join2<A, B>
where
    A::Output: Unpin,
    B::Output: Unpin,
{
    type Output = (A::Output, B::Output);
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let me = &mut *self;
        if me.ra.is_none() {
            if let Poll::Ready(v) = me.a.as_mut().poll(cx) {
                me.ra = Some(v);
            }
        }
        if me.rb.is_none() {
            if let Poll::Ready(v) = me.b.as_mut().poll(cx) {
                me.rb = Some(v);
            }
        }
        match (me.ra.take(), me.rb.take()) {
            (Some(a), Some(b)) => Poll::Ready((a, b)),
            (ra, rb) => {
                me.ra = ra;
                me.rb = rb;
                Poll::Pending
            }
        }
    }
}

fn join2<A: Future, B: Future>(a: A, b: B) -> Join2<A, B> {
    Join2 { a: Box::pin(a), b: Box::pin(b), ra: None, rb: None }
}

fn block_on<F: Future>(f: F) -> (F::Output, u32) {
    let mut f = pin!(f);
    let mut cx = Context::from_waker(Waker::noop());
    let mut polls = 0;
    loop {
        polls += 1;
        if let Poll::Ready(v) = f.as_mut().poll(&mut cx) {
            return (v, polls);
        }
    }
}

async fn tagged(log: Log, name: &'static str, n: u32) -> String {
    let r = Countdown { name, left: n, log: log.clone() }.await;
    format!("{r} done")
}

fn main() {
    let log: Log = Rc::new(RefCell::new(Vec::new()));
    let ((a, b), polls) = block_on(join2(
        Countdown { name: "a", left: 1, log: log.clone() },
        Countdown { name: "b", left: 3, log: log.clone() },
    ));
    println!("{a} {b} polls {polls}");
    println!("{}", log.borrow().join(", "));
    log.borrow_mut().clear();
    let ((x, y), polls) = block_on(join2(tagged(log.clone(), "x", 2), tagged(log.clone(), "y", 0)));
    println!("{x} {y} polls {polls}");
    println!("{}", log.borrow().join(", "));
    let mut futs: Vec<Pin<Box<dyn Future<Output = u32>>>> = Vec::new();
    for i in 1..=3u32 {
        futs.push(Box::pin(async move { i * 100 }));
    }
    let mut cx = Context::from_waker(Waker::noop());
    let mut outs = Vec::new();
    for f in futs.iter_mut() {
        if let Poll::Ready(v) = f.as_mut().poll(&mut cx) {
            outs.push(v);
        }
    }
    println!("{:?}", outs);
}
