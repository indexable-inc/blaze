//! A linked child whose standard streams, `argv[0]` and fate at its parent's death are the
//! caller's typed choice ([`start_run`]): the one start every linked child goes through
//! ([`super::start_linked`] is [`start_run`] with [`Streams::PIPED`], no `argv[0]` of its own
//! and [`ParentDeath::DropOnly`]).
//!
//! - **Streams.** Each of standard output and error is [`Output::Inherit`] (a copy of this
//!   process's own descriptor 1 or 2: `blaze run`'s program writes to the command's terminal or
//!   pipe), [`Output::Pipe`] (a pipe whose read end is given once with the child,
//!   [`Started::stdout`]: `blaze test` reads it by deadline) or [`Output::Null`]
//!   (`/dev/null`). Standard input is [`Input::Inherit`] or [`Input::Null`]. Nothing is
//!   chosen for the caller: a [`RunLink`] names all three.
//! - **`argv[0]`.** `arg0`, where given, is the child's `argv[0]` (the program it runs is still
//!   `command.program`): the run parent starts the launcher under the binary target's name.
//! - **A child never outlives its parent.** [`ParentDeath`] says how far that holds; see it.
//!   On every path that returns or unwinds, the drop of [`LinkedChild`] kills and reaps it.

use std::{ffi::OsStr, os::fd::{AsRawFd as _, OwnedFd, RawFd}, sync::atomic::Ordering};

use super::{BorrowedFd, Exit, LinkError, LinkOp, LinkedChild, MOST_INHERITED, Started, copy_past, os_of};
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
    /// A pipe; its read end is the caller's, given once ([`Started::stdout`],
    /// [`Started::stderr`]). Unread, it fills and the child blocks on its next write.
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

/// A child's confinement, entered in the child before its program runs (D177): DATA the parent
/// made, never a closure. On Linux blaze-sys's [`blaze_sys::linux::confine::ChildConfine`]
/// (a Landlock ruleset, a seccomp filter, resource limits), built by blaze-sys, carried here
/// and run by the child of `spawn.rs` through `blaze_sys::linux::confine::enter`. The type is
/// blaze-sys's because blaze-sys builds the filter and is the one crate below both blaze-core
/// (which runs it) and bstd (which asks for it); blaze-core already depends on blaze-sys on
/// Linux and macOS and gains no dependency.
#[cfg(target_os = "linux")]
pub use blaze_sys::linux::confine::ChildConfine;

/// Elsewhere none can be made: the type has no value, so a `Some` does not compile. macOS's
/// confinement is Seatbelt, which `posix_spawn` cannot apply: no public spawn attribute takes a
/// sandbox profile (the private `posix_spawnattr_setmacpolicyinfo_np` takes a policy's opaque
/// data, whose form for the Sandbox policy is undocumented), so bstd's backend runs the
/// program under `/usr/bin/sandbox-exec` instead.
#[cfg(not(target_os = "linux"))]
pub struct ChildConfine<'a> {
    never: core::convert::Infallible,
    _of: core::marker::PhantomData<&'a ()>,
}

/// How a linked child is started: descriptors it is given (`(descriptor, target)`, targets 3
/// and up, each once), whether it leads a process group of its own, its streams, its
/// `argv[0]`, what ends it at its parent's death, and the confinement it enters before its
/// program runs (`None`: none; bstd's hermetic run is the one caller that gives one).
pub struct RunLink<'a> {
    pub inherit: &'a [(BorrowedFd<'a>, u32)],
    pub own_group: bool,
    pub streams: Streams,
    pub arg0: Option<&'a OsStr>,
    pub parent_death: ParentDeath,
    pub confine: Option<&'a ChildConfine<'a>>,
}

fn refuse_command(command: &Command, op: LinkOp, os: i32) -> LinkError {
    LinkError {
        // clone: an error path, the program's path.
        program: command.program.clone(),
        op,
        os,
    }
}

/// `command` started linked as `link` says, listed until reaped, with the read ends of its
/// piped streams. Refused by name: `Sandbox::On`, a memory limit, input bytes, a target below
/// 3 or named twice, more than [`MOST_INHERITED`] descriptors, and [`ParentDeath::Kernel`]
/// where the system has none.
pub fn start_run(command: &Command, link: &RunLink<'_>) -> Result<Started, LinkError> {
    let refuse = |op: LinkOp, os: i32| refuse_command(command, op, os);
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
    // No confinement can be made here (the type has no value).
    #[cfg(not(target_os = "linux"))]
    if let Some(confine) = link.confine {
        match confine.never {}
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
        session: false,
        parent_death: link.parent_death == ParentDeath::Kernel,
        confine: link.confine,
    };
    let spawned = super::super::spawn::streams::spawn_streams(&program, &start).map_err(|error| match error {
        super::super::spawn::StartError::Io(error) => refuse(LinkOp::Spawn, os_of(&error)),
        #[cfg(target_os = "linux")]
        super::super::spawn::StartError::Confine { step, errno } => refuse(LinkOp::Confine(step), i32::from(errno.raw())),
    })?;
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
    let child = LinkedChild {
        pid: spawned.pid,
        // clone: once per linked start, the program's path its errors name.
        program: command.program.clone(),
        own_group: link.own_group,
        clock: Monotonic::new(),
        reaped: false,
        #[cfg(target_os = "linux")]
        _pidfd: spawned.pidfd,
        #[cfg(target_os = "macos")]
        watch,
        listed,
    };
    Ok(Started { child, stdout: spawned.stdout, stderr: spawned.stderr })
}

/// `command` started detached in a new session (`setsid` on Linux, `POSIX_SPAWN_SETSID` on macOS)
/// with `/dev/null` for stdin, stdout and stderr and every other descriptor closed on exec, so the
/// parent never blocks on inherited pipes or descriptors and the child outlives the parent. Not
/// registered among linked/reaped children; returns the started process id.
pub fn start_detached(command: &Command) -> Result<u32, LinkError> {
    let refuse = |op: LinkOp, os: i32| refuse_command(command, op, os);
    if !matches!(command.sandbox, Sandbox::Off) {
        return Err(refuse(LinkOp::SandboxNotJoined, 0));
    }
    if command.memory.is_some() {
        return Err(refuse(LinkOp::MemoryNotHeld, 0));
    }
    if !command.stdin.is_empty() {
        return Err(refuse(LinkOp::InputNotWritten, 0));
    }
    let program = super::super::spawn::Program {
        program: &command.program,
        args: &command.args,
        env: &command.env,
        dir: &command.dir,
    };
    let start = super::super::spawn::streams::Start {
        arg0: None,
        streams: Streams { stdin: Input::Null, stdout: Output::Null, stderr: Output::Null },
        extra: &[],
        group: false,
        session: true,
        parent_death: false,
        confine: None,
    };
    let spawned = super::super::spawn::streams::spawn_streams(&program, &start).map_err(|error| match error {
        super::super::spawn::StartError::Io(error) => refuse(LinkOp::Spawn, os_of(&error)),
        #[cfg(target_os = "linux")]
        super::super::spawn::StartError::Confine { step, errno } => refuse(LinkOp::Confine(step), i32::from(errno.raw())),
    })?;
    Ok(u32::try_from(spawned.pid).unwrap_or(0))
}

impl LinkedChild {
    /// `SIGKILL` to it alone (not its group), then its reap: how it ended (a child that exited
    /// before the kill is a zombie the kill finds, and its reap reads its own end). For a child
    /// started without a group of its own, which [`LinkedChild::kill_group`] refuses.
    pub fn kill_now(self) -> Result<Exit, LinkError> {
        // SAFETY: no pointers; the pid is unreaped (this value lives), so it is this process's
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
    /// caller's deadline (the run parent's grace, D103). A reaped child has no value to call
    /// this on, so nothing is ever sent to a pid that may be another's.
    pub fn terminate(&self) -> Result<(), LinkError> {
        let target = if self.own_group { -self.pid } else { self.pid };
        // SAFETY: no pointers; the pid is unreaped (this value lives), so it, and the group it
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
            Waited,
            channel::{self, Message},
        },
        wait::{Clock as _, Deadline},
    };

    /// The end `child` came to by `bound`; a child still running then fails the test.
    fn ended(child: LinkedChild, bound: Deadline) -> Exit {
        match child.wait_until(bound).expect("a wait") {
            Waited::Ended(exit) => exit,
            Waited::Running(_) => panic!("still running at the test's bound"),
        }
    }

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
        RunLink { inherit, own_group: false, streams, arg0: None, parent_death: ParentDeath::DropOnly, confine: None }
    }

    /// Prints "linked run: exit 7, signal 9, deadline then killed, a message on 3, stdout piped".
    /// A start that ignored `Streams` (piping by default) would leave `take_stdout` of a
    /// `Null` start `Some`; a wait that reported a signal as an exit fails the second line.
    #[test]
    fn a_run_child_exits_dies_hangs_and_speaks_on_its_channel() {
        let null = Streams { stdin: Input::Null, stdout: Output::Null, stderr: Output::Null };
        let bound = Deadline::at(Duration::from_secs(20));

        let started = start_run(&sh("exit 7"), &link(&[], null)).expect("sh starts");
        assert!(started.stdout.is_none(), "Output::Null gives the caller no pipe");
        assert_eq!(ended(started.child, bound), Exit::Exited(7));

        let started = start_run(&sh("kill -s KILL $$"), &link(&[], null)).expect("sh starts");
        assert_eq!(ended(started.child, bound), Exit::Signaled(libc::SIGKILL));

        let child = start_run(&sh("exec sleep 30"), &link(&[], null)).expect("sh starts").child;
        let soon = Deadline::after(child.clock().now(), Duration::from_millis(100));
        let Waited::Running(child) = child.wait_until(soon).expect("a wait") else { panic!("it still runs") };
        assert_eq!(child.kill_now().expect("a kill"), Exit::Signaled(libc::SIGKILL));

        // A two-byte message "hi" behind its four-byte big-endian length, on descriptor 3.
        let (mut parent, end) = channel::pair().expect("a socket pair");
        let given = [(end.as_fd(), channel::CHILD_FD)];
        let child = start_run(&sh("printf '\\000\\000\\000\\002hi' >&3"), &link(&given, null)).expect("sh starts").child;
        drop(end);
        let clock = Monotonic::new();
        let mut inbox = Vec::new();
        let got = parent.recv_message(&mut inbox, 64, Deadline::after(clock.now(), Duration::from_secs(20)), &clock);
        assert_eq!(got.expect("a message"), Message::Whole { len: 6 });
        assert_eq!(&inbox[..], b"\0\0\0\x02hi");
        assert_eq!(ended(child, bound), Exit::Exited(0));
        inbox.clear();
        let after = parent.recv_message(&mut inbox, 64, Deadline::after(clock.now(), Duration::from_secs(20)), &clock);
        assert_eq!(after.expect("the end"), Message::Closed, "the child's end closed at its exit");

        let piped = Streams { stdin: Input::Null, stdout: Output::Pipe, stderr: Output::Null };
        let started = start_run(&sh("printf out"), &link(&[], piped)).expect("sh starts");
        let out = channel::DeadlineFd::new(started.stdout.expect("a pipe"), "its stdout").expect("nonblocking");
        assert_eq!(ended(started.child, bound), Exit::Exited(0));
        let mut buf = [0u8; 16];
        let read = out.read_some(&mut buf, Deadline::after(clock.now(), Duration::from_secs(20)), &clock).expect("a read");
        assert_eq!(read, channel::Read::Bytes(3));
        assert_eq!(&buf[..3], b"out");
        println!("linked run: exit 7, signal 9, deadline then killed, a message on 3, stdout piped");
    }

    #[test]
    fn start_detached_starts_child_in_new_session_with_null_stdio() {
        let pid = start_detached(&sh("exec sleep 30")).expect("detached child starts");
        assert!(pid > 0, "detached child has a positive pid: {pid}");
        let child_pid = libc::pid_t::try_from(pid).expect("pid fits pid_t");
        // SAFETY: POSIX `getsid(pid)` reads the session id of our live child process.
        let sid = unsafe { libc::getsid(child_pid) };
        // SAFETY: POSIX `getsid(0)` reads this process's own session id.
        let own_sid = unsafe { libc::getsid(0) };
        // Clean up the test child immediately before asserting so no process is leaked on failure.
        // SAFETY: `child_pid` is our unreaped child process.
        unsafe { libc::kill(child_pid, libc::SIGKILL) };
        let _ = super::super::super::reap(child_pid);
        assert_eq!(sid, child_pid, "detached child leads its own session (sid == pid)");
        assert_ne!(sid, own_sid, "detached child is in a different session from its parent");
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    fn open_fd_count() -> usize {
        // SAFETY: `F_GETFD` inspects descriptor existence without side effects; closed fds return -1/EBADF.
        (0..1024).filter(|&fd| unsafe { libc::fcntl(fd, libc::F_GETFD) } >= 0).count()
    }

    /// Spawning many linked children (piped stdout/stderr, channel on fd 3, own process group)
    /// must not leak file descriptors or leave unreaped zombies, and a spawn failure must
    /// preserve its OS errno (e.g. ENOENT) instead of returning 0.
    #[test]
    fn many_linked_spawns_leak_no_descriptors_or_zombies_and_preserve_errno() {
        let bound = Deadline::at(Duration::from_secs(30));
        let before_fds = open_fd_count();
        for _ in 0..64 {
            let (parent, end) = channel::pair().expect("a socket pair");
            let given = [(end.as_fd(), channel::CHILD_FD)];
            let run_link = RunLink {
                inherit: &given,
                own_group: true,
                streams: Streams::PIPED,
                arg0: None,
                parent_death: ParentDeath::DropOnly,
                confine: None,
            };
            let started = start_run(&sh("exit 0"), &run_link).expect("sh starts");
            drop(end);
            assert_eq!(ended(started.child, bound), Exit::Exited(0));
            drop(started.stdout);
            drop(started.stderr);
            drop(parent);
        }
        let after_fds = open_fd_count();
        // Each iteration opens 4+ FDs (socketpair, stdout pipe, stderr pipe, kqueue/pidfd,
        // /dev/null); even a 1-FD leak per spawn would grow open FDs by >= 64.
        assert!(
            after_fds.saturating_sub(before_fds) < 16,
            "FD leak across 64 linked spawns: before={before_fds}, after={after_fds}"
        );
        let mut missing = sh("");
        missing.program = PathBuf::from("/nonexistent/blaze-spawn-errno-check");
        missing.args.clear();
        missing.env.clear();
        let err = start_run(&missing, &link(&[], Streams::PIPED)).err().expect("missing binary must fail");
        assert_eq!(err.op, LinkOp::Spawn);
        assert_eq!(err.os, libc::ENOENT, "expected ENOENT, got {err}");
    }
}

// The child's confine step (D177), on the host's kernel: Landlock and seccomp entered between
// `clone` and `execve`.
#[cfg(all(test, target_os = "linux"))]
mod confine_tests;
