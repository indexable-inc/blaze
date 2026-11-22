//! x1-cancel-01 (front2, on the pinned library's real core and std; success 42, else the failed
//! check's number): an `async fn`'s future, suspended at its SECOND await while it holds two
//! `Drop` values and the awaited future, is dropped without being polled again: what tokio does
//! to every task it aborts and every `select!` branch that loses.
//!
//! The Reference, "Await expressions": `.await` is `match operand.into_future() { mut pinned =>
//! loop { .. match Pin::new_unchecked(&mut pinned).poll(cx) { Poll::Ready(r) => break r,
//! Poll::Pending => yield () } } }` (the binding `pinned` is the innermost thing alive at the
//! suspension, and is dropped when the `match` ends, that is when the await completes). The
//! Reference, "Destructors", "Drop scopes": "When control flow leaves a drop scope all variables
//! associated to that scope are dropped in reverse order of declaration (for variables) or
//! creation (for temporaries)." Dropping a suspended future leaves every scope open at its
//! suspension, as a `return` written there would (ASYNC.md D-A1: "The cancel successor is the
//! drop path", "cancel = the drops a `return` there makes"): the awaited future first, then the
//! locals in reverse order of declaration.
//!
//! Each drop appends its id as a decimal digit: log = log * 10 + id. `two_awaits` declares
//! `a` (1), awaits a future holding 4 that is pending once, declares `b` (2), awaits a future
//! holding 3 that is never ready.
//! 1. poll 1: suspended at the first await; nothing dropped: log 0.
//! 2. poll 2: the first await completes, its future (4) is dropped at the end of its `match`;
//!    `b` is made; suspended at the second await: log 4.
//! 3. drop: the second await's future (3), then `b` (2), then `a` (1): log 4321. The body
//!    never runs past its second await (`finished` stays false).
//!
//! What each wrong build exits with:
//! - 1: poll 1 not pending, or something dropped by it;
//! - 2: poll 2 not pending, or its log not 4 (the first await's future not dropped when that
//!   await completed, or dropped twice);
//! - 3: the body ran past its second await;
//! - 4: the log after the drop not 4321 (a value held across the suspension not dropped:
//!   43 or 432; dropped in declaration order: 4123 or 4213; dropped twice; the awaited
//!   future dropped after the locals: 4213).
//!
//! The waker is this program's own no-op through a `static` table, so the rung needs no
//! promotion. NEEDS: the run path on the pinned library's std (not a `no_core` twin) and E2's
//! cancel path (the LIR split's `drop` at a suspended state). Until then it is REFUSED by name.
//!
//! The run prints: `front2/x1-cancel-01: PASS (status 42, stdout byte-equal)`.

use std::cell::Cell;
use std::future::Future;
use std::pin::Pin;
use std::process::ExitCode;
use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

struct Noisy<'a> {
    id: u32,
    log: &'a Cell<u32>,
}

impl Drop for Noisy<'_> {
    fn drop(&mut self) {
        self.log.set(self.log.get() * 10 + self.id);
    }
}

/// Pending `left` times, then ready; holds `held` until it is dropped.
struct Wait<'a> {
    left: u32,
    held: Noisy<'a>,
}

impl Future for Wait<'_> {
    type Output = u32;

    fn poll(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<u32> {
        if self.left == 0 {
            Poll::Ready(self.held.id)
        } else {
            self.left -= 1;
            Poll::Pending
        }
    }
}

async fn two_awaits(log: &Cell<u32>, finished: &Cell<bool>) -> u32 {
    let a = Noisy { id: 1, log };
    let first = Wait { left: 1, held: Noisy { id: 4, log } }.await;
    let b = Noisy { id: 2, log };
    let second = Wait { left: u32::MAX, held: Noisy { id: 3, log } }.await;
    finished.set(true);
    a.id + b.id + first + second
}

unsafe fn noop_clone(data: *const ()) -> RawWaker {
    RawWaker::new(data, &NOOP_VTABLE)
}

unsafe fn noop(_data: *const ()) {}

static NOOP_VTABLE: RawWakerVTable = RawWakerVTable::new(noop_clone, noop, noop, noop);

fn main() -> ExitCode {
    let log = Cell::new(0u32);
    let finished = Cell::new(false);
    // SAFETY: the table's functions never read `data`.
    let waker = unsafe { Waker::from_raw(RawWaker::new(std::ptr::null(), &NOOP_VTABLE)) };
    let mut cx = Context::from_waker(&waker);

    let mut fut = Box::pin(two_awaits(&log, &finished));

    let first = fut.as_mut().poll(&mut cx);
    let after_first = log.get();
    println!("poll 1: {}, log {after_first}", if first.is_pending() { "pending" } else { "ready" });
    if first.is_ready() || after_first != 0 {
        return ExitCode::from(1);
    }

    let second = fut.as_mut().poll(&mut cx);
    let after_second = log.get();
    println!("poll 2: {}, log {after_second}", if second.is_pending() { "pending" } else { "ready" });
    if second.is_ready() || after_second != 4 {
        return ExitCode::from(2);
    }

    drop(fut);
    let after_drop = log.get();
    println!("dropped: log {after_drop}");
    if finished.get() {
        return ExitCode::from(3);
    }
    if after_drop != 4321 {
        return ExitCode::from(4);
    }
    ExitCode::from(42)
}
