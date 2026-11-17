//! x0-async-nest-01 (front2; status 42): three futures held one inside another BY VALUE, each
//! across an `.await`, and a borrow of a local held across one (ASYNC.md P8: states laid out
//! from TYPES before any LIR, in dependency order: `inner`'s, then `middle`'s, which holds
//! `inner`'s, then `outer`'s, which holds `middle`'s).
//!
//! The Reference, "Async blocks": the future is "roughly equivalent to an enum with one variant
//! per await point, where each variant stores the data needed to resume from its corresponding
//! point": `outer` awaits `middle(..)`, so `outer`'s state holds `middle`'s future by value, and
//! so on down. In `middle`, `r = &kept` is held across the `.await`; `kept` itself is not named
//! after it, so only the borrow keeps it saved (blaze-flow suspend.rs, rustc's
//! `locals_live_across_suspend_points`: live OR maybe borrowed). And `f(&g(), x.await)`'s first
//! argument, a temporary evaluated before the `.await`, is held across it (main's ruling: a
//! pre-await temporary is a flow local, saved).
//!
//! middle(6): kept = 6 * 2 = 12, r = &kept, inner(12).await = 13 after one pending, + *r = 25;
//! outer: add(&base(), middle(6).await) = 17 + 25 = 42, in 2 polls.
//!
//! What each wrong build exits with:
//! - 1: not 42 (a saved local lost: `kept` read through `r` from freed stack, the temporary
//!   `base()` not saved, a held state laid too small);
//! - 2: not ready after 100 polls.
//!
//! The run prints: `front2/x0-async-nest-01: PASS (status 42, stdout byte-equal)`.

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll, Waker};

/// Pending once, then ready.
struct Yield(bool);

impl Future for Yield {
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

async fn inner(x: u32) -> u32 {
    Yield(false).await;
    x + 1
}

async fn middle(x: u32) -> u32 {
    let kept = x * 2;
    let r = &kept;
    let got = inner(kept).await;
    got + *r
}

fn base() -> u32 {
    17
}

fn add(a: &u32, b: u32) -> u32 {
    *a + b
}

async fn outer() -> u32 {
    add(&base(), middle(6).await)
}

fn main() {
    let mut cx = Context::from_waker(Waker::noop());
    let mut fut = Box::pin(outer());
    let mut polls = 0u32;
    let got = loop {
        polls += 1;
        if polls > 100 {
            std::process::exit(2);
        }
        if let Poll::Ready(v) = fut.as_mut().poll(&mut cx) {
            break v;
        }
    };
    println!("{got} after {polls} polls");
    if got != 42 {
        std::process::exit(1);
    }
    std::process::exit(42);
}
