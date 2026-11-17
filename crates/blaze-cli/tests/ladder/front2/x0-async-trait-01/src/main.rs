//! x0-async-trait-01 (front2, no_core; success 42, else the failed check's number): an `async
//! fn` declared in a trait, implemented once by an `async fn` and once by a fn returning a
//! named future type through `impl Future`, awaited through a generic, driven by an executor
//! written here.
//!
//! The Reference, "Async functions": `async fn f(x: A) -> T { body }` "is roughly equivalent
//! to" `fn f(x: A) -> impl Future<Output = T> { async move { body } }`. "Impl trait",
//! "Abstract return types": the caller of a fn returning `impl Trait` "can only use the
//! methods declared by the trait bounds", so `sum::<S>` sees `S::get`'s future only as `impl
//! Future<Output = u32>`; each instance of `sum` runs the future of ITS impl: the opaque type of
//! the trait fn has one value per impl, the type the impl fn's own return type gives it (an
//! `async fn`'s future for `Slow`, `Ready` for `Now`).
//!
//! `Slow { value: 33 }.get()` is pending once (its `Twice` with `left: 1`), then 33; `sum`
//! adds 1: 34, in 2 polls with 1 wake. `Now(7).get()` is `Ready(7)`, ready at its first poll;
//! `sum` adds 1: 8, in 1 poll. 34 + 8 = 42.
//!
//! What each wrong build exits with:
//! - 1: `sum(&Slow)` is not 34 (the opaque at `Slow` taken as another impl's future, or the
//!   value not kept across the suspend);
//! - 2: `sum(&Now)` is not 8 (the opaque at `Now` not `Ready`: its poll is another's);
//! - 3: a future not ready after 64 polls;
//! - 4: not 3 polls and 1 wake in all (the task context not passed down, or a future polled
//!   anew instead of resumed).
//!
//! The run prints: `front2/x0-async-trait-01: PASS (status 42, stdout byte-equal)`.
#![feature(no_core, lang_items, intrinsics, rustc_attrs, unboxed_closures, arbitrary_self_types)]
#![no_core]
#![no_main]
#![allow(internal_features)]

mod future;
mod prelude;
use future::*;
use prelude::*;

/// Pending `left` more times, then ready with `value`.
struct Twice {
    left: u32,
    value: u32,
}

impl Future for Twice {
    type Output = u32;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<u32> {
        let this = self.pointer;
        if this.left == 0 {
            Poll::Ready(this.value)
        } else {
            this.left = this.left - 1;
            *cx.wakes = *cx.wakes + 1;
            Poll::Pending
        }
    }
}

/// Ready with its value at the first poll.
struct Ready(u32);

impl Future for Ready {
    type Output = u32;

    fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<u32> {
        let this = self.pointer;
        Poll::Ready(this.0)
    }
}

trait Source {
    async fn get(&self) -> u32;
}

struct Slow {
    value: u32,
}

impl Source for Slow {
    async fn get(&self) -> u32 {
        Twice { left: 1, value: self.value }.await
    }
}

struct Now(u32);

impl Source for Now {
    fn get(&self) -> impl Future<Output = u32> {
        Ready(self.0)
    }
}

async fn sum<S: Source>(s: &S) -> u32 {
    s.get().await + 1
}

/// The executor: polls `fut` until it is ready, counting the polls in `polls`; 1000 when it
/// is not ready after 64.
fn block_on<F: Future<Output = u32>>(fut: F, polls: &mut u32, wakes: &mut u32) -> u32 {
    let mut fut = fut;
    let mut cx = Context { wakes };
    let mut n = 0u32;
    loop {
        if n == 64 {
            return 1000;
        }
        n = n + 1;
        *polls = *polls + 1;
        // The future is a local that is not moved again: pinned where it stands.
        let pinned = Pin { pointer: &mut fut };
        match <F as Future>::poll(pinned, &mut cx) {
            Poll::Ready(v) => return v,
            Poll::Pending => {}
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    let mut polls = 0u32;
    let mut wakes = 0u32;
    let slow = Slow { value: 33 };
    let now = Now(7);
    let a = block_on(sum(&slow), &mut polls, &mut wakes);
    let b = block_on(sum(&now), &mut polls, &mut wakes);
    if a == 1000 || b == 1000 {
        3
    } else if a != 34 {
        1
    } else if b != 8 {
        2
    } else if polls != 3 || wakes != 1 {
        4
    } else {
        42
    }
}
