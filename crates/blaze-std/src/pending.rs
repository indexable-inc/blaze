//! An op in flight (docs/v2/plan/design/std.md, "Tasks and waiting"): [`Pending`], what
//! [`crate::fs::File::read_into`], [`crate::fs::File::write_from`],
//! [`crate::net::Conn::read_into`] and [`crate::net::Conn::write_from`] return. The op is
//! handed to the driver when the call returns; [`Pending::wait`] waits for its outcome and
//! [`Pending::cancel`] gives it up, waiting for its end until a deadline its caller names. Both consume it, and both end only once the driver is done
//! with the op, so nothing it does (a write landing, bytes read) happens after they return.
//! Each blocking call is its op waited for at once: `file.read_into(buf, at).wait(cx)`.
//!
//! **Linear.** A `Pending` is `#[blaze::linear]`: leaving one unfinished is a compile error
//! (crates/hir-ty-linear), because the op it names is still running and its buffer is lent to
//! it. A panic that unwinds past one runs its abandonment path, its `Drop`: it waits, heeding
//! no cancellation, up to [`crate::io::Limits::abandon_wait`] on the driver's clock for the op to end, then
//! records in the trace how that wait ended (`pending: abandoned, ended` or `pending:
//! abandoned, still in flight`).
//!
//! **What S1 lacks.** blaze-core's effects own their buffers (`Effect::Read`'s bytes come back
//! in its `Done::Read`); its `ReadInto`/`WriteFrom`, which read into and write from a
//! registered span of the caller's memory, need an `unsafe` registration and a process-wide
//! slot table, which this crate does not have (`#![deny(unsafe_code)]`, nothing global). So a
//! `Pending` copies: a write's bytes when it starts, a read's into the lent buffer when it is
//! waited for. And blaze-core has no effect that cancels another, so [`Pending::cancel`]
//! cannot ask the driver to stop the op: it waits for it to end, which is why it never
//! returns before the op is done.

use alloc::vec::Vec;
use core::{fmt, marker::PhantomData};

use blaze_core::{Done, Effect, Tag, wait::Deadline};

use crate::{
    Plant, Stopped,
    cx::Cx,
    io::{Ends, Io},
    trace::RecordKind,
    unfinished::Unfinished,
};

/// How a [`Pending`] turns its driver completion into its caller's outcome, inline with zero
/// heap allocation (no `Box<dyn FnOnce>` per op in flight).
pub trait Complete<'buf> {
    type Output;
    fn complete(self, into: &'buf mut [u8], done: Result<Done, Stopped>) -> Self::Output;
}

/// An op in flight, borrowing its buffer for as long as it runs: finished by
/// [`Pending::wait`] or [`Pending::cancel`] (see the module's doc for why it is linear and
/// what a panic that drops it does).
#[blaze::linear]
#[must_use = "an op in flight is finished with wait(cx) or cancel(cx)"]
pub struct Pending<'buf, C: Complete<'buf>> {
    /// Where a read's bytes go; empty for a write.
    into: &'buf mut [u8],
    /// A write's bytes stay lent until it ends, so the caller cannot change them meanwhile.
    from: PhantomData<&'buf [u8]>,
    finish: C,
    settle: Settle,
    unfinished: Unfinished,
}

/// The op's tag and its abandonment path.
struct Settle {
    io: Io,
    /// The op's tag; `None` for an op that was refused before it started ([`refused`]), which
    /// has nothing in flight to wait for.
    tag: Option<Tag>,
    span: &'static str,
    /// Whether the op is still this value's to end: cleared by `wait` and `cancel`.
    armed: bool,
}

impl Drop for Settle {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let Some(tag) = self.tag else { return };
        let until = Deadline::after(self.io.now(), self.io.limits().abandon_wait);
        let what = match self.io.settle(tag, until, self.span) {
            Ok(Done::Read(..) | Done::ReadInto { .. } | Done::ReadMany(..) | Done::Appended | Done::Synced | Done::Ordered | Done::WrittenBack | Done::Prewarmed { .. } | Done::Truncated | Done::Written | Done::Listed(..) | Done::Renamed | Done::Moded | Done::Private(..) | Done::Timed | Done::Cloned | Done::Copied { .. } | Done::Linked | Done::Stat(..) | Done::Removed | Done::Locked | Done::Contended | Done::Unlocked | Done::Changed { .. } | Done::Journaled(..) | Done::Stamped(..) | Done::Entries(..) | Done::Unwatched | Done::Pressed { .. } | Done::Flushed | Done::Fired | Done::Mapped(..) | Done::MappedWrite(..) | Done::Exited(..) | Done::Accepted { .. } | Done::Connected { .. } | Done::Unlistened | Done::Sent(..) | Done::HungUp | Done::Broken { .. } | Done::Bound { .. } | Done::Datagrams(..) | Done::SentDatagrams(..) | Done::DatagramsFailed { .. } | Done::Unbound | Done::Resolved(..) | Done::Unresolved(..) | Done::Wall(..) | Done::Random(..) | Done::Roots(..) | Done::Untrusted(..) | Done::MadeDir | Done::RemovedDir | Done::Ancestors(..) | Done::Failed(..)) => "pending: abandoned, ended",
            Err(Stopped::Deadline { .. } | Stopped::Cancelled { .. } | Stopped::OutOfMemory { .. } | Stopped::Stalled { .. }) => "pending: abandoned, still in flight",
        };
        self.io.record_trace(self.span, RecordKind::Event(what));
    }
}

/// `effect` handed to the driver now, its outcome made inline by `finish` once it ends
/// (zero heap allocation).
#[track_caller]
pub(crate) fn start<'buf, C: Complete<'buf>>(
    io: &Io,
    span: &'static str,
    into: &'buf mut [u8],
    effect: impl FnOnce(Tag) -> Effect,
    finish: C,
) -> Pending<'buf, C> {
    let tag = io.submit(effect);
    Pending {
        into,
        from: PhantomData,
        finish,
        settle: Settle { io: io.clone(), tag: Some(tag), span, armed: true },
        unfinished: Unfinished::new("Pending", "wait(cx) or cancel(cx, give_up)"),
    }
}

/// An op refused before it started (a write past its capability's budget): nothing is handed
/// to the driver, and waiting for it or cancelling it gives `finish`'s outcome at once
/// (zero heap allocation).
#[track_caller]
pub(crate) fn refused<'buf, C: Complete<'buf>>(io: &Io, span: &'static str, finish: C) -> Pending<'buf, C> {
    Pending {
        into: &mut [],
        from: PhantomData,
        finish,
        settle: Settle { io: io.clone(), tag: None, span, armed: true },
        unfinished: Unfinished::new("Pending", "wait(cx) or cancel(cx, give_up)"),
    }
}

impl<'buf, C: Complete<'buf>> Pending<'buf, C> {
    /// The op's outcome, waited for under `cx`: until its deadline, ending early on its
    /// cancellation (after which the op's end is still waited for, as [`Pending::cancel`]
    /// does, so nothing it does lands later).
    #[blaze::consumes]
    pub fn wait(self, cx: &Cx) -> C::Output {
        let Pending { into, from: _, finish, mut settle, unfinished } = self;
        unfinished.finished();
        settle.armed = false;
        let done = match settle.tag {
            Some(tag) => cx.wait(&settle.io, tag, Ends::Once),
            // Refused before it started: its outcome is made already and reads no completion.
            None => Err(Stopped::Cancelled { span: settle.span }),
        };
        finish.complete(into, done)
    }

    /// The op given up: blaze-core cannot cancel an effect in flight, so this waits, heeding
    /// no cancellation, for the op to end, until `give_up`: the caller's bound on a wait that
    /// by design outlasts a cancelled or expired `cx` (it was once the later of `cx`'s
    /// deadline and [`crate::io::Limits::abandon_wait`] from now, picked here). Its outcome is what the op did:
    /// done, or why the wait stopped (then the op may still be in flight, and the stop says
    /// so). `cx` names the span the wait is recorded in.
    #[blaze::consumes]
    pub fn cancel(self, cx: &Cx, give_up: Deadline) -> C::Output {
        let Pending { into, from: _, finish, mut settle, unfinished } = self;
        unfinished.finished();
        settle.armed = false;
        let Some(tag) = settle.tag else {
            // Refused before it started: nothing is in flight to wait for.
            return finish.complete(into, Err(Stopped::Cancelled { span: cx.name() }));
        };
        if cx.plant() == Plant::CancelSkipsWait {
            settle.io.forget(tag, Ends::Once);
            return finish.complete(into, Err(Stopped::Cancelled { span: cx.name() }));
        }
        let done = settle.io.settle(tag, give_up, cx.name());
        finish.complete(into, done)
    }

    /// Non-blocking completion check: if the driver already has this op's completion ready,
    /// consumes `self` and returns `Ok(output)`; otherwise returns `Err(self)` still armed.
    #[blaze::consumes]
    pub fn try_wait(self) -> Result<C::Output, Self> {
        let Pending { into, from, finish, mut settle, unfinished } = self;
        match settle.tag {
            None => {
                unfinished.finished();
                settle.armed = false;
                Ok(finish.complete(into, Err(Stopped::Cancelled { span: settle.span })))
            }
            Some(tag) => match settle.io.take(tag) {
                Some(done) => {
                    unfinished.finished();
                    settle.armed = false;
                    Ok(finish.complete(into, Ok(done)))
                }
                None => Err(Pending { into, from, finish, settle, unfinished }),
            },
        }
    }

    fn finish_with(self, done: Result<Done, Stopped>) -> C::Output {
        let Pending { into, from: _, finish, mut settle, unfinished } = self;
        unfinished.finished();
        settle.armed = false;
        finish.complete(into, done)
    }
}

/// Which of two concurrent [`Pending`] operations finished first in [`select2`].
#[derive(Debug)]
pub enum Either<A, B> {
    First(A),
    Second(B),
}

/// Linear 2-way I/O multiplex: waits in a single driver poll loop until either `first` or
/// `second` completes, completing the winner inline and returning the other still-in-flight
/// linear [`Pending`] intact so the caller can wait or cancel it.
///
/// If `cx` stops (deadline, cancellation, stall) before either completes, returns
/// `Err((stopped, first, second))` with BOTH linear ops intact (neither abandoned).
pub fn select2<'a, 'b, A: Complete<'a>, B: Complete<'b>>(
    cx: &Cx,
    first: Pending<'a, A>,
    second: Pending<'b, B>,
) -> Result<Either<(A::Output, Pending<'b, B>), (Pending<'a, A>, B::Output)>, (Stopped, Pending<'a, A>, Pending<'b, B>)> {
    let Some(tag_a) = first.settle.tag else {
        let span = first.settle.span;
        return Ok(Either::First((first.finish_with(Err(Stopped::Cancelled { span })), second)));
    };
    let Some(tag_b) = second.settle.tag else {
        let span = second.settle.span;
        return Ok(Either::Second((first, second.finish_with(Err(Stopped::Cancelled { span })))));
    };
    let io = first.settle.io.clone();
    match cx.wait_any(&io, &[tag_a, tag_b]) {
        Ok((0, done)) => Ok(Either::First((first.finish_with(Ok(done)), second))),
        Ok((_, done)) => Ok(Either::Second((first, second.finish_with(Ok(done))))),
        Err(stopped) => Err((stopped, first, second)),
    }
}

/// Drives two concurrent [`Pending`] operations to completion in whichever order they land,
/// with zero heap allocation and without spawning a task.
pub fn wait_both<'a, 'b, A: Complete<'a>, B: Complete<'b>>(
    cx: &Cx,
    first: Pending<'a, A>,
    second: Pending<'b, B>,
) -> (A::Output, B::Output) {
    match select2(cx, first, second) {
        Ok(Either::First((a, second))) => (a, second.wait(cx)),
        Ok(Either::Second((first, b))) => (first.wait(cx), b),
        Err((_stopped, first, second)) => (first.wait(cx), second.wait(cx)),
    }
}

struct Slot<'buf, C: Complete<'buf>> {
    id: usize,
    into: &'buf mut [u8],
    from: PhantomData<&'buf [u8]>,
    finish: C,
    tag: Option<Tag>,
    span: &'static str,
}

/// Abandonment guard for [`WaitSet`]: settles any tags still in flight on unwind.
struct SettleMany<'f> {
    io: Io,
    tags: Vec<Tag, &'f blaze_mem::Frame>,
    tag_to_slot: Vec<usize, &'f blaze_mem::Frame>,
}

impl Drop for SettleMany<'_> {
    fn drop(&mut self) {
        if self.tags.is_empty() {
            return;
        }
        let until = Deadline::after(self.io.now(), self.io.limits().abandon_wait);
        for &tag in &self.tags {
            let _ = self.io.settle(tag, until, "bstd::pending::WaitSet");
        }
        self.tags.clear();
        self.tag_to_slot.clear();
    }
}

/// Linear N-way I/O multiplexer for homogeneous [`Pending<'buf, C>`] operations (e.g. N
/// concurrent file reads or socket receives), allocated explicitly in `cx.frame()` (zero
/// `Global` heap allocations, zero `Box<dyn FnOnce>`, `#![deny(unsafe_code)]`, `#[blaze::linear]`).
#[blaze::linear]
#[must_use = "a WaitSet is finished with drain(cx, on_done) or cancel_all(cx, give_up)"]
pub struct WaitSet<'f, 'buf, C: Complete<'buf>> {
    slots: Vec<Slot<'buf, C>, &'f blaze_mem::Frame>,
    settle: SettleMany<'f>,
    unfinished: Unfinished,
}

impl<'f, 'buf, C: Complete<'buf>> WaitSet<'f, 'buf, C> {
    /// Creates an empty linear [`WaitSet`] backed explicitly by `cx.frame()`.
    #[track_caller]
    pub fn new(cx: &'f Cx) -> WaitSet<'f, 'buf, C> {
        let frame = cx.frame();
        WaitSet {
            slots: Vec::new_in(frame),
            settle: SettleMany {
                io: cx.io().clone(),
                tags: Vec::new_in(frame),
                tag_to_slot: Vec::new_in(frame),
            },
            unfinished: Unfinished::new("pending::WaitSet", "drain(cx, on_done) or cancel_all(cx, give_up)"),
        }
    }

    /// Transfers linear ownership of `pending` into this set under caller `id`. If `cx.frame()`
    /// cannot fit another slot, returns `Err((Stopped::OutOfMemory, pending))` with `pending`
    /// still armed and untouched so the caller can wait or cancel it cleanly.
    pub fn push(&mut self, id: usize, pending: Pending<'buf, C>) -> Result<(), (Stopped, Pending<'buf, C>)> {
        let oom = |asked| Stopped::OutOfMemory { arena: "bstd::pending::WaitSet", asked };
        if self.slots.try_reserve(1).is_err() {
            return Err((oom(size_of::<Slot<'buf, C>>()), pending));
        }
        if pending.settle.tag.is_some() {
            if self.settle.tags.try_reserve(1).is_err() {
                return Err((oom(size_of::<Tag>()), pending));
            }
            if self.settle.tag_to_slot.try_reserve(1).is_err() {
                return Err((oom(size_of::<usize>()), pending));
            }
        }
        let Pending { into, from, finish, mut settle, unfinished } = pending;
        unfinished.finished();
        settle.armed = false;
        let idx = self.slots.len();
        if let Some(tag) = settle.tag {
            self.settle.tags.push(tag);
            self.settle.tag_to_slot.push(idx);
        }
        self.slots.push(Slot { id, into, from, finish, tag: settle.tag, span: settle.span });
        Ok(())
    }

    /// How many operations remain in this set.
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn len(&self) -> usize {
        self.slots.len()
    }

    /// Whether no operations remain in this set.
    #[blaze::nopanic]
    #[blaze::noalloc]
    #[blaze::nonblocking]
    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    /// Waits for the next operation in this set to complete, returning `None` when the set is
    /// empty, `Some(Ok((id, output)))` when an op finishes, or `Some(Err(stopped))` if `cx`
    /// stopped before any op completed (leaving remaining ops intact for `cancel_all`).
    pub fn next(&mut self, cx: &Cx) -> Option<Result<(usize, C::Output), Stopped>> {
        if self.slots.is_empty() {
            return None;
        }
        // Immediately complete any op that was refused before reaching the driver.
        if let Some(pos) = self.slots.iter().position(|s| s.tag.is_none()) {
            let slot = self.remove_slot(pos);
            let out = slot.finish.complete(slot.into, Err(Stopped::Cancelled { span: slot.span }));
            return Some(Ok((slot.id, out)));
        }
        match cx.wait_any(&self.settle.io, &self.settle.tags) {
            Ok((tag_pos, done)) => {
                let slot_pos = self.settle.tag_to_slot[tag_pos];
                let slot = self.remove_slot(slot_pos);
                let out = slot.finish.complete(slot.into, Ok(done));
                Some(Ok((slot.id, out)))
            }
            Err(stopped) => Some(Err(stopped)),
        }
    }

    /// Drains every remaining operation in completion order, invoking `on_done(id, output)` for
    /// each. If `cx` stops mid-drain, settles all remaining in-flight ops up to `cx.deadline()`
    /// before returning `Err(stopped)` so nothing lands after return.
    #[blaze::consumes]
    pub fn drain(mut self, cx: &Cx, mut on_done: impl FnMut(usize, C::Output)) -> Result<(), Stopped> {
        while let Some(step) = self.next(cx) {
            match step {
                Ok((id, out)) => on_done(id, out),
                Err(stopped) => {
                    let give_up = cx.deadline().max(Deadline::after(self.settle.io.now(), self.settle.io.limits().abandon_wait));
                    self.cancel_all(cx, give_up);
                    return Err(stopped);
                }
            }
        }
        let WaitSet { unfinished, .. } = self;
        unfinished.finished();
        Ok(())
    }

    /// Cancels/settles every remaining operation in this set up to `give_up` and consumes the
    /// linear `WaitSet`.
    #[blaze::consumes]
    pub fn cancel_all(self, cx: &Cx, give_up: Deadline) {
        let WaitSet { mut slots, mut settle, unfinished } = self;
        unfinished.finished();
        for tag in settle.tags.drain(..) {
            let _ = settle.io.settle(tag, give_up, cx.name());
        }
        settle.tag_to_slot.clear();
        slots.clear();
    }

    fn remove_slot(&mut self, slot_pos: usize) -> Slot<'buf, C> {
        let last_slot = self.slots.len() - 1;
        let removed = self.slots.swap_remove(slot_pos);
        if removed.tag.is_some()
            && let Some(tpos) = self.settle.tag_to_slot.iter().position(|&s| s == slot_pos)
        {
            self.settle.tags.swap_remove(tpos);
            self.settle.tag_to_slot.swap_remove(tpos);
        }
        if slot_pos != last_slot
            && let Some(moved) = self.settle.tag_to_slot.iter_mut().find(|s| **s == last_slot)
        {
            *moved = slot_pos;
        }
        removed
    }
}

impl<'buf, C: Complete<'buf>> fmt::Debug for Pending<'buf, C> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Pending({:?} in {}, {} bytes lent)", self.settle.tag, self.settle.span, self.into.len())
    }
}
