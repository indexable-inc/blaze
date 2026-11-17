//! A child run as a program of its own under a parent that serves it ([`Spawner::start_run`]):
//! `blaze run`'s program under the run parent, and later `blaze test`'s. Over blaze-core's one
//! linked start ([`blaze_core::linked::run::start_run`]) and its channel
//! ([`blaze_core::linked::channel`]).
//!
//! - **Streams** are the caller's typed choice ([`Streams`]): [`Streams::INHERIT`] for a run (the
//!   command's own terminal), pipes the caller reads by deadline ([`Run::take_stdout`],
//!   [`OutPipe::read`]) for a test, or `/dev/null`. Never a default inside.
//! - **The channel** ([`RunSpec::channel`]): a socket pair, the child's end its descriptor
//!   [`CHILD_FD`], the parent's read and written in whole length-prefixed messages by deadline
//!   ([`RunChild::recv_message`], [`RunChild::send`]).
//! - **Its end**: [`RunChild::wait`] by a deadline, how it ended (an exit's status or the
//!   signal's number) or [`Ended::Deadline`]; [`RunChild::kill`].
//! - **Never outliving the parent**: [`RunSpec::parent_death`] ([`ParentDeath::STRONGEST`]:
//!   the kernel's kill on Linux, the channel's hang-up on macOS); on every return or unwind the
//!   drop kills and reaps it.
//!
//! [`RunChild`] is the seam a driver loop is written against: [`Run`] is the native one,
//! [`super::sim_child::SimChild`] the simulation's (a scripted child on a virtual clock).
//! Every wait is capped by the task's deadline, which, passing first, is
//! [`RunFault::Stopped`].

use alloc::{string::String, vec::Vec};
use core::{alloc::Allocator, fmt, time::Duration};

pub use blaze_core::linked::{
    channel::{CHILD_FD, ChannelError, Message, PREFIX, Read, whole_messages},
    run::{Input, Output, ParentDeath, Streams},
};
use blaze_core::{
    Command, Sandbox,
    linked::{
        LinkError, LinkedChild,
        channel::{self, Channel, DeadlineFd},
        run::{RunLink, start_run},
    },
    wait::Deadline,
};

use super::{DriverClock, Ended, Spawner};
use crate::{
    Stopped,
    cx::Cx,
    io::Io,
    procs::ProcId,
};

/// The span a run child's waits are named by in a [`Stopped::Deadline`].
const RUN_SPAN: &str = "bstd::procs run child";

/// What to start: the program (a path, or a name found on `env`'s `PATH`), its `argv[0]`
/// where it is not the program's path, its arguments, its whole environment (nothing of this
/// process's is inherited), its directory, its streams, whether it is given the channel as
/// [`CHILD_FD`], whether it leads a process group of its own, and what ends it at this
/// process's death.
pub struct RunSpec<'a> {
    pub program: &'a str,
    pub arg0: Option<&'a str>,
    pub args: &'a [&'a str],
    pub env: &'a [(&'a str, &'a str)],
    pub dir: &'a str,
    pub streams: Streams,
    pub channel: bool,
    pub own_group: bool,
    pub parent_death: ParentDeath,
}

/// What a step on a run child was.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RunStep {
    Start,
    Recv,
    Send,
    Wait,
    Kill,
    /// SIGTERM sent to it ([`RunChild::terminate`]).
    Terminate,
    ReadOutput,
}

/// How a step on a run child failed.
#[derive(Clone, PartialEq, Eq, Debug)] // clone: an error
pub enum RunFault {
    /// blaze-core refused or failed it, saying this.
    Host(&'static str),
    /// The channel: torn, too long, hung up, a write past its deadline, a system error.
    Channel(ChannelError),
    /// A channel step on a child started without one ([`RunSpec::channel`] false).
    NoChannel,
    /// The task's deadline passed first, or the task was cancelled.
    Stopped(Stopped),
}

/// A step on a run child that failed: the step, the child (none before it started) and how.
#[derive(Clone, PartialEq, Eq, Debug)] // clone: an error
pub struct RunError {
    pub step: RunStep,
    pub proc: Option<ProcId>,
    pub fault: RunFault,
}

impl fmt::Display for RunError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.proc {
            Some(proc) => write!(f, "bstd::procs run child {proc}, {:?}: ", self.step)?,
            None => write!(f, "bstd::procs run child, {:?}: ", self.step)?,
        }
        match &self.fault {
            RunFault::Host(said) => write!(f, "the host refused: {said}"),
            RunFault::Channel(error) => write!(f, "{error}"),
            RunFault::NoChannel => f.write_str("it was started without a channel"),
            RunFault::Stopped(stopped) => write!(f, "{stopped}"),
        }
    }
}

impl core::error::Error for RunError {}

/// A child a driver loop serves: the native [`Run`] or the simulation's
/// [`super::sim_child::SimChild`]. Deadlines are on [`RunChild::now`]'s clock.
pub trait RunChild {
    /// The clock every deadline here is on.
    fn now(&self) -> Duration;

    /// Reads into the end of `into` until it starts with a whole message ([`Message::Whole`]),
    /// `deadline` passes ([`Message::Deadline`], what came kept in `into`) or the child's end
    /// closes at a message boundary ([`Message::Closed`]); a close inside a message is
    /// [`ChannelError::Torn`], a body past `most` [`ChannelError::TooLong`].
    fn recv_message<A: Allocator>(
        &mut self,
        into: &mut Vec<u8, A>,
        most: u32,
        deadline: Deadline,
    ) -> Result<Message, RunError>;

    /// `bytes`, whole messages, all written by `deadline`.
    fn send(&mut self, bytes: &[u8], deadline: Deadline) -> Result<(), RunError>;

    /// How it ended, by `deadline`; [`Ended::Deadline`] with it still running.
    fn wait(&mut self, deadline: Deadline) -> Result<Ended, RunError>;

    /// Ended now by `SIGKILL` (its group where it leads one) and reaped: how it ended (an end
    /// that came first is kept).
    fn kill(&mut self) -> Result<Ended, RunError>;

    /// `SIGTERM` sent to it (its group where it leads one): asked to end, which it may handle
    /// or ignore. Nothing is waited for: its end is [`RunChild::wait`]'s, by the caller's
    /// deadline (the run parent's grace, decision D103). An ended child is sent nothing.
    fn terminate(&mut self) -> Result<(), RunError>;
}

/// A started run child, until it has ended and been waited for or killed. Dropped before
/// that, it is killed and reaped (blaze-core's `LinkedChild`).
pub struct Run {
    child: LinkedChild,
    channel: Option<Channel>,
    own_group: bool,
    io: Io,
    /// The starting task's deadline: every wait is capped by it.
    task: Deadline,
}

/// The read end of a piped standard output or error ([`Output::Pipe`]), read by deadline.
pub struct OutPipe {
    fd: DeadlineFd,
    proc: ProcId,
    io: Io,
    task: Deadline,
}

impl Spawner {
    /// `spec` started: its streams as named, the channel given where asked, its group its own
    /// where asked. Refused before it starts when the task's deadline has passed; what
    /// blaze-core refuses by name ([`blaze_core::linked::LinkOp`]) is [`RunFault::Host`].
    pub fn start_run(&self, cx: &Cx, spec: &RunSpec<'_>) -> Result<Run, RunError> {
        let refuse = |fault| RunError { step: RunStep::Start, proc: None, fault };
        let task = cx.deadline();
        if task.passed(self.io.now()) {
            return Err(refuse(RunFault::Stopped(Stopped::Deadline { span: RUN_SPAN, at: task.get() })));
        }
        // blaze-core's `Command` holds std's path and string types, made here from text through
        // `From<String>`: the allocator is the global one its fields name.
        let command = Command {
            program: String::from(spec.program).into(),
            args: spec.args.iter().map(|it| String::from(*it).into()).collect(),
            env: spec.env.iter().map(|(name, value)| (String::from(*name).into(), String::from(*value).into())).collect(),
            dir: String::from(spec.dir).into(),
            stdin: Vec::new(),
            sandbox: Sandbox::Off,
            memory: None,
        };
        let pair = match spec.channel {
            true => Some(channel::pair().map_err(|error| refuse(RunFault::Channel(error)))?),
            false => None,
        };
        let given = pair.as_ref().map(|(_, end)| [(end.fd(), CHILD_FD)]);
        let link = RunLink {
            inherit: match &given {
                Some(one) => &one[..],
                None => &[],
            },
            own_group: spec.own_group,
            streams: spec.streams,
            arg0: spec.arg0.map(|it| it.as_ref()),
            parent_death: spec.parent_death,
        };
        let child = start_run(&command, &link).map_err(|error: LinkError| refuse(RunFault::Host(error.op.what())))?;
        // The child's end is dropped here: from now on the child holds the only copy, so its
        // exit closes the channel for this end's reads.
        let channel = pair.map(|(parent, _child_end)| parent);
        Ok(Run { child, channel, own_group: spec.own_group, io: self.io.clone(), task })
    }
}

impl Run {
    /// Its process id; also its group's where it leads one.
    pub fn id(&self) -> ProcId {
        ProcId(self.child.pid())
    }

    /// Its standard output's read end, once, where it was [`Output::Pipe`].
    pub fn take_stdout(&mut self) -> Result<Option<OutPipe>, RunError> {
        match self.child.take_stdout() {
            None => Ok(None),
            Some(fd) => self.out_pipe(DeadlineFd::new(fd, "a run child's standard output")).map(Some),
        }
    }

    /// Its standard error's read end, once, where it was [`Output::Pipe`].
    pub fn take_stderr(&mut self) -> Result<Option<OutPipe>, RunError> {
        match self.child.take_stderr() {
            None => Ok(None),
            Some(fd) => self.out_pipe(DeadlineFd::new(fd, "a run child's standard error")).map(Some),
        }
    }

    fn out_pipe<E>(&self, made: Result<DeadlineFd, E>) -> Result<OutPipe, RunError> {
        match made {
            Ok(fd) => Ok(OutPipe { fd, proc: self.id(), io: self.io.clone(), task: self.task }),
            Err(_) => Err(self.error(RunStep::ReadOutput, RunFault::Host("making the pipe non-blocking (fcntl O_NONBLOCK)"))),
        }
    }

    fn error(&self, step: RunStep, fault: RunFault) -> RunError {
        RunError { step, proc: Some(self.id()), fault }
    }

    /// The earlier of `deadline` and the task's, and whether the task's is the one.
    fn capped(&self, deadline: Deadline) -> (Deadline, bool) {
        let until = deadline.min(self.task);
        (until, until < deadline)
    }

    fn stopped(&self, step: RunStep, at: Deadline) -> RunError {
        self.error(step, RunFault::Stopped(Stopped::Deadline { span: RUN_SPAN, at: at.get() }))
    }
}

impl RunChild for Run {
    fn now(&self) -> Duration {
        self.io.now()
    }

    fn recv_message<A: Allocator>(
        &mut self,
        into: &mut Vec<u8, A>,
        most: u32,
        deadline: Deadline,
    ) -> Result<Message, RunError> {
        let (until, task_first) = self.capped(deadline);
        let id = self.id();
        let Some(channel) = self.channel.as_mut() else {
            return Err(RunError { step: RunStep::Recv, proc: Some(id), fault: RunFault::NoChannel });
        };
        match channel.recv_message(into, most, until, &DriverClock(&self.io)) {
            Ok(Message::Deadline) if task_first => Err(self.stopped(RunStep::Recv, until)),
            Ok(message) => Ok(message),
            Err(error) => Err(RunError { step: RunStep::Recv, proc: Some(id), fault: RunFault::Channel(error) }),
        }
    }

    fn send(&mut self, bytes: &[u8], deadline: Deadline) -> Result<(), RunError> {
        let (until, task_first) = self.capped(deadline);
        let id = self.id();
        let Some(channel) = self.channel.as_mut() else {
            return Err(RunError { step: RunStep::Send, proc: Some(id), fault: RunFault::NoChannel });
        };
        match channel.send(bytes, until, &DriverClock(&self.io)) {
            Ok(()) => Ok(()),
            Err(ChannelError::WriteDeadline { .. }) if task_first => Err(self.stopped(RunStep::Send, until)),
            Err(error) => Err(RunError { step: RunStep::Send, proc: Some(id), fault: RunFault::Channel(error) }),
        }
    }

    fn wait(&mut self, deadline: Deadline) -> Result<Ended, RunError> {
        let (until, task_first) = self.capped(deadline);
        match self.child.wait_until_on(until, &DriverClock(&self.io)) {
            Ok(Ended::Deadline) if task_first => Err(self.stopped(RunStep::Wait, until)),
            Ok(ended) => Ok(ended),
            Err(error) => Err(self.error(RunStep::Wait, RunFault::Host(error.op.what()))),
        }
    }

    fn kill(&mut self) -> Result<Ended, RunError> {
        let killed = match self.own_group {
            // The group's kill reaps the leader; its end is then kept, read back at once.
            true => self.child.kill_group().and_then(|()| self.child.wait_until_on(Deadline::PASSED, &DriverClock(&self.io))),
            false => self.child.kill_now(),
        };
        killed.map_err(|error| self.error(RunStep::Kill, RunFault::Host(error.op.what())))
    }

    fn terminate(&mut self) -> Result<(), RunError> {
        self.child.terminate().map_err(|error| self.error(RunStep::Terminate, RunFault::Host(error.op.what())))
    }
}

impl OutPipe {
    /// Up to `into.len()` bytes by `deadline` (capped by the starting task's): [`Read::Closed`]
    /// once the child and all it started have closed the pipe.
    pub fn read(&self, into: &mut [u8], deadline: Deadline) -> Result<Read, RunError> {
        let until = deadline.min(self.task);
        let error = |fault| RunError { step: RunStep::ReadOutput, proc: Some(self.proc), fault };
        match self.fd.read_some(into, until, &DriverClock(&self.io)) {
            Ok(Read::Deadline) if until < deadline => {
                Err(error(RunFault::Stopped(Stopped::Deadline { span: RUN_SPAN, at: until.get() })))
            }
            Ok(read) => Ok(read),
            Err(fault) => Err(error(RunFault::Channel(fault))),
        }
    }
}

impl fmt::Debug for Run {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Run({})", self.child.pid())
    }
}
