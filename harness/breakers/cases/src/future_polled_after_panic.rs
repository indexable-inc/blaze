//! A future polled again after it panicked panics with "`async fn` resumed after panicking",
//! as rustc's coroutine transform has it. ra-jit marked a future as panicked only where the call
//! that unwound had something to clean up, so this one, which panics with nothing left in
//! scope, was resumed at its last await: it printed `name` again after freeing it, and freed it
//! twice.

use std::cell::RefCell;
use std::future::Future;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::pin::Pin;
use std::task::{Context, Poll, Waker};

thread_local! {
    static LOG: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
}

fn log(said: String) {
    LOG.with(|it| it.borrow_mut().push(said));
}

/// Pending once, then ready.
struct Once(bool);

impl Future for Once {
    type Output = ();
    fn poll(mut self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<()> {
        if std::mem::replace(&mut self.0, true) { Poll::Ready(()) } else { Poll::Pending }
    }
}

fn fails() -> bool {
    std::hint::black_box(true)
}

pub fn run() -> Vec<String> {
    let mut future = Box::pin(async {
        {
            let name = String::from("held");
            Once(false).await;
            log(format!("{name} after the await"));
        }
        if fails() {
            panic!("after the block");
        }
    });
    let mut cx = Context::from_waker(Waker::noop());
    for _ in 0..3 {
        let polled = catch_unwind(AssertUnwindSafe(|| future.as_mut().poll(&mut cx)));
        log(match &polled {
            Ok(Poll::Ready(())) => "ready".to_string(),
            Ok(Poll::Pending) => "pending".to_string(),
            Err(payload) => payload.downcast_ref::<&str>().copied().unwrap_or("?").to_string(),
        });
    }
    drop(future);
    LOG.with(|it| it.take())
}
