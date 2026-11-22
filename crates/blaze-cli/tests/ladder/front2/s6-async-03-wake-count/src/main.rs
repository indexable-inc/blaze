//! s6-async-03: a real waker: `std::task::Wake` on an `Arc` (`Waker::from(Arc<W>)`: the vtable
//! std/alloc/src/task.rs builds), a one-shot channel future that stores the waker and is woken
//! by the sender (once from this thread, once from another), and the waker's clones released by
//! `wake` (which consumes them), so the `Arc` count ends at 2 (`counter` and `waker`).

use std::future::Future;
use std::pin::{Pin, pin};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Wake, Waker};
use std::thread;

struct CountingWaker {
    wakes: AtomicUsize,
}

impl Wake for CountingWaker {
    fn wake(self: Arc<Self>) {
        self.wakes.fetch_add(1, Ordering::SeqCst);
    }
    fn wake_by_ref(self: &Arc<Self>) {
        self.wakes.fetch_add(1, Ordering::SeqCst);
    }
}

struct Shared {
    value: Option<u32>,
    waker: Option<Waker>,
}

struct Receiver(Arc<Mutex<Shared>>);

impl Future for Receiver {
    type Output = u32;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<u32> {
        let mut s = self.0.lock().unwrap();
        match s.value.take() {
            Some(v) => Poll::Ready(v),
            None => {
                s.waker = Some(cx.waker().clone());
                Poll::Pending
            }
        }
    }
}

fn send(shared: &Arc<Mutex<Shared>>, v: u32) {
    let waker = {
        let mut s = shared.lock().unwrap();
        s.value = Some(v);
        s.waker.take()
    };
    if let Some(w) = waker {
        w.wake();
    }
}

fn main() {
    let counter = Arc::new(CountingWaker { wakes: AtomicUsize::new(0) });
    let waker = Waker::from(counter.clone());
    let mut cx = Context::from_waker(&waker);
    let shared = Arc::new(Mutex::new(Shared { value: None, waker: None }));
    let mut fut = pin!(async {
        let r = Receiver(shared.clone()).await;
        r + 1
    });
    println!("{:?}", fut.as_mut().poll(&mut cx));
    println!("wakes {}", counter.wakes.load(Ordering::SeqCst));
    send(&shared, 41);
    println!("wakes {}", counter.wakes.load(Ordering::SeqCst));
    println!("{:?}", fut.as_mut().poll(&mut cx));
    let shared2 = Arc::new(Mutex::new(Shared { value: None, waker: None }));
    let mut fut2 = pin!(Receiver(shared2.clone()));
    println!("{:?}", fut2.as_mut().poll(&mut cx));
    let s2 = shared2.clone();
    thread::spawn(move || send(&s2, 7)).join().unwrap();
    let wakes = counter.wakes.load(Ordering::SeqCst);
    println!("wakes {} {:?}", wakes, fut2.as_mut().poll(&mut cx));
    println!("{}", Arc::strong_count(&counter));
}
