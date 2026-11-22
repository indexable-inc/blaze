//! x0-async-01 (front2, no_core; success 42, else the failed check's number): an `async fn`
//! awaiting another, driven by an executor written here, through the whole real path: the
//! front end's head and check, the builder's coroutine, the LIR pass's split into poll and
//! drop, the emitted code.
//!
//! The Reference, "Async functions": `async fn f(x: A) -> T { body }` "is roughly equivalent
//! to" `fn f(x: A) -> impl Future<Output = T> { async move { body } }`: calling one runs none
//! of its body and returns a future that holds its arguments. "Await expressions": "Create a
//! future by calling `IntoFuture::into_future` on the future operand." "This pinned future is
//! then polled by calling the `Future::poll` method and passing it the current task context"
//! "If the call to `poll` returns `Poll::Pending`, then the future returns `Poll::Pending`,
//! suspending its state so that, when the surrounding async context is re-polled, execution
//! returns to step 3" "Otherwise the call to `poll` must have returned `Poll::Ready`, in which
//! case the value contained in the `Poll::Ready` variant is used as the result of the await
//! expression itself."
//!
//! `Twice` is pending twice, then ready with its value, and counts a wake each time it is
//! pending. `add_slowly(13, 10)` awaits two of them: its first poll and its second are
//! pending in the first, its third takes 13 from the first and is pending in the second, its
//! fourth is pending, its fifth takes 10 and returns 23. `compute(13)` keeps `kept = 14`
//! across its one `.await` (of `add_slowly`'s future, held by value in its own state) and
//! returns 23 + 14 = 37. So `block_on` polls 5 times, 4 of them pending (4 wakes), and gets 37.
//!
//! What each wrong build exits with:
//! - 1: not 5 polls (a body that never suspends is ready at the first: 1 poll);
//! - 2: 5 polls but not 37 (a value not kept across a suspend: `b`, `x` or `kept`; an argument
//!   not moved into the future);
//! - 3: not ready after 64 polls (a poll that starts the body again instead of resuming: the
//!   first `Twice` is made anew each time and is never ready);
//! - 4: not 4 wakes (the task context not passed down to the awaited future's `poll`).
//!
//! The run prints: `front2/x0-async-01: PASS (status 42, stdout byte-equal)`.
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

async fn add_slowly(a: u32, b: u32) -> u32 {
    let x = Twice { left: 2, value: a }.await;
    let y = Twice { left: 2, value: b }.await;
    x + y
}

async fn compute(seed: u32) -> u32 {
    let kept = seed + 1;
    let sum = add_slowly(seed, 10).await;
    sum + kept
}

/// The executor: polls `fut` until it is ready, counting the polls in `polls`; 1000 when it
/// is not ready after 64.
fn block_on<F: Future<Output = u32>>(fut: F, polls: &mut u32, wakes: &mut u32) -> u32 {
    let mut fut = fut;
    let mut cx = Context { wakes };
    loop {
        if *polls == 64 {
            return 1000;
        }
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
    let got = block_on(compute(13), &mut polls, &mut wakes);
    if got == 1000 {
        3
    } else if polls != 5 {
        1
    } else if wakes != 4 {
        4
    } else if got != 37 {
        2
    } else {
        42
    }
}
