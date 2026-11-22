//! The simulation's run child ([`SimChild`]): a [`RunChild`] played from a script of acts on a
//! virtual clock, so a driver loop written against [`RunChild`] is tested with no process, and a
//! failing seed replays exactly.
//!
//! What it models, as a real child behind a socket pair does:
//! - a message it sends is in flight until read, and arrives in pieces: each read takes a
//!   seeded number of bytes (one byte up to all in flight, across message boundaries), each
//!   after a seeded delay of up to [`MOST_DELAY`], so a reader sees split prefixes, split
//!   bodies and two messages in one read across seeds;
//! - [`Act::Await`] is a stub waiting for its answer: nothing it does later happens until the
//!   parent has sent that many whole messages in all;
//! - its end ([`Act::Exit`], [`Act::Signal`]) closes its end of the channel: what it sent before
//!   is still read, then [`Message::Closed`];
//! - [`Act::DieMidMessage`] sends part of a message and is killed: [`ChannelError::Torn`];
//! - [`Act::Hang`] never sends and never exits: only the parent's deadline and its kill end it.
//!
//! Nothing allocates: the acts and their messages are the test's, borrowed.
//!
//! The loop is handed `&mut SimChild` (the [`RunChild`]): a wait or a kill takes that borrow, so
//! the loop cannot step the child after its end, and the test reads the child's record
//! ([`SimChild::killed`], [`SimChild::answers`]) once the loop has returned.

use alloc::vec::Vec;
use core::{alloc::Allocator, time::Duration};

use blaze_core::wait::Deadline;

use super::{
    Exit, Waited,
    run::{ChannelError, Message, PREFIX, Reaped, RunChild, RunError, RunFault, RunStep, RunWaited, whole_messages},
};
use crate::{Stopped, procs::ProcId};

/// The longest a piece of a message takes to arrive after the one before it.
pub const MOST_DELAY: Duration = Duration::from_millis(3);

/// `SIGKILL`'s number (POSIX: "9 ... SIGKILL"), how a killed child is reported.
pub const SIGKILL: i32 = 9;

/// One thing the scripted child does, in order.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Act<'s> {
    /// `after` this act becomes current, it sends `message` (a whole length-prefixed message).
    Send { after: Duration, message: &'s [u8] },
    /// It waits until the parent has sent `answers` whole messages in all.
    Await { answers: u32 },
    /// `after`, it exits with `status`.
    Exit { after: Duration, status: i32 },
    /// `after`, the signal `signal` ends it.
    Signal { after: Duration, signal: i32 },
    /// `after`, it sends the first `keep` bytes of `message` and is killed (`SIGKILL`).
    DieMidMessage { after: Duration, message: &'s [u8], keep: usize },
    /// It never sends and never exits.
    Hang,
}

/// A scripted child. See the module's doc.
pub struct SimChild<'s> {
    acts: &'s [Act<'s>],
    id: ProcId,
    now: Duration,
    /// The next act not yet done, and when the one before it was done.
    next: usize,
    since: Duration,
    /// The first act whose sent bytes are not all read, and how many of them are.
    reading: usize,
    read_off: usize,
    /// Whole messages the parent has sent.
    answers: u32,
    ended: Option<Exit>,
    killed: bool,
    rng: u64,
    trace: u64,
}

impl<'s> SimChild<'s> {
    /// The child `acts` plays, its pieces and delays drawn from `seed`.
    pub fn new(acts: &'s [Act<'s>], seed: u64, id: ProcId) -> SimChild<'s> {
        SimChild {
            acts,
            id,
            now: Duration::ZERO,
            next: 0,
            since: Duration::ZERO,
            reading: 0,
            read_off: 0,
            answers: 0,
            ended: None,
            killed: false,
            // A zero state would stay zero (xorshift): the seed is mixed with an odd constant.
            rng: seed ^ 0x9e37_79b9_7f4a_7c15,
            trace: 0xcbf2_9ce4_8422_2325,
        }
    }

    /// Whole messages the parent sent it.
    pub fn answers(&self) -> u32 {
        self.answers
    }

    /// Whether [`RunChild::kill`] ended it.
    pub fn killed(&self) -> bool {
        self.killed
    }

    /// A digest of every piece's size and arrival time: two runs with equal traces delivered
    /// the same pieces at the same times. A model test counts distinct traces across seeds to
    /// show it explored more than one execution.
    pub fn trace(&self) -> u64 {
        self.trace
    }

    fn draw(&mut self, below: u64) -> u64 {
        // xorshift64: deterministic, no allocation.
        self.rng ^= self.rng << 13;
        self.rng ^= self.rng >> 7;
        self.rng ^= self.rng << 17;
        if below == 0 { 0 } else { self.rng % below }
    }

    fn mix(&mut self, value: u64) {
        self.trace = (self.trace ^ value).wrapping_mul(0x0100_0000_01b3);
    }

    fn error(&self, step: RunStep, fault: RunFault) -> RunError {
        RunError { step, proc: Some(self.id), fault }
    }

    /// When the current act is done, or `None` while it waits (an unanswered await, a hang)
    /// or once it ended.
    fn due(&self) -> Option<Duration> {
        if self.ended.is_some() {
            return None;
        }
        match self.acts.get(self.next)? {
            Act::Send { after, .. } | Act::Exit { after, .. } | Act::Signal { after, .. } | Act::DieMidMessage { after, .. } => {
                Some(self.since.saturating_add(*after))
            }
            Act::Await { answers } if self.answers >= *answers => Some(self.since),
            Act::Await { .. } | Act::Hang => None,
        }
    }

    /// Does the current act, which is due at `at`.
    fn act(&mut self, at: Duration) {
        self.now = self.now.max(at);
        match self.acts[self.next] {
            Act::Exit { status, .. } => self.ended = Some(Exit::Exited(status)),
            Act::Signal { signal, .. } => self.ended = Some(Exit::Signaled(signal)),
            Act::DieMidMessage { .. } => self.ended = Some(Exit::Signaled(SIGKILL)),
            Act::Send { .. } | Act::Await { .. } | Act::Hang => {}
        }
        self.next += 1;
        self.since = self.now;
    }

    /// The bytes act `at` sent (none for an act that sends nothing or is not done).
    fn sent(&self, at: usize) -> &'s [u8] {
        if at >= self.next {
            return &[];
        }
        match self.acts[at] {
            Act::Send { message, .. } => message,
            Act::DieMidMessage { message, keep, .. } => &message[..keep.min(message.len())],
            _ => &[],
        }
    }

    /// How many sent bytes are not read yet.
    fn in_flight(&self) -> usize {
        let mut left = 0;
        let mut at = self.reading;
        while at < self.next {
            let bytes = self.sent(at).len();
            left += if at == self.reading { bytes - self.read_off } else { bytes };
            at += 1;
        }
        left
    }

    /// Up to `n` in-flight bytes appended to `into`, across acts.
    fn deliver<A: Allocator>(&mut self, into: &mut Vec<u8, A>, mut n: usize) {
        while n > 0 && self.reading < self.next {
            let bytes = self.sent(self.reading);
            let rest = &bytes[self.read_off..];
            let take = rest.len().min(n);
            into.extend_from_slice(&rest[..take]);
            n -= take;
            self.read_off += take;
            if self.read_off == bytes.len() {
                self.reading += 1;
                self.read_off = 0;
            }
        }
        // Acts that sent nothing are passed over.
        while self.reading < self.next && self.sent(self.reading).is_empty() {
            self.reading += 1;
        }
    }

    /// `deadline` as a time the clock can be moved to: never [`Deadline::NEVER`], which the
    /// model refuses (a wait with no deadline is the bug the deadline exists for).
    fn until(&self, step: RunStep, deadline: Deadline) -> Result<Duration, RunError> {
        match deadline == Deadline::NEVER {
            true => Err(self.error(step, RunFault::Stopped(Stopped::Deadline { span: "bstd::procs sim child: no deadline", at: deadline.get() }))),
            false => Ok(deadline.get()),
        }
    }
}

/// The steps the [`RunChild`] of a `&mut SimChild` is made of, on the child itself, so
/// [`super::sim_term::TermChild`] wraps them.
impl SimChild<'_> {
    /// The virtual clock.
    pub fn now(&self) -> Duration {
        self.now
    }

    /// [`RunChild::recv_message`].
    pub(super) fn recv_in<A: Allocator>(
        &mut self,
        into: &mut Vec<u8, A>,
        most: u32,
        deadline: Deadline,
    ) -> Result<Message, RunError> {
        let until = self.until(RunStep::Recv, deadline)?;
        loop {
            let want = match into.first_chunk::<PREFIX>() {
                Some(prefix) => {
                    let len = u32::from_be_bytes(*prefix);
                    if len > most {
                        return Err(self.error(RunStep::Recv, RunFault::Channel(ChannelError::TooLong { len, most })));
                    }
                    PREFIX + len as usize
                }
                None => PREFIX,
            };
            if into.len() >= want {
                return Ok(Message::Whole { len: want });
            }
            let flying = self.in_flight();
            if flying > 0 {
                let delay = Duration::from_micros(self.draw(MOST_DELAY.as_micros() as u64 + 1));
                let at = self.now.saturating_add(delay);
                if at > until {
                    self.now = self.now.max(until);
                    return Ok(Message::Deadline);
                }
                self.now = at;
                // What the child did by the time this piece arrives happens first: a message it
                // sent meanwhile joins what is in flight, so one piece may span two messages.
                while let Some(due) = self.due() {
                    if due > at {
                        break;
                    }
                    self.act(due);
                }
                let flying = self.in_flight();
                let n = 1 + self.draw(flying as u64) as usize;
                self.mix(n as u64);
                self.mix(at.as_micros() as u64);
                self.deliver(into, n);
                continue;
            }
            if self.ended.is_some() {
                return match into.is_empty() {
                    true => Ok(Message::Closed),
                    false => Err(self.error(RunStep::Recv, RunFault::Channel(ChannelError::Torn { have: into.len(), want }))),
                };
            }
            match self.due() {
                Some(at) if at <= until => self.act(at),
                _ => {
                    self.now = self.now.max(until);
                    return Ok(Message::Deadline);
                }
            }
        }
    }

    /// [`RunChild::send`].
    pub(super) fn send_in(&mut self, bytes: &[u8], deadline: Deadline) -> Result<(), RunError> {
        self.until(RunStep::Send, deadline)?;
        if self.ended.is_some() {
            return Err(self.error(RunStep::Send, RunFault::Channel(ChannelError::Hangup)));
        }
        let whole = whole_messages(bytes);
        if whole != bytes.len() {
            return Err(self.error(RunStep::Send, RunFault::Channel(ChannelError::NotWhole { at: whole, of: bytes.len() })));
        }
        let mut at = 0;
        while let Some(prefix) = bytes[at..].first_chunk::<PREFIX>() {
            at += PREFIX + u32::from_be_bytes(*prefix) as usize;
            self.answers += 1;
        }
        Ok(())
    }

    /// [`RunChild::wait`]: its end by `deadline`, or `None` with it still running.
    pub(super) fn wait_in(&mut self, deadline: Deadline) -> Result<Option<Exit>, RunError> {
        let until = self.until(RunStep::Wait, deadline)?;
        loop {
            if let Some(ended) = self.ended {
                return Ok(Some(ended));
            }
            match self.due() {
                Some(at) if at <= until => self.act(at),
                _ => {
                    self.now = self.now.max(until);
                    return Ok(None);
                }
            }
        }
    }

    /// [`RunChild::kill`]: an end that came first is kept, as a reaped child's is.
    pub(super) fn kill_in(&mut self) -> Exit {
        match self.ended {
            Some(ended) => ended,
            None => {
                self.killed = true;
                self.ended = Some(Exit::Signaled(SIGKILL));
                Exit::Signaled(SIGKILL)
            }
        }
    }
}

impl RunChild for &mut SimChild<'_> {
    type Left = ();

    fn now(&self) -> Duration {
        self.now
    }

    fn recv_message<A: Allocator>(
        &mut self,
        into: &mut Vec<u8, A>,
        most: u32,
        deadline: Deadline,
    ) -> Result<Message, RunError> {
        self.recv_in(into, most, deadline)
    }

    fn send(&mut self, bytes: &[u8], deadline: Deadline) -> Result<(), RunError> {
        self.send_in(bytes, deadline)
    }

    fn wait(self, deadline: Deadline) -> Result<RunWaited<Self>, RunError> {
        Ok(match self.wait_in(deadline)? {
            Some(exit) => Waited::Ended(Reaped { exit, left: () }),
            None => Waited::Running(self),
        })
    }

    fn kill(self) -> Result<Reaped<()>, RunError> {
        Ok(Reaped { exit: self.kill_in(), left: () })
    }

    /// A scripted child goes on as its acts say: it models a program whose SIGTERM handler does
    /// nothing. A heeded SIGTERM is [`super::sim_term::TermChild`]'s.
    fn terminate(&mut self) -> Result<(), RunError> {
        Ok(())
    }
}
