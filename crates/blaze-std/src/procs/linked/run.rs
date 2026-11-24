//! A child run as a program of its own under a parent that serves it ([`Spawner::start_served`]):
//! `blaze run`'s program under the run parent, and `blaze test`'s. Over blaze-core's one
//! linked start ([`blaze_core::linked::run::start_run`]) and its channel
//! ([`blaze_core::linked::channel`]).
//!
//! - **Streams** are the caller's typed choice ([`Streams`]): [`Streams::INHERIT`] for a run (the
//!   command's own terminal), pipes the caller reads by deadline ([`Pipes`], given once with the
//!   child at its start, [`OutPipe::read`]) for a test, or `/dev/null`. Never a default inside.
//! - **The channel** is a start of its own, not a flag: [`Spawner::start`] gives a [`Run`] with
//!   none, [`Spawner::start_served`] a [`Served`], a run with a socket pair whose child end is its
//!   descriptor [`CHILD_FD`] and whose parent end is read and written in whole length-prefixed
//!   messages by deadline ([`RunChild::recv_message`], [`RunChild::send`]). A channel step on a
//!   run without one does not compile (the run-time `NoChannel` refusal is gone).
//! - **Its end** is a fact of the type: [`RunChild::wait`] by a deadline takes the child and gives
//!   back either its end ([`Waited::Ended`], a [`Reaped`]: the [`Exit`] and what is left of it) or
//!   the child, still running ([`Waited::Running`]); [`RunChild::kill`] takes it and gives its end.
//!   Nothing is done to a child after its end; nothing says "ended" of a child still running.
//! - **Never outliving the parent**: [`RunSpec::parent_death`] ([`ParentDeath::STRONGEST`]:
//!   the kernel's kill on Linux, the channel's hang-up on macOS); on every return or unwind the
//!   drop kills and reaps it.
//!
//! [`RunChild`] is the seam a driver loop is written against: [`Served`] is the native one, a
//! `&mut` [`super::sim_child::SimChild`] the simulation's (a scripted child on a virtual clock).
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

use super::{DriverClock, Exit, Spawner, Waited};
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
/// process's is inherited), its directory, its streams, whether it leads a process group of
/// its own, and what ends it at this process's death. Whether it is given a channel is which
/// start is called ([`Spawner::start`] or [`Spawner::start_served`]).
pub struct RunSpec<'a> {
    pub program: &'a str,
    pub arg0: Option<&'a str>,
    pub args: &'a [&'a str],
    pub env: &'a [(&'a str, &'a str)],
    pub dir: &'a str,
    pub streams: Streams,
    pub own_group: bool,
    pub parent_death: ParentDeath,
}

/// What to start detached in a new session ([`Spawner::start_detached`]): the program, its
/// arguments, its whole environment (nothing of this process's is inherited), and its directory.
/// Standard streams are `/dev/null` and every other descriptor is closed on exec, so the parent
/// never blocks on it and the child outlives the parent.
pub struct DetachedSpec<'a> {
    pub program: &'a str,
    pub args: &'a [&'a str],
    pub env: &'a [(&'a str, &'a str)],
    pub dir: &'a str,
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
    /// blaze-core refused or failed it, saying `what`, with raw OS errno `os` (0 when none).
    Host { what: &'static str, os: i32 },
    /// The channel: torn, too long, hung up, a write past its deadline, a system error.
    Channel(ChannelError),
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
            RunFault::Host { what, os: 0 } => write!(f, "the host refused: {what}"),
            RunFault::Host { what, os } => write!(f, "the host refused: {what}: {}", blaze_core::linked::OsError(*os)),
            RunFault::Channel(error) => write!(f, "{error}"),
            RunFault::Stopped(stopped) => write!(f, "{stopped}"),
        }
    }
}

impl core::error::Error for RunError {}

/// A child's end: how it ended, and what is left of it ([`RunChild::Left`]: a served child's
/// channel end, still read for what it wrote before its end; nothing for a simulation's).
#[derive(Debug)]
pub struct Reaped<L> {
    pub exit: Exit,
    pub left: L,
}

/// What [`RunChild::wait`] gives: the child's end, or the child back, still running.
pub type RunWaited<C> = Waited<C, Reaped<<C as RunChild>::Left>>;

/// A child a driver loop serves: the native [`Served`] or the simulation's (a `&mut`
/// [`super::sim_child::SimChild`] or [`super::sim_term::TermChild`]). Deadlines are on
/// [`RunChild::now`]'s clock. A wait and a kill take the child: after its end there is no child
/// to step, only what it left ([`Reaped::left`]).
pub trait RunChild: Sized {
    /// What is left once it has ended (see [`Reaped`]).
    type Left;

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

    /// How it ended, by `deadline` (it is reaped, and only what it left is given), or the child
    /// back, still running. A wait that fails drops the child (native: killed and reaped).
    fn wait(self, deadline: Deadline) -> Result<RunWaited<Self>, RunError>;

    /// Ended now by `SIGKILL` (its group where it leads one) and reaped: how it ended (an end
    /// that came first is the one read) and what it left.
    fn kill(self) -> Result<Reaped<Self::Left>, RunError>;

    /// `SIGTERM` sent to it (its group where it leads one): asked to end, which it may handle
    /// or ignore. Nothing is waited for: its end is [`RunChild::wait`]'s, by the caller's
    /// deadline (the run parent's grace, decision D103).
    fn terminate(&mut self) -> Result<(), RunError>;
}

/// A started run child with no channel, running while this value lives. Dropped before its
/// end, it is killed and reaped (blaze-core's `LinkedChild`).
pub struct Run {
    child: LinkedChild,
    own_group: bool,
    rest: Rest,
}

/// What of a run outlives its end: its id, the driver's clock and the starting task's deadline
/// (every wait is capped by it).
struct Rest {
    proc: ProcId,
    io: Io,
    task: Deadline,
}

/// A run started with its channel ([`Spawner::start_served`]): the only child the channel steps
/// are offered on.
pub struct Served {
    run: Run,
    channel: Channel,
}

/// What a served child leaves at its end: its channel's parent end, still read for what the
/// child wrote before it ended (then [`Message::Closed`]), and the clock.
pub struct ServedEnd {
    channel: Channel,
    rest: Rest,
}

/// The read end of a piped standard output or error ([`Output::Pipe`]), read by deadline.
pub struct OutPipe {
    fd: DeadlineFd,
    proc: ProcId,
    io: Io,
    task: Deadline,
}

/// The read ends of a run's streams asked as [`Output::Pipe`], given once, with the child.
pub struct Pipes {
    pub stdout: Option<OutPipe>,
    pub stderr: Option<OutPipe>,
}

/// A child just started, and its pipes.
pub struct Started<C> {
    pub child: C,
    pub pipes: Pipes,
}

fn make_command(program: &str, args: &[&str], env: &[(&str, &str)], dir: &str) -> Command {
    // blaze-core's `Command` holds std's path and string types, made here from text through
    // `From<String>`: the allocator is the global one its fields name.
    Command {
        program: String::from(program).into(),
        args: args.iter().map(|it| String::from(*it).into()).collect(),
        env: env.iter().map(|(name, value)| (String::from(*name).into(), String::from(*value).into())).collect(),
        dir: String::from(dir).into(),
        stdin: Vec::new(),
        sandbox: Sandbox::Off,
        memory: None,
    }
}

impl Spawner {
    /// `spec` started with no channel: its streams as named, its group its own where asked.
    /// Refused before it starts when the task's deadline has passed; what blaze-core refuses by
    /// name ([`blaze_core::linked::LinkOp`]) is [`RunFault::Host`].
    pub fn start(&self, cx: &Cx, spec: &RunSpec<'_>) -> Result<Started<Run>, RunError> {
        self.start_with(cx, spec, &[], None)
    }

    /// `spec` started detached in a new session (`setsid` on Linux, `POSIX_SPAWN_SETSID` on macOS)
    /// with `/dev/null` stdio and every other descriptor closed on exec. Returns the started
    /// process's [`ProcId`]; the child is not linked to this process and outlives it.
    pub fn start_detached(&self, cx: &Cx, spec: &DetachedSpec<'_>) -> Result<ProcId, RunError> {
        let refuse = |fault| RunError { step: RunStep::Start, proc: None, fault };
        let task = cx.deadline();
        if task.passed(self.io.now()) {
            return Err(refuse(RunFault::Stopped(Stopped::Deadline { span: RUN_SPAN, at: task.get() })));
        }
        let command = make_command(spec.program, spec.args, spec.env, spec.dir);
        let pid = blaze_core::linked::run::start_detached(&command).map_err(|error: LinkError| refuse(RunFault::Host { what: error.op.what(), os: error.os }))?;
        Ok(ProcId(pid))
    }

    /// `spec` started confined: the child enters `confine` last before its program runs (D177;
    /// blaze-core's linked start runs it as data in the child). Only the hermetic run
    /// ([`super::super::hermetic`]) calls it: a confinement is made from its plan, never by
    /// hand. A refused step is [`RunFault::Host`] naming it; the program never ran.
    #[cfg(target_os = "linux")]
    pub(crate) fn start_confined(
        &self,
        cx: &Cx,
        spec: &RunSpec<'_>,
        confine: &blaze_core::linked::run::ChildConfine<'_>,
    ) -> Result<Started<Run>, RunError> {
        self.start_with(cx, spec, &[], Some(confine))
    }

    /// `spec` started with its channel as [`CHILD_FD`]: as [`Spawner::start`] otherwise.
    pub fn start_served(&self, cx: &Cx, spec: &RunSpec<'_>) -> Result<Started<Served>, RunError> {
        let refuse = |fault| RunError { step: RunStep::Start, proc: None, fault };
        let (parent, child_end) = channel::pair().map_err(|error| refuse(RunFault::Channel(error)))?;
        let given = [(child_end.fd(), CHILD_FD)];
        let Started { child: run, pipes } = self.start_with(cx, spec, &given, None)?;
        // The child's end is dropped here: from now on the child holds the only copy, so its
        // exit closes the channel for this end's reads.
        drop(child_end);
        Ok(Started { child: Served { run, channel: parent }, pipes })
    }

    fn start_with(
        &self,
        cx: &Cx,
        spec: &RunSpec<'_>,
        inherit: &[(super::BorrowedFd<'_>, u32)],
        confine: Option<&blaze_core::linked::run::ChildConfine<'_>>,
    ) -> Result<Started<Run>, RunError> {
        let refuse = |fault| RunError { step: RunStep::Start, proc: None, fault };
        let task = cx.deadline();
        if task.passed(self.io.now()) {
            return Err(refuse(RunFault::Stopped(Stopped::Deadline { span: RUN_SPAN, at: task.get() })));
        }
        let command = make_command(spec.program, spec.args, spec.env, spec.dir);
        let link = RunLink {
            inherit,
            own_group: spec.own_group,
            streams: spec.streams,
            arg0: spec.arg0.map(|it| it.as_ref()),
            parent_death: spec.parent_death,
            confine,
        };
        let started = start_run(&command, &link).map_err(|error: LinkError| refuse(RunFault::Host { what: error.op.what(), os: error.os }))?;
        let proc = ProcId(started.child.pid());
        let pipe = |fd, what| match DeadlineFd::new(fd, what) {
            Ok(fd) => Ok(OutPipe { fd, proc, io: self.io.clone(), task }),
            Err(error) => Err(RunError {
                step: RunStep::ReadOutput,
                proc: Some(proc),
                fault: RunFault::Host {
                    what: "making the pipe non-blocking (fcntl O_NONBLOCK)",
                    os: error.raw_os_error().unwrap_or(0),
                },
            }),
        };
        // A pipe refused here drops the child: killed and reaped.
        let stdout = started.stdout.map(|fd| pipe(fd, "a run child's standard output")).transpose()?;
        let stderr = started.stderr.map(|fd| pipe(fd, "a run child's standard error")).transpose()?;
        let rest = Rest { proc, io: self.io.clone(), task };
        Ok(Started { child: Run { child: started.child, own_group: spec.own_group, rest }, pipes: Pipes { stdout, stderr } })
    }
}

impl Rest {
    fn error(&self, step: RunStep, fault: RunFault) -> RunError {
        RunError { step, proc: Some(self.proc), fault }
    }

    /// The earlier of `deadline` and the task's, and whether the task's is the one.
    fn capped(&self, deadline: Deadline) -> (Deadline, bool) {
        let until = deadline.min(self.task);
        (until, until < deadline)
    }

    fn stopped(&self, step: RunStep, at: Deadline) -> RunError {
        self.error(step, RunFault::Stopped(Stopped::Deadline { span: RUN_SPAN, at: at.get() }))
    }

    /// A read of `channel` by `deadline` (capped by the task's).
    fn recv<A: Allocator>(&self, channel: &mut Channel, into: &mut Vec<u8, A>, most: u32, deadline: Deadline) -> Result<Message, RunError> {
        let (until, task_first) = self.capped(deadline);
        match channel.recv_message(into, most, until, &DriverClock(&self.io)) {
            Ok(Message::Deadline) if task_first => Err(self.stopped(RunStep::Recv, until)),
            Ok(message) => Ok(message),
            Err(error) => Err(self.error(RunStep::Recv, RunFault::Channel(error))),
        }
    }
}

impl Run {
    /// Its process id; also its group's where it leads one.
    pub fn id(&self) -> ProcId {
        self.rest.proc
    }

    /// The driver's clock, which every deadline here is on.
    pub fn now(&self) -> Duration {
        self.rest.io.now()
    }

    /// How it ended by `deadline` (capped by the task's), or the run back, still running.
    pub fn wait(self, deadline: Deadline) -> Result<Waited<Run>, RunError> {
        self.wait_rest(deadline).map(|waited| match waited {
            Waited::Running(run) => Waited::Running(run),
            Waited::Ended((exit, _rest)) => Waited::Ended(exit),
        })
    }

    /// Ended by `SIGKILL` (its group where it leads one) and reaped: how it ended.
    pub fn kill(self) -> Result<Exit, RunError> {
        self.kill_rest().map(|(exit, _rest)| exit)
    }

    /// `SIGTERM` to it (its group where it leads one). See [`RunChild::terminate`].
    pub fn terminate(&mut self) -> Result<(), RunError> {
        self.child.terminate().map_err(|error| self.rest.error(RunStep::Terminate, RunFault::Host { what: error.op.what(), os: error.os }))
    }

    fn wait_rest(self, deadline: Deadline) -> Result<Waited<Run, (Exit, Rest)>, RunError> {
        let (until, task_first) = self.rest.capped(deadline);
        let Run { child, own_group, rest } = self;
        match child.wait_until_on(until, &DriverClock(&rest.io)) {
            // The child drops with the error: killed and reaped.
            Ok(Waited::Running(_)) if task_first => Err(rest.stopped(RunStep::Wait, until)),
            Ok(Waited::Running(child)) => Ok(Waited::Running(Run { child, own_group, rest })),
            Ok(Waited::Ended(exit)) => Ok(Waited::Ended((exit, rest))),
            Err(error) => Err(rest.error(RunStep::Wait, RunFault::Host { what: error.op.what(), os: error.os })),
        }
    }

    fn kill_rest(self) -> Result<(Exit, Rest), RunError> {
        let Run { child, own_group, rest } = self;
        let killed = match own_group {
            true => child.kill_group(),
            false => child.kill_now(),
        };
        match killed {
            Ok(exit) => Ok((exit, rest)),
            Err(error) => Err(rest.error(RunStep::Kill, RunFault::Host { what: error.op.what(), os: error.os })),
        }
    }
}

impl Served {
    /// Its process id; also its group's where it leads one.
    pub fn id(&self) -> ProcId {
        self.run.id()
    }
}

impl RunChild for Served {
    type Left = ServedEnd;

    fn now(&self) -> Duration {
        self.run.now()
    }

    fn recv_message<A: Allocator>(
        &mut self,
        into: &mut Vec<u8, A>,
        most: u32,
        deadline: Deadline,
    ) -> Result<Message, RunError> {
        self.run.rest.recv(&mut self.channel, into, most, deadline)
    }

    fn send(&mut self, bytes: &[u8], deadline: Deadline) -> Result<(), RunError> {
        let rest = &self.run.rest;
        let (until, task_first) = rest.capped(deadline);
        match self.channel.send(bytes, until, &DriverClock(&rest.io)) {
            Ok(()) => Ok(()),
            Err(ChannelError::WriteDeadline { .. }) if task_first => Err(rest.stopped(RunStep::Send, until)),
            Err(error) => Err(rest.error(RunStep::Send, RunFault::Channel(error))),
        }
    }

    fn wait(self, deadline: Deadline) -> Result<RunWaited<Served>, RunError> {
        let Served { run, channel } = self;
        Ok(match run.wait_rest(deadline)? {
            Waited::Running(run) => Waited::Running(Served { run, channel }),
            Waited::Ended((exit, rest)) => Waited::Ended(Reaped { exit, left: ServedEnd { channel, rest } }),
        })
    }

    fn kill(self) -> Result<Reaped<ServedEnd>, RunError> {
        let Served { run, channel } = self;
        let (exit, rest) = run.kill_rest()?;
        Ok(Reaped { exit, left: ServedEnd { channel, rest } })
    }

    fn terminate(&mut self) -> Result<(), RunError> {
        self.run.terminate()
    }
}

impl ServedEnd {
    /// The ended child's id (its pid is reaped: nothing is sent to it).
    pub fn id(&self) -> ProcId {
        self.rest.proc
    }

    /// The driver's clock, which every deadline here is on.
    pub fn now(&self) -> Duration {
        self.rest.io.now()
    }

    /// What the child wrote before its end, as [`RunChild::recv_message`] reads it; then
    /// [`Message::Closed`] (its end closed the channel).
    pub fn recv_message<A: Allocator>(&mut self, into: &mut Vec<u8, A>, most: u32, deadline: Deadline) -> Result<Message, RunError> {
        self.rest.recv(&mut self.channel, into, most, deadline)
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
        write!(f, "Run({})", self.rest.proc)
    }
}

impl fmt::Debug for Served {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Served({})", self.run.rest.proc)
    }
}

impl fmt::Debug for ServedEnd {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ServedEnd({})", self.rest.proc)
    }
}
