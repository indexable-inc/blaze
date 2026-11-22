//! x1-poll-panic-01 (front2, on the pinned library's real core and std, `panic = "unwind"`;
//! success 42, else the failed check's number): an `async fn`'s future panics inside a poll
//! while it holds a `Drop` value, the panic is caught by `catch_unwind` around the poll (as
//! tokio's task harness polls every task), and the future is polled once more and dropped.
//!
//! The Reference, "Panic": with the `unwind` strategy, "as the panic traverses Rust frames, live
//! objects in those frames that implement Drop will have their drop methods called". The pinned
//! std (library/std/src/panic.rs, nightly-2026-10-27) on `catch_unwind`: "This function will
//! return `Ok` with the closure's result if the closure does not panic, and will return
//! `Err(cause)` if the closure panics. The `cause` returned is the object with which panic was
//! originally invoked." A panic whose message is a plain string carries it as a `&'static str`
//! (library/std/src/panicking.rs line 677: `if let Some(s) = msg.as_str()` raises a
//! `StaticStrPayload(s)`). The pinned core (library/core/src/panicking.rs line 203):
//! `panic_const_async_fn_resumed_panic = "`async fn` resumed after panicking"`: the panic an
//! `async fn`'s future raises when it is polled again after a poll of it panicked (ASYNC.md
//! D-A5: the split stores the tag `PANICKED` before each `resume` reached from the body, `poll`
//! at `PANICKED` calls that lang fn, and `drop` at `PANICKED` does nothing, since the unwind
//! already dropped what was live).
//!
//! Each drop appends its id as a decimal digit: log = log * 10 + id. `boom` holds `held` (7)
//! across one await of a future that is pending once, then panics with "boom in poll".
//! 1. poll 1: pending; nothing dropped: log 0.
//! 2. poll 2, inside `catch_unwind`: panics; the unwind drops `held` once: caught, the payload
//!    is "boom in poll", log 7.
//! 3. poll 3, inside `catch_unwind`: panics with "`async fn` resumed after panicking": caught,
//!    log still 7 (the body is not run again, nothing is dropped again).
//! 4. drop of the future: log still 7.
//! The panic hook is replaced by one that prints nothing, so standard error stays empty.
//!
//! What each wrong build exits with:
//! - 1: poll 1 not pending, or something dropped by it;
//! - 2: poll 2 did not panic with "boom in poll" (or did not come back to `main` at all: the
//!   process aborts, or the run is CRASHED);
//! - 3: the log after poll 2 not 7 (0: `held` not dropped by the unwind; 77: dropped twice);
//! - 4: poll 3 did not panic with "`async fn` resumed after panicking" (a poll that runs the
//!   body again from its saved state, ready, pending, or a trap in place of the library's panic);
//! - 5: the log after poll 3 or after the drop not 7 (the future's drop at `PANICKED` dropped
//!   `held` again: 77).
//!
//! NEEDS: the run path on the pinned library's std with `panic = "unwind"` (not a `no_core`
//! twin), and P5 (the LIR split takes landing blocks: a panic inside a poll; the library's two
//! panics in place of the traps). Until then the rung is REFUSED by name ("unwinding is not
//! built", crates/blaze-lir/src/coroutine.rs).
//!
//! The run prints: `front2/x1-poll-panic-01: PASS (status 42, stdout byte-equal)`.

use std::any::Any;
use std::cell::Cell;
use std::future::Future;
use std::hint::black_box;
use std::panic::{self, AssertUnwindSafe};
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

/// Pending `left` times, then ready.
struct Wait {
    left: u32,
}

impl Future for Wait {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<()> {
        if self.left == 0 {
            Poll::Ready(())
        } else {
            self.left -= 1;
            Poll::Pending
        }
    }
}

async fn boom(log: &Cell<u32>) -> u32 {
    let held = Noisy { id: 7, log };
    Wait { left: 1 }.await;
    if black_box(true) {
        panic!("boom in poll");
    }
    held.id
}

unsafe fn noop_clone(data: *const ()) -> RawWaker {
    RawWaker::new(data, &NOOP_VTABLE)
}

unsafe fn noop(_data: *const ()) {}

static NOOP_VTABLE: RawWakerVTable = RawWakerVTable::new(noop_clone, noop, noop, noop);

/// The panic's message when it is a string; `None` for any other payload.
fn message(payload: &(dyn Any + Send)) -> Option<&str> {
    if let Some(s) = payload.downcast_ref::<&'static str>() {
        return Some(*s);
    }
    payload.downcast_ref::<String>().map(String::as_str)
}

/// What a caught poll did, for the printed line.
fn told(caught: &Result<Poll<u32>, Box<dyn Any + Send>>) -> String {
    match caught {
        Ok(Poll::Pending) => "pending".to_string(),
        Ok(Poll::Ready(v)) => format!("ready with {v}"),
        Err(payload) => match message(&**payload) {
            Some(text) => format!("panicked with \"{text}\""),
            None => "panicked with a payload that is not a string".to_string(),
        },
    }
}

fn panicked_with(caught: &Result<Poll<u32>, Box<dyn Any + Send>>, want: &str) -> bool {
    match caught {
        Err(payload) => message(&**payload) == Some(want),
        Ok(_) => false,
    }
}

fn main() -> ExitCode {
    panic::set_hook(Box::new(|_| {}));
    let log = Cell::new(0u32);
    // SAFETY: the table's functions never read `data`.
    let waker = unsafe { Waker::from_raw(RawWaker::new(std::ptr::null(), &NOOP_VTABLE)) };
    let mut cx = Context::from_waker(&waker);

    let mut fut = Box::pin(boom(&log));

    let first = fut.as_mut().poll(&mut cx);
    let after_first = log.get();
    println!("poll 1: {}, log {after_first}", if first.is_pending() { "pending" } else { "ready" });
    if first.is_ready() || after_first != 0 {
        return ExitCode::from(1);
    }

    let second = panic::catch_unwind(AssertUnwindSafe(|| fut.as_mut().poll(&mut cx)));
    let after_second = log.get();
    println!("poll 2: {}, log {after_second}", told(&second));
    if !panicked_with(&second, "boom in poll") {
        return ExitCode::from(2);
    }
    if after_second != 7 {
        return ExitCode::from(3);
    }

    let third = panic::catch_unwind(AssertUnwindSafe(|| fut.as_mut().poll(&mut cx)));
    let after_third = log.get();
    println!("poll 3: {}, log {after_third}", told(&third));
    if !panicked_with(&third, "`async fn` resumed after panicking") {
        return ExitCode::from(4);
    }
    if after_third != 7 {
        return ExitCode::from(5);
    }

    drop(fut);
    let after_drop = log.get();
    println!("dropped: log {after_drop}");
    if after_drop != 7 {
        return ExitCode::from(5);
    }
    ExitCode::from(42)
}
