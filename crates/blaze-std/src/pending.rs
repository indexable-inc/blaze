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

use alloc::boxed::Box;
use core::{fmt, marker::PhantomData};

use blaze_core::{Done, Effect, Tag, wait::Deadline};

use crate::{
    Plant, Stopped,
    cx::Cx,
    io::{Ends, Io},
    trace::RecordKind,
};

/// What a `Pending` makes of its op's end: its outcome, given the lent buffer and the
/// completion or why the wait stopped.
type Finish<'buf, T> = Box<dyn FnOnce(&mut [u8], Result<Done, Stopped>) -> T + 'buf>;

/// An op in flight, borrowing its buffer for as long as it runs: finished by
/// [`Pending::wait`] or [`Pending::cancel`] (see the module's doc for why it is linear and
/// what a panic that drops it does).
#[blaze::linear]
#[must_use = "an op in flight is finished with wait(cx) or cancel(cx)"]
pub struct Pending<'buf, T> {
    /// Where a read's bytes go; empty for a write.
    into: &'buf mut [u8],
    /// A write's bytes stay lent until it ends, so the caller cannot change them meanwhile.
    from: PhantomData<&'buf [u8]>,
    finish: Finish<'buf, T>,
    settle: Settle,
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
            Ok(Done::Read(..) | Done::ReadInto { .. } | Done::Appended | Done::Synced | Done::WrittenBack | Done::Truncated | Done::Written | Done::Listed(..) | Done::Renamed | Done::Moded | Done::Private(..) | Done::Timed | Done::Cloned | Done::Linked | Done::Stat(..) | Done::Removed | Done::Locked | Done::Contended | Done::Unlocked | Done::Changed { .. } | Done::Journaled(..) | Done::Stamped(..) | Done::Entries(..) | Done::Unwatched | Done::Pressed { .. } | Done::Flushed | Done::Fired | Done::Mapped(..) | Done::Exited(..) | Done::Accepted { .. } | Done::Connected { .. } | Done::Unlistened | Done::Sent(..) | Done::HungUp | Done::Broken { .. } | Done::Bound { .. } | Done::Datagram { .. } | Done::Unbound | Done::Wall(..) | Done::Random(..) | Done::MadeDir | Done::RemovedDir | Done::Failed(..)) => "pending: abandoned, ended",
            Err(Stopped::Deadline { .. } | Stopped::Cancelled { .. } | Stopped::OutOfMemory { .. } | Stopped::Stalled { .. }) => "pending: abandoned, still in flight",
        };
        self.io.record_trace(self.span, RecordKind::Event(what));
    }
}

/// `effect` handed to the driver now, its outcome made by `finish` once it ends.
pub(crate) fn start<'buf, T>(
    io: &Io,
    span: &'static str,
    into: &'buf mut [u8],
    effect: impl FnOnce(Tag) -> Effect,
    finish: impl FnOnce(&mut [u8], Result<Done, Stopped>) -> T + 'buf,
) -> Pending<'buf, T> {
    let tag = io.submit(effect);
    Pending {
        into,
        from: PhantomData,
        // alloc: once per op, its outcome's maker (bounded: freed when the op is finished).
        finish: Box::new(finish),
        settle: Settle { io: io.clone(), tag: Some(tag), span, armed: true },
    }
}

/// An op refused before it started (a write past its capability's budget): nothing is handed
/// to the driver, and waiting for it or cancelling it gives `outcome` at once.
pub(crate) fn refused<'buf, T: 'buf>(io: &Io, span: &'static str, outcome: T) -> Pending<'buf, T> {
    Pending {
        into: &mut [],
        from: PhantomData,
        // alloc: once per op, its outcome's holder (freed when the op is finished).
        finish: Box::new(move |_: &mut [u8], _| outcome),
        settle: Settle { io: io.clone(), tag: None, span, armed: true },
    }
}

impl<T> Pending<'_, T> {
    /// The op's outcome, waited for under `cx`: until its deadline, ending early on its
    /// cancellation (after which the op's end is still waited for, as [`Pending::cancel`]
    /// does, so nothing it does lands later).
    #[blaze::consumes]
    pub fn wait(self, cx: &Cx) -> T {
        let Pending { into, from: _, finish, mut settle } = self;
        settle.armed = false;
        let done = match settle.tag {
            Some(tag) => cx.wait(&settle.io, tag, Ends::Once),
            // Refused before it started: its outcome is made already and reads no completion.
            None => Err(Stopped::Cancelled { span: settle.span }),
        };
        finish(into, done)
    }

    /// The op given up: blaze-core cannot cancel an effect in flight, so this waits, heeding
    /// no cancellation, for the op to end, until `give_up`: the caller's bound on a wait that
    /// by design outlasts a cancelled or expired `cx` (it was once the later of `cx`'s
    /// deadline and [`crate::io::Limits::abandon_wait`] from now, picked here). Its outcome is what the op did:
    /// done, or why the wait stopped (then the op may still be in flight, and the stop says
    /// so). `cx` names the span the wait is recorded in.
    #[blaze::consumes]
    pub fn cancel(self, cx: &Cx, give_up: Deadline) -> T {
        let Pending { into, from: _, finish, mut settle } = self;
        settle.armed = false;
        let Some(tag) = settle.tag else {
            // Refused before it started: nothing is in flight to wait for.
            return finish(into, Err(Stopped::Cancelled { span: cx.name() }));
        };
        if cx.plant() == Plant::CancelSkipsWait {
            settle.io.forget(tag, Ends::Once);
            return finish(into, Err(Stopped::Cancelled { span: cx.name() }));
        }
        let done = settle.io.settle(tag, give_up, cx.name());
        finish(into, done)
    }
}

impl<T> fmt::Debug for Pending<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Pending({:?} in {}, {} bytes lent)", self.settle.tag, self.settle.span, self.into.len())
    }
}
