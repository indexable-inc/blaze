//! x1-waker-01 (front2, on the pinned library's real core and std; success 42, else the failed
//! check's number): a hand-written executor drives a future through wakers made from a
//! `RawWakerVTable`, once with the table in a `static` and twice with the table a generic fn
//! returns as `&'static`, as tokio's util/wake.rs:44 `waker_vtable::<W>()` does.
//!
//! The pinned core (library/core/src/task/wake.rs, nightly-2026-10-27): `RawWakerVTable::new`
//! is `#[rustc_promotable]` (line 197) and documents its four functions: `clone` "will be called
//! when the [`RawWaker`] gets cloned, e.g. when the [`Waker`] in which the [`RawWaker`] is stored
//! gets cloned"; `wake` "will be called when `wake` is called on the [`Waker`]"; `wake_by_ref`
//! "will be called when `wake_by_ref` is called on the [`Waker`]" and "must not consume the
//! provided data pointer"; `drop` "will be called when a [`Waker`] gets dropped". `Waker::wake`
//! (line 443) takes `self` and wraps it in `ManuallyDrop::new(self)` before calling the table's
//! `wake`: a woken waker is NOT also dropped.
//!
//! The Reference, "Constant promotion": "Promotion of a value expression to a 'static slot
//! occurs when the expression could be written in a constant and borrowed, and that borrow
//! could be dereferenced where the expression was originally written, without changing the
//! runtime behavior. That is, the promoted expression can be evaluated at compile-time and the
//! resulting value does not contain interior mutability or destructors". So
//! `&RawWakerVTable::new(g_clone::<T>, ..)` in `vtable::<T>()` is the address of a static, one
//! per instance, and stays valid after `vtable` returns.
//!
//! `SelfWaking { left: 2 }` is pending twice and wakes itself each time: at its first poll by
//! `wake_by_ref`, at its second by `clone().wake()` (one clone, one wake, no drop of the clone);
//! the third poll is ready with 7. The executor then drops its own waker (one drop). The static
//! table counts 1 each; `vtable::<One>()` adds 1 each, `vtable::<Ten>()` adds 10 each (two
//! instances, two tables). Between taking a generic table and using it, `scribble` writes zeros
//! over the stack below `main`, where `vtable`'s frame was.
//!
//! What each wrong build exits with:
//! - 1: the static table's counts are not 1 each (a table entry wired to the wrong function, a
//!   woken waker also dropped, a clone not counted);
//! - 2: `vtable::<One>()`'s counts are not 1 each;
//! - 3: `vtable::<Ten>()`'s counts are not 10 each (the two instances share one table, or an
//!   instance's fn pointer names the other instance);
//! - 4: not ready with 7 at the third poll (16 polls without ready, or the wrong value).
//! A build WITHOUT constant promotion (ASYNC.md P10: the borrow made a temporary of `vtable`'s
//! frame) returns a pointer into that dead frame; `scribble` zeroes it and the first
//! `wake_by_ref` through it calls address 0: the run CRASHES. That is the wrong code this rung
//! exists to show.
//!
//! NEEDS: the run path on the pinned library's std (not a `no_core` twin), and P10 (constant
//! promotion). Until they land the rung is REFUSED by name.
//!
//! The run prints: `front2/x1-waker-01: PASS (status 42, stdout byte-equal)`.

use std::future::Future;
use std::hint::black_box;
use std::pin::Pin;
use std::process::ExitCode;
use std::sync::atomic::{AtomicU32, Ordering};
use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

/// What a waker's table was asked to do, counted by weight.
struct Counts {
    clones: AtomicU32,
    wakes: AtomicU32,
    by_ref: AtomicU32,
    drops: AtomicU32,
}

impl Counts {
    const fn new() -> Counts {
        Counts { clones: AtomicU32::new(0), wakes: AtomicU32::new(0), by_ref: AtomicU32::new(0), drops: AtomicU32::new(0) }
    }

    fn each_is(&self, weight: u32) -> bool {
        self.clones.load(Ordering::SeqCst) == weight
            && self.wakes.load(Ordering::SeqCst) == weight
            && self.by_ref.load(Ordering::SeqCst) == weight
            && self.drops.load(Ordering::SeqCst) == weight
    }

    fn print(&self, name: &str, polls: u32) {
        println!(
            "{name}: polls {polls}, clones {}, wakes {}, wake_by_ref {}, drops {}",
            self.clones.load(Ordering::SeqCst),
            self.wakes.load(Ordering::SeqCst),
            self.by_ref.load(Ordering::SeqCst),
            self.drops.load(Ordering::SeqCst),
        );
    }
}

/// # Safety
/// `data` was made from a `&Counts` that outlives every waker holding it (each `Counts` is a
/// local of `main` that outlives the run it is passed to).
unsafe fn counts<'a>(data: *const ()) -> &'a Counts {
    unsafe { &*data.cast::<Counts>() }
}

// The table in a `static`.

unsafe fn s_clone(data: *const ()) -> RawWaker {
    let c = unsafe { counts(data) };
    c.clones.fetch_add(1, Ordering::SeqCst);
    RawWaker::new(data, &STATIC_VTABLE)
}

unsafe fn s_wake(data: *const ()) {
    let c = unsafe { counts(data) };
    c.wakes.fetch_add(1, Ordering::SeqCst);
}

unsafe fn s_wake_by_ref(data: *const ()) {
    let c = unsafe { counts(data) };
    c.by_ref.fetch_add(1, Ordering::SeqCst);
}

unsafe fn s_drop(data: *const ()) {
    let c = unsafe { counts(data) };
    c.drops.fetch_add(1, Ordering::SeqCst);
}

static STATIC_VTABLE: RawWakerVTable = RawWakerVTable::new(s_clone, s_wake, s_wake_by_ref, s_drop);

// The table a generic fn returns, promoted: one per instance.

trait Weight {
    const W: u32;
}

struct One;

impl Weight for One {
    const W: u32 = 1;
}

struct Ten;

impl Weight for Ten {
    const W: u32 = 10;
}

fn vtable<T: Weight>() -> &'static RawWakerVTable {
    &RawWakerVTable::new(g_clone::<T>, g_wake::<T>, g_wake_by_ref::<T>, g_drop::<T>)
}

unsafe fn g_clone<T: Weight>(data: *const ()) -> RawWaker {
    let c = unsafe { counts(data) };
    c.clones.fetch_add(T::W, Ordering::SeqCst);
    RawWaker::new(data, vtable::<T>())
}

unsafe fn g_wake<T: Weight>(data: *const ()) {
    let c = unsafe { counts(data) };
    c.wakes.fetch_add(T::W, Ordering::SeqCst);
}

unsafe fn g_wake_by_ref<T: Weight>(data: *const ()) {
    let c = unsafe { counts(data) };
    c.by_ref.fetch_add(T::W, Ordering::SeqCst);
}

unsafe fn g_drop<T: Weight>(data: *const ()) {
    let c = unsafe { counts(data) };
    c.drops.fetch_add(T::W, Ordering::SeqCst);
}

/// Pending while `left` is not 0, waking itself each time; then ready with 7.
struct SelfWaking {
    left: u32,
}

impl Future for SelfWaking {
    type Output = u32;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<u32> {
        match self.left {
            0 => Poll::Ready(7),
            1 => {
                self.left = 0;
                cx.waker().clone().wake();
                Poll::Pending
            }
            _ => {
                self.left -= 1;
                cx.waker().wake_by_ref();
                Poll::Pending
            }
        }
    }
}

/// Polls a fresh `SelfWaking { left: 2 }` with `waker` until ready (at most 16 polls); the value
/// and the number of polls. `waker` is dropped when this returns.
fn block_on(waker: Waker) -> Option<(u32, u32)> {
    let mut fut = SelfWaking { left: 2 };
    let mut cx = Context::from_waker(&waker);
    let mut polls = 0u32;
    while polls < 16 {
        polls += 1;
        if let Poll::Ready(value) = Pin::new(&mut fut).poll(&mut cx) {
            return Some((value, polls));
        }
    }
    None
}

/// Overwrites the stack below the caller with zeros: what a dead frame held is gone.
#[inline(never)]
fn scribble() -> u64 {
    let wipe = black_box([0u64; 64]);
    black_box(&wipe);
    wipe[63]
}

/// One run through `table` with `counts`; `Err(code)` names the failed check.
fn run(name: &str, counts: &Counts, table: &'static RawWakerVTable, weight: u32, code: u8) -> Result<(), u8> {
    let data = (counts as *const Counts).cast::<()>();
    // SAFETY: `table`'s four functions take a `data` made from a `&Counts` that outlives the
    // waker (see `counts`), and none of them frees it.
    let waker = unsafe { Waker::from_raw(RawWaker::new(data, table)) };
    let Some((value, polls)) = block_on(waker) else {
        return Err(4);
    };
    counts.print(name, polls);
    if value != 7 || polls != 3 {
        return Err(4);
    }
    if !counts.each_is(weight) {
        return Err(code);
    }
    Ok(())
}

fn main() -> ExitCode {
    let in_static = Counts::new();
    if let Err(code) = run("static", &in_static, &STATIC_VTABLE, 1, 1) {
        return ExitCode::from(code);
    }

    let one = Counts::new();
    let table = vtable::<One>();
    black_box(scribble());
    if let Err(code) = run("generic One", &one, table, 1, 2) {
        return ExitCode::from(code);
    }

    let ten = Counts::new();
    let table = vtable::<Ten>();
    black_box(scribble());
    if let Err(code) = run("generic Ten", &ten, table, 10, 3) {
        return ExitCode::from(code);
    }

    ExitCode::from(42)
}
