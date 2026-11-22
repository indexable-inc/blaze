//! The simulation's model of a run child's answer to SIGTERM ([`TermChild`]): a [`SimChild`]
//! whose [`RunChild::terminate`] it either heeds (it ends by SIGTERM a set time later, as a
//! program with the default disposition or a shutdown handler does) or ignores (a program that
//! catches SIGTERM and goes on, or blocks it), so the run parent's rule (decision D103:
//! forward once, wait its own bounded step, then kill) is tested on both.
//!
//! What it adds to [`SimChild`]: only the end by SIGTERM. Everything else is the inner child's
//! (its messages, its own end, a kill). A heeded SIGTERM ends it at its time even inside a
//! read: what it had sent and not yet been read is dropped with it (a close at a message's
//! boundary is [`Message::Closed`], inside one [`ChannelError::Torn`]), the model's
//! simplification; the tests that use it send nothing after the forward.

use alloc::vec::Vec;
use core::{alloc::Allocator, time::Duration};

use blaze_core::wait::Deadline;

use super::{
    Exit, Waited,
    run::{ChannelError, Message, PREFIX, Reaped, RunChild, RunError, RunFault, RunStep, RunWaited},
    sim_child::SimChild,
};
use crate::procs::ProcId;

/// SIGTERM's number (POSIX: "15 ... SIGTERM"), how a child that heeds it is reported.
pub const SIGTERM: i32 = 15;

/// What the modelled program does with a SIGTERM.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum OnTerminate {
    /// It ends by the signal `after` it came.
    Heed { after: Duration },
    /// It goes on as if none came.
    Ignore,
}

/// A [`SimChild`] with an answer to SIGTERM. See the module's doc.
pub struct TermChild<'s> {
    inner: SimChild<'s>,
    on: OnTerminate,
    id: ProcId,
    /// When a heeded SIGTERM ends it.
    ends_at: Option<Duration>,
    /// Its end by a heeded SIGTERM (the inner child's own ends are the inner's).
    ended: Option<Exit>,
    terminated_at: Option<Duration>,
    killed_at: Option<Duration>,
    terminations: u32,
}

impl<'s> TermChild<'s> {
    /// `inner` (started as `id`), answering a SIGTERM as `on` says.
    pub fn new(inner: SimChild<'s>, on: OnTerminate, id: ProcId) -> TermChild<'s> {
        TermChild { inner, on, id, ends_at: None, ended: None, terminated_at: None, killed_at: None, terminations: 0 }
    }

    /// The scripted child beneath (its answers, its kill, its trace).
    pub fn inner(&self) -> &SimChild<'s> {
        &self.inner
    }

    /// When the first SIGTERM was sent to it.
    pub fn terminated_at(&self) -> Option<Duration> {
        self.terminated_at
    }

    /// When [`RunChild::kill`] was called on it while it ran.
    pub fn killed_at(&self) -> Option<Duration> {
        self.killed_at
    }

    /// How many SIGTERMs were sent to it.
    pub fn terminations(&self) -> u32 {
        self.terminations
    }

    /// The earlier of `deadline` and a heeded SIGTERM's end, and whether the end is it.
    fn cut(&self, deadline: Deadline) -> (Deadline, bool) {
        match self.ends_at {
            Some(at) if Deadline::at(at) < deadline => (Deadline::at(at), true),
            Some(_) | None => (deadline, false),
        }
    }

    /// Its channel as a heeded end leaves it: closed at a boundary, torn inside a message.
    fn closed(&self, into: &[u8]) -> Result<Message, RunError> {
        if into.is_empty() {
            return Ok(Message::Closed);
        }
        let want = match into.first_chunk::<PREFIX>() {
            Some(prefix) => PREFIX + u32::from_be_bytes(*prefix) as usize,
            None => PREFIX,
        };
        Err(RunError { step: RunStep::Recv, proc: Some(self.id), fault: RunFault::Channel(ChannelError::Torn { have: into.len(), want }) })
    }
}

impl TermChild<'_> {
    /// [`RunChild::wait`]: its end by `deadline`, or `None` with it still running.
    fn wait_in(&mut self, deadline: Deadline) -> Result<Option<Exit>, RunError> {
        if let Some(ended) = self.ended {
            return Ok(Some(ended));
        }
        let (until, end_first) = self.cut(deadline);
        match self.inner.wait_in(until)? {
            None if end_first => {
                self.ended = Some(Exit::Signaled(SIGTERM));
                Ok(Some(Exit::Signaled(SIGTERM)))
            }
            ended => Ok(ended),
        }
    }

    /// [`RunChild::kill`]: an end that came first is kept, as a reap's.
    fn kill_in(&mut self) -> Exit {
        if let Some(ended) = self.ended {
            return ended;
        }
        let ended = self.inner.kill_in();
        // Only a kill that ended it is one.
        if self.inner.killed() && self.killed_at.is_none() {
            self.killed_at = Some(self.inner.now());
        }
        ended
    }
}

impl RunChild for &mut TermChild<'_> {
    type Left = ();

    fn now(&self) -> Duration {
        self.inner.now()
    }

    fn recv_message<A: Allocator>(&mut self, into: &mut Vec<u8, A>, most: u32, deadline: Deadline) -> Result<Message, RunError> {
        if self.ended.is_some() {
            return self.closed(into);
        }
        let (until, end_first) = self.cut(deadline);
        match self.inner.recv_in(into, most, until)? {
            Message::Deadline if end_first => {
                self.ended = Some(Exit::Signaled(SIGTERM));
                self.closed(into)
            }
            message => Ok(message),
        }
    }

    fn send(&mut self, bytes: &[u8], deadline: Deadline) -> Result<(), RunError> {
        if self.ended.is_some() {
            return Err(RunError { step: RunStep::Send, proc: Some(self.id), fault: RunFault::Channel(ChannelError::Hangup) });
        }
        self.inner.send_in(bytes, deadline)
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

    fn terminate(&mut self) -> Result<(), RunError> {
        self.terminations += 1;
        let now = self.inner.now();
        if self.terminated_at.is_none() {
            self.terminated_at = Some(now);
        }
        match self.on {
            OnTerminate::Heed { after } if self.ends_at.is_none() && self.ended.is_none() => {
                self.ends_at = Some(now.saturating_add(after));
            }
            OnTerminate::Heed { .. } | OnTerminate::Ignore => {}
        }
        Ok(())
    }
}
