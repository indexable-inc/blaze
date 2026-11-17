//! A child process started linked to this one ([`Spawner::start_linked`]): given descriptors of
//! this process's (a socket on its descriptor 3), leading a process group of its own, waited
//! for by the task's deadline and ended with everything it started. One surface for the test
//! zygote, the run parent and later the sandbox's helper, over blaze-core's one spawn path
//! ([`blaze_core::linked`]).
//!
//! **Provider.** A native world on Linux and macOS ([`crate::host::Host::spawner`]); the
//! simulation has none (`Missing { capability: "spawner" }`): a real child is not simulated.
//!
//! **Waits.** [`Linked::wait_until`] blocks the calling thread in blaze-core's wait for a
//! child's exit (the wait ring or a pidfd on Linux, a kqueue on macOS), by the earlier of the
//! deadline asked and the task's, on the driver's clock. The task's cancellation is seen
//! before the wait, not during it: a caller that must answer a cancel at once waits in short
//! deadlines.
//!
//! What blaze-core refuses is refused here by name ([`blaze_core::linked::LinkOp`]: a sandbox,
//! a memory limit, input, a bad target), carried as [`Refused::Host`] with its words.

use alloc::{string::String, vec::Vec};
use core::time::Duration;

pub use blaze_core::linked::{BorrowedFd, Ended};
use blaze_core::{
    Command, Sandbox,
    linked::{Link, LinkError, LinkedChild},
    wait::Deadline,
};

use crate::{
    Stopped,
    cx::Cx,
    hosted::Refused,
    io::{Io, Missing},
    procs::{ProcId, ProcsError, ProcsOp},
};

#[cfg(test)]
mod tests;

/// The span a linked child's wait is named by in a [`Stopped::Deadline`].
const WAIT_SPAN: &str = "bstd::procs wait_linked";

/// What to start: the program (a path, or a name found on `env`'s `PATH`), its arguments, its
/// whole environment (nothing of this process's is inherited), its directory, the descriptors
/// it is given (each `(descriptor, target)`, targets 3 and up, each once) and whether it leads
/// a process group of its own.
pub struct LinkedSpec<'a> {
    pub program: &'a str,
    pub args: &'a [&'a str],
    pub env: &'a [(&'a str, &'a str)],
    pub dir: &'a str,
    pub inherit: &'a [(BorrowedFd<'a>, u32)],
    pub own_group: bool,
}

/// The authority to start child processes: from [`crate::host::Host::spawner`].
#[derive(Clone)]
pub struct Spawner {
    io: Io,
}

/// A started child, until it has ended and been waited for or its group killed. Dropped
/// before that, its group is killed and it is reaped (blaze-core's `LinkedChild`).
pub struct Linked {
    child: LinkedChild,
    io: Io,
}

/// The driver's clock, which a task's deadline is on.
struct DriverClock<'a>(&'a Io);

impl blaze_core::wait::Clock for DriverClock<'_> {
    fn now(&self) -> Duration {
        self.0.now()
    }
}

impl Spawner {
    /// The native world's spawner; the simulation's is missing.
    pub(crate) fn of(io: &Io) -> Result<Spawner, Missing> {
        match io.is_sim() {
            true => Err(Missing { capability: "spawner" }),
            false => Ok(Spawner { io: io.clone() }),
        }
    }

    /// `spec` started: its descriptors given, its group its own where asked. Refused before it
    /// starts when the task's deadline has passed.
    pub fn start_linked(&self, cx: &Cx, spec: &LinkedSpec<'_>) -> Result<Linked, ProcsError> {
        let refuse = |kind| ProcsError { op: ProcsOp::StartLinked, proc: None, kind };
        let deadline = cx.deadline();
        if deadline.passed(self.io.now()) {
            return Err(refuse(Refused::Stopped(Stopped::Deadline { span: WAIT_SPAN, at: deadline.get() })));
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
        let link = Link { inherit: spec.inherit, own_group: spec.own_group };
        match blaze_core::linked::start_linked(&command, &link) {
            Ok(child) => Ok(Linked { child, io: self.io.clone() }),
            Err(error) => Err(refuse(Refused::Host(error.op.what()))),
        }
    }
}

impl Linked {
    /// Its process id; also its group's where it leads one.
    pub fn id(&self) -> ProcId {
        ProcId(self.child.pid())
    }

    /// Waits until it ends or `deadline` (on the driver's clock) passes: how it ended, or
    /// [`Ended::Deadline`] with it still running. Where the task's deadline is earlier and
    /// passes first, the wait stops with [`Stopped::Deadline`].
    pub fn wait_until(&self, cx: &Cx, deadline: Deadline) -> Result<Ended, ProcsError> {
        let task = cx.deadline();
        let until = deadline.min(task);
        match self.child.wait_until_on(until, &DriverClock(&self.io)) {
            Ok(Ended::Deadline) if until < deadline => Err(self.refuse(
                ProcsOp::WaitLinked,
                Refused::Stopped(Stopped::Deadline { span: WAIT_SPAN, at: until.get() }),
            )),
            Ok(ended) => Ok(ended),
            Err(error) => Err(self.refused(ProcsOp::WaitLinked, &error)),
        }
    }

    /// `SIGKILL` to its whole group, then its reap: nothing it started is left.
    pub fn kill_group(&self, cx: &Cx) -> Result<(), ProcsError> {
        let _ = cx;
        self.child.kill_group().map_err(|error| self.refused(ProcsOp::KillGroup, &error))
    }

    fn refuse(&self, op: ProcsOp, kind: Refused) -> ProcsError {
        ProcsError { op, proc: Some(self.id()), kind }
    }

    fn refused(&self, op: ProcsOp, error: &LinkError) -> ProcsError {
        self.refuse(op, Refused::Host(error.op.what()))
    }
}

impl core::fmt::Debug for Linked {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "Linked({})", self.child.pid())
    }
}

impl core::fmt::Debug for Spawner {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("Spawner")
    }
}
