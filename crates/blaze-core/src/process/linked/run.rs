//! A linked child whose standard streams, `argv[0]` and fate at its parent's death are the
//! caller's typed choice ([`start_run`]): the one start every linked child goes through
//! ([`super::start_linked`] is [`start_run`] with [`Streams::PIPED`], no `argv[0]` of its own
//! and [`ParentDeath::DropOnly`]).
//!
//! - **Streams.** Each of standard output and error is [`Output::Inherit`] (a copy of this
//!   process's own descriptor 1 or 2: `blaze run`'s program writes to the command's terminal or
//!   pipe), [`Output::Pipe`] (a pipe whose read end the caller takes,
//!   [`LinkedChild::take_stdout`]: `blaze test` reads it by deadline) or [`Output::Null`]
//!   (`/dev/null`). Standard input is [`Input::Inherit`] or [`Input::Null`]. Nothing is
//!   chosen for the caller: a [`RunLink`] names all three.
//! - **`argv[0]`.** `arg0`, where given, is the child's `argv[0]` (the program it runs is still
//!   `command.program`): the run parent starts the launcher under the binary target's name.
//! - **A child never outlives its parent.** [`ParentDeath`] says how far that holds; see it.
//!   On every path that returns or unwinds, the drop of [`LinkedChild`] kills and reaps it.

use std::{ffi::OsStr, os::fd::{AsRawFd as _, OwnedFd, RawFd}, sync::atomic::Ordering};

use super::{BorrowedFd, LinkError, LinkOp, LinkedChild, MOST_INHERITED, copy_past, os_of};
use crate::{Command, Sandbox, wait::Monotonic};

/// Where a linked child's standard input comes from.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Input {
    /// A copy of this process's descriptor 0: the command's own input (`blaze run`).
    Inherit,
    /// `/dev/null`: a read sees the end at once.
    Null,
}

/// Where one of a linked child's standard output and error goes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Output {
    /// A copy of this process's own descriptor (1 for output, 2 for error).
    Inherit,
    /// A pipe; its read end is the caller's ([`LinkedChild::take_stdout`],
    /// [`LinkedChild::take_stderr`]). Unread, it fills and the child blocks on its next write.
    Pipe,
    /// `/dev/null`: what it writes is dropped.
    Null,
}

/// The three standard streams of a linked child.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Streams {
    pub stdin: Input,
    pub stdout: Output,
    pub stderr: Output,
}

impl Streams {
    /// `blaze run`'s program: the command's own input, output and error.
    pub const INHERIT: Streams = Streams { stdin: Input::Inherit, stdout: Output::Inherit, stderr: Output::Inherit };
    /// A child whose output the caller reads (`blaze test`, the test zygote): no input, both
    /// outputs piped. What [`super::start_linked`] has always given.
    pub const PIPED: Streams = Streams { stdin: Input::Null, stdout: Output::Pipe, stderr: Output::Pipe };
}

/// What ends a linked child when its parent dies without running the child's drop (killed by
/// `SIGKILL`, or ended by a signal whose default is to end it).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ParentDeath {
    /// The kernel kills it with `SIGKILL` (Linux: `prctl(PR_SET_PDEATHSIG, SIGKILL)` in the
    /// child before its program runs, then `getppid` checked against this process so a parent
    /// that died before the arming is seen). Its limits, from prctl(2): "the 'parent' in this
    /// case is considered to be the thread that created this process": the signal comes when
    /// the THREAD that called [`start_run`] ends, so the caller starts it from a thread that
    /// lives as long as the child (the run parent's own); the setting is cleared by an
    /// `execve` of a set-user-ID or set-group-ID program or one with file capabilities; it
    /// reaches the child alone, not what the child started. macOS has no such mechanism:
    /// refused there by name ([`LinkOp::NoParentDeathSignal`]).
    Kernel,
    /// The child ends itself when its channel ([`super::channel`]) hangs up: blaze's launcher
    /// sees its descriptor 3 end and stops. Nothing here arms it; the child's own code holds
    /// the promise. The only form macOS has.
    Channel,
    /// Only the drop's kill: a parent killed by `SIGKILL` leaves the child running.
    DropOnly,
}

impl ParentDeath {
    /// The strongest this system gives: [`ParentDeath::Kernel`] on Linux,
    /// [`ParentDeath::Channel`] elsewhere.
    #[cfg(target_os = "linux")]
    pub const STRONGEST: ParentDeath = ParentDeath::Kernel;
    /// The strongest this system gives: [`ParentDeath::Kernel`] on Linux,
    /// [`ParentDeath::Channel`] elsewhere.
    #[cfg(not(target_os = "linux"))]
    pub const STRONGEST: ParentDeath = ParentDeath::Channel;
}

/// How a linked child is started: descriptors it is given (`(descriptor, target)`, targets 3
/// and up, each once), whether it leads a process group of its own, its streams, its
/// `argv[0]` and what ends it at its parent's death.
pub struct RunLink<'a> {
    pub inherit: &'a [(BorrowedFd<'a>, u32)],
    pub own_group: bool,
    pub streams: Streams,
    pub arg0: Option<&'a OsStr>,
    pub parent_death: ParentDeath,
}

/// `command` started linked as `link` says, listed until reaped. Refused by name: `Sandbox::On`,
/// a memory limit, input bytes, a target below 3 or named twice, more than
/// [`MOST_INHERITED`] descriptors, and [`ParentDeath::Kernel`] where the system has none.
pub fn start_run(command: &Command, link: &RunLink<'_>) -> Result<LinkedChild, LinkError> {
    let refuse = |op: LinkOp, os: i32| LinkError {
        // clone: an error path, the program's path.
        program: command.program.clone(),
        op,
        os,
    };
    if !matches!(command.sandbox, Sandbox::Off) {
        return Err(refuse(LinkOp::SandboxNotJoined, 0));
    }
    if command.memory.is_some() {
        return Err(refuse(LinkOp::MemoryNotHeld, 0));
    }
    if !command.stdin.is_empty() {
        return Err(refuse(LinkOp::InputNotWritten, 0));
    }
    #[cfg(not(target_os = "linux"))]
    if link.parent_death == ParentDeath::Kernel {
        return Err(refuse(LinkOp::NoParentDeathSignal, 0));
    }
    if link.inherit.len() > MOST_INHERITED {
        return Err(refuse(LinkOp::TooManyDescriptors, 0));
    }
    // Every target past 2 and named once; the copies go past the highest, so no `dup2` in the
    // child lands on a source a later one still reads (`spawn_with`).
    let mut highest: RawFd = 2;
    for (at, &(_, target)) in link.inherit.iter().enumerate() {
        let Ok(target) = RawFd::try_from(target) else { return Err(refuse(LinkOp::Target, 0)) };
        let twice = link.inherit[..at].iter().any(|&(_, it)| RawFd::try_from(it) == Ok(target));
        if target < 3 || twice {
            return Err(refuse(LinkOp::Target, 0));
        }
        highest = highest.max(target);
    }
    // The parent's copies, closed on exec and when this returns: the child has its own by then.
    let mut copies: [Option<OwnedFd>; MOST_INHERITED] = Default::default();
    let mut extra: [(RawFd, RawFd); MOST_INHERITED] = [(-1, -1); MOST_INHERITED];
    for (at, (fd, target)) in link.inherit.iter().enumerate() {
        let copy = copy_past(*fd, highest + 1).map_err(|error| refuse(LinkOp::Copy, os_of(&error)))?;
        // `target` was checked to fit a `RawFd` above.
        extra[at] = (copy.as_raw_fd(), *target as RawFd);
        copies[at] = Some(copy);
    }
    let program = super::super::spawn::Program {
        program: &command.program,
        args: &command.args,
        env: &command.env,
        dir: &command.dir,
    };
    let start = super::super::spawn::streams::Start {
        arg0: link.arg0,
        streams: link.streams,
        extra: &extra[..link.inherit.len()],
        group: link.own_group,
        parent_death: link.parent_death == ParentDeath::Kernel,
    };
    let spawned = super::super::spawn::streams::spawn_streams(&program, &start)
        .map_err(|error| refuse(LinkOp::Spawn, os_of(&error)))?;
    drop(copies);
    let run = super::super::RUNS.fetch_add(1, Ordering::Relaxed);
    let listed = super::super::Listed::new(spawned.pid, run, &command.program);
    #[cfg(target_os = "macos")]
    let watch = match super::watch_exit(spawned.pid) {
        Ok(watch) => watch,
        Err(error) => {
            // Not left running: killed (with its group where it has one) and reaped.
            let target = if link.own_group { -spawned.pid } else { spawned.pid };
            // SAFETY: no pointers; `spawned.pid` is this process's child, not yet reaped, so
            // it and the group it leads are no one else's.
            unsafe { libc::kill(target, libc::SIGKILL) };
            _ = super::super::reap(spawned.pid);
            drop(listed);
            return Err(refuse(LinkOp::Watch, os_of(&error)));
        }
    };
    Ok(LinkedChild {
        pid: spawned.pid,
        // clone: once per linked start, the program's path its errors name.
        program: command.program.clone(),
        own_group: link.own_group,
        clock: Monotonic::new(),
        ended: std::cell::Cell::new(None),
        stdout: spawned.stdout,
        stderr: spawned.stderr,
        #[cfg(target_os = "linux")]
        _pidfd: spawned.pidfd,
        #[cfg(target_os = "macos")]
        watch,
        listed,
    })
}

impl LinkedChild {
    /// The read end of its standard output, once, where it was [`Output::Pipe`].
    pub fn take_stdout(&mut self) -> Option<OwnedFd> {
        self.stdout.take()
    }

    /// The read end of its standard error, once, where it was [`Output::Pipe`].
    pub fn take_stderr(&mut self) -> Option<OwnedFd> {
        self.stderr.take()
    }

    /// `SIGKILL` to it alone (not its group), then its reap: how it ended. A child that ended
    /// already is left as it ended. For a child started without a group of its own, which
    /// [`LinkedChild::kill_group`] refuses.
    pub fn kill_now(&self) -> Result<super::Ended, LinkError> {
        if let Some(ended) = self.ended.get() {
            return Ok(ended);
        }
        // SAFETY: no pointers; the pid is unreaped (`ended` is none), so it is this process's
        // child and no one else's.
        let sent = unsafe { libc::kill(self.pid, libc::SIGKILL) };
        if sent != 0 {
            let os = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
            // An unreaped child is a zombie at worst, which `kill` still finds; ESRCH would
            // mean the pid is not ours, which the unreaped state rules out.
            return Err(self.error(LinkOp::KillGroup, os));
        }
        self.reap_now()
    }

    /// SIGTERM to it (to its group where it leads one): asked to end. It may handle or ignore
    /// the signal, so nothing is waited for here: its end is waited for as any other's, by the
    /// caller's deadline (the run parent's grace, D103). A child that has ended is left as it
    /// ended, and nothing is sent (its pid is reaped and may be another's).
    pub fn terminate(&self) -> Result<(), LinkError> {
        if self.ended.get().is_some() {
            return Ok(());
        }
        let target = if self.own_group { -self.pid } else { self.pid };
        // SAFETY: no pointers; the pid is unreaped (`ended` is none), so it, and the group it
        // leads where `own_group`, is this process's child and no one else's.
        let sent = unsafe { libc::kill(target, libc::SIGTERM) };
        if sent != 0 {
            let os = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
            return Err(self.error(LinkOp::Terminate, os));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    //! Run on the host with `/bin/sh` (POSIX: `sh -c`, `exit n`, `kill -s KILL $$`, `printf`
    //! with octal escapes). The platform's rules quoted: waitpid(2), "WIFEXITED ... returns
    //! true if the child terminated normally ... WEXITSTATUS ... the least significant 8 bits
    //! of the status argument that the child specified in a call to exit(3) or _exit(2)";
    //! "WIFSIGNALED ... returns true if the child process was terminated by a signal ...
    //! WTERMSIG ... returns the number of the signal".

    use std::{ffi::OsString, os::fd::AsFd as _, path::PathBuf, time::Duration};

    use super::*;
    use crate::{
        linked::{
            Ended,
            channel::{self, Message},
        },
        wait::{Clock as _, Deadline},
    };

    fn sh(script: &str) -> Command {
        Command {
            program: PathBuf::from("/bin/sh"),
            args: vec![OsString::from("-c"), OsString::from(script)],
            env: vec![(OsString::from("PATH"), OsString::from("/bin:/usr/bin"))],
            dir: PathBuf::from("/"),
            stdin: Vec::new(),
            sandbox: Sandbox::Off,
            memory: None,
        }
    }

    fn link<'a>(inherit: &'a [(BorrowedFd<'a>, u32)], streams: Streams) -> RunLink<'a> {
        RunLink { inherit, own_group: false, streams, arg0: None, parent_death: ParentDeath::DropOnly }
    }

    /// Prints "linked run: exit 7, signal 9, deadline then killed, a message on 3, stdout piped".
    /// A start that ignored `Streams` (piping by default) would leave `take_stdout` of a
    /// `Null` start `Some`; a wait that reported a signal as an exit fails the second line.
    #[test]
    fn a_run_child_exits_dies_hangs_and_speaks_on_its_channel() {
        let null = Streams { stdin: Input::Null, stdout: Output::Null, stderr: Output::Null };
        let bound = Deadline::at(Duration::from_secs(20));

        let mut child = start_run(&sh("exit 7"), &link(&[], null)).expect("sh starts");
        assert!(child.take_stdout().is_none(), "Output::Null gives the caller no pipe");
        assert_eq!(child.wait_until(bound).expect("a wait"), Ended::Exited(7));

        let child = start_run(&sh("kill -s KILL $$"), &link(&[], null)).expect("sh starts");
        assert_eq!(child.wait_until(bound).expect("a wait"), Ended::Signaled(libc::SIGKILL));

        let child = start_run(&sh("exec sleep 30"), &link(&[], null)).expect("sh starts");
        let soon = Deadline::after(child.clock().now(), Duration::from_millis(100));
        assert_eq!(child.wait_until(soon).expect("a wait"), Ended::Deadline, "it still runs");
        assert_eq!(child.kill_now().expect("a kill"), Ended::Signaled(libc::SIGKILL));

        // A two-byte message "hi" behind its four-byte big-endian length, on descriptor 3.
        let (mut parent, end) = channel::pair().expect("a socket pair");
        let given = [(end.as_fd(), channel::CHILD_FD)];
        let child = start_run(&sh("printf '\\000\\000\\000\\002hi' >&3"), &link(&given, null)).expect("sh starts");
        drop(end);
        let clock = Monotonic::new();
        let mut inbox = Vec::new();
        let got = parent.recv_message(&mut inbox, 64, Deadline::after(clock.now(), Duration::from_secs(20)), &clock);
        assert_eq!(got.expect("a message"), Message::Whole { len: 6 });
        assert_eq!(&inbox[..], b"\0\0\0\x02hi");
        assert_eq!(child.wait_until(bound).expect("a wait"), Ended::Exited(0));
        inbox.clear();
        let after = parent.recv_message(&mut inbox, 64, Deadline::after(clock.now(), Duration::from_secs(20)), &clock);
        assert_eq!(after.expect("the end"), Message::Closed, "the child's end closed at its exit");

        let piped = Streams { stdin: Input::Null, stdout: Output::Pipe, stderr: Output::Null };
        let mut child = start_run(&sh("printf out"), &link(&[], piped)).expect("sh starts");
        let out = channel::DeadlineFd::new(child.take_stdout().expect("a pipe"), "its stdout").expect("nonblocking");
        assert_eq!(child.wait_until(bound).expect("a wait"), Ended::Exited(0));
        let mut buf = [0u8; 16];
        let read = out.read_some(&mut buf, Deadline::after(clock.now(), Duration::from_secs(20)), &clock).expect("a read");
        assert_eq!(read, channel::Read::Bytes(3));
        assert_eq!(&buf[..3], b"out");
        println!("linked run: exit 7, signal 9, deadline then killed, a message on 3, stdout piped");
    }
}
