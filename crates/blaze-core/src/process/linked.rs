//! A process started linked to its parent: given descriptors of the parent's (a socket on its
//! descriptor 3, as the test zygote and the run parent hand their child), leading a process
//! group of its own, waited for by a deadline and ended with all it started by one signal to
//! the group. The one spawn path ([`super::spawn::spawn_with`]) starts it; it is listed among
//! the children not reaped ([`super::unreaped`]) from its start to its reap, as every child of
//! [`super::start`] is.
//!
//! What it does not do is refused by name ([`LinkOp`]): a sandbox (`Sandbox::On`: the
//! sandbox's helper joins this surface later, never by starting the program outside it), a
//! memory limit, and input written to its standard input. Its standard input is `/dev/null`;
//! its standard output and error are pipes the caller takes ([`LinkedChild::take_outputs`])
//! and reads: unread, a pipe fills and the child blocks on its next write.
//!
//! Waits: on Linux [`crate::wait::exited`] (the thread's wait ring, or a pidfd), on macOS a
//! kqueue holding `EVFILT_PROC`/`NOTE_EXIT` for the pid, waited on through
//! [`crate::wait::until`]; both by the deadline asked, never forever, and both leave the child
//! unreaped until [`super::reap`], so its pid (and its group's id) is no one else's until then.

#[cfg(test)]
mod tests;

/// The parent's end of a socket pair given to the child as descriptor 3, read and written by
/// deadline in whole length-prefixed messages.
pub mod channel;
/// The start every linked child goes through: streams, argv[0], the kill at the parent's death.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub mod run;

pub use std::os::fd::BorrowedFd;
use std::{
    cell::Cell,
    fmt, io,
    os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd, RawFd},
    path::PathBuf,
    time::Duration,
};

use crate::{
    Command,
    wait::{Clock, Deadline, Monotonic},
};

/// The most descriptors a linked child is given beyond its standard three: a socket and a few
/// more; past it the start is refused by name ([`LinkOp::TooManyDescriptors`]), so the copies
/// sit in an array on the stack.
pub const MOST_INHERITED: usize = 8;

/// How a linked child is started: each `(descriptor, target)` of `inherit` is the child's
/// descriptor `target` (3 and up, each once), a copy of the parent's; with `own_group` it leads
/// a process group of its own, which [`LinkedChild::kill_group`] ends.
pub struct Link<'a> {
    pub inherit: &'a [(BorrowedFd<'a>, u32)],
    pub own_group: bool,
}

/// How a linked child ended, or that the deadline came first.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Ended {
    /// It exited with this status.
    Exited(i32),
    /// A signal of this number ended it.
    Signaled(i32),
    /// The deadline passed with it still running.
    Deadline,
}

/// What a step on a linked child was, for its error.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LinkOp {
    /// The command asks for a sandbox, and this surface does not join one: nothing runs
    /// outside the sandbox it asks for.
    SandboxNotJoined,
    /// The command asks for a memory limit, which this surface does not hold.
    MemoryNotHeld,
    /// The command has input for its standard input, which this surface does not write.
    InputNotWritten,
    /// A target descriptor below 3 (a standard one) or named twice.
    Target,
    /// More than [`MOST_INHERITED`] descriptors to give.
    TooManyDescriptors,
    /// Copying an inherited descriptor past every target (`F_DUPFD_CLOEXEC`).
    Copy,
    /// Starting it.
    Spawn,
    /// Making the watch of its exit (macOS: the kqueue and its `EVFILT_PROC`).
    Watch,
    /// Waiting for its exit.
    Wait,
    /// Reaping it, or reading its status.
    Reap,
    /// Signalling its group.
    KillGroup,
    /// Sending it SIGTERM (`LinkedChild::terminate`: the run parent forwarding the SIGTERM it
    /// was sent, D103).
    Terminate,
    /// A group was asked to be killed of a child started without one of its own.
    NotOwnGroup,
    /// The kernel's kill at the parent's death (`run::ParentDeath::Kernel`) is asked, and this
    /// system has none (macOS).
    NoParentDeathSignal,
}

impl LinkOp {
    /// What the step is, in words.
    pub fn what(self) -> &'static str {
        match self {
            LinkOp::SandboxNotJoined => {
                "a sandbox is asked for, and a linked start joins none (Sandbox::On is refused, never run outside it)"
            }
            LinkOp::MemoryNotHeld => "a memory limit is asked for, and a linked start holds none",
            LinkOp::InputNotWritten => {
                "input for its standard input is given, and a linked start writes none (its input is /dev/null)"
            }
            LinkOp::Target => "an inherited descriptor's target is below 3 or named twice",
            LinkOp::TooManyDescriptors => "more inherited descriptors than MOST_INHERITED",
            LinkOp::Copy => "copying an inherited descriptor past every target",
            LinkOp::Spawn => "starting it",
            LinkOp::Watch => "watching for its exit",
            LinkOp::Wait => "waiting for its exit",
            LinkOp::Reap => "reaping it",
            LinkOp::KillGroup => "killing its process group",
            LinkOp::Terminate => "sending it SIGTERM (the run parent forwards the SIGTERM it was sent, D103)",
            LinkOp::NotOwnGroup => "killing its group, when it was started without a group of its own",
            LinkOp::NoParentDeathSignal => {
                "the kernel's kill at the parent's death is asked, and this system has none (Linux's PR_SET_PDEATHSIG)"
            }
        }
    }
}

/// A step on a linked child that failed: the program, the step, and the system's error number
/// (0 where the refusal is blaze's own).
#[derive(Clone, PartialEq, Eq, Debug)] // clone: an error
pub struct LinkError {
    pub program: PathBuf,
    pub op: LinkOp,
    pub os: i32,
}

impl fmt::Display for LinkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: linked child: {}", self.program.display(), self.op.what())?;
        match self.os {
            0 => Ok(()),
            os => write!(f, ": {}", io::Error::from_raw_os_error(os)),
        }
    }
}

impl std::error::Error for LinkError {}

/// Which bug a test plants in [`LinkedChild::kill_group`]: [`LinkPlant::None`] but in the
/// test's control.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum LinkPlant {
    None,
    /// The signal goes to the leader's pid, not its group: what the leader started lives on.
    KillLeaderOnly,
}

/// A child [`start_linked`] started, not yet reaped until it has ended and been waited for or
/// killed. Dropped unreaped, its group (or, without one, itself) is killed and reaped; where
/// that reap fails it stays listed as abandoned ([`super::Waiter::Abandoned`]).
pub struct LinkedChild {
    pid: libc::pid_t,
    program: PathBuf,
    own_group: bool,
    /// The clock of [`LinkedChild::wait_until`]'s deadline: zero at its start.
    clock: Monotonic,
    /// How it ended, once reaped: then its pid may be another's, and nothing signals it.
    ended: Cell<Option<Ended>>,
    stdout: Option<OwnedFd>,
    stderr: Option<OwnedFd>,
    /// Kept so the pid it names stays this process's for the driver's own waits.
    #[cfg(target_os = "linux")]
    _pidfd: OwnedFd,
    /// A kqueue holding `EVFILT_PROC`/`NOTE_EXIT` of `pid`: readable once it exits.
    #[cfg(target_os = "macos")]
    watch: OwnedFd,
    listed: super::Listed,
}

/// `command` started linked as `link` says: its descriptors given, its group its own where
/// asked, listed until reaped; no input, its output and error piped to the caller
/// ([`LinkedChild::take_outputs`]). [`run::start_run`] with [`run::Streams::PIPED`]: its
/// refusals are that function's.
pub fn start_linked(command: &Command, link: &Link<'_>) -> Result<LinkedChild, LinkError> {
    let run = run::RunLink {
        inherit: link.inherit,
        own_group: link.own_group,
        streams: run::Streams::PIPED,
        arg0: None,
        parent_death: run::ParentDeath::DropOnly,
    };
    run::start_run(command, &run)
}

impl LinkedChild {
    /// Its process id; also its group's id where it leads one.
    pub fn pid(&self) -> u32 {
        self.pid as u32
    }

    /// The clock [`LinkedChild::wait_until`] reads: zero at its start.
    pub fn clock(&self) -> &Monotonic {
        &self.clock
    }

    /// Its standard output and error, once: the caller reads them (unread, a full pipe stops
    /// the child at its next write).
    pub fn take_outputs(&mut self) -> Option<(OwnedFd, OwnedFd)> {
        match (self.stdout.take(), self.stderr.take()) {
            (Some(out), Some(err)) => Some((out, err)),
            _ => None,
        }
    }

    /// Waits until it ends or `deadline` passes on [`LinkedChild::clock`]: how it ended (then it
    /// is reaped), or [`Ended::Deadline`] with it still running and unreaped.
    pub fn wait_until(&self, deadline: Deadline) -> Result<Ended, LinkError> {
        self.wait_until_on(deadline, &self.clock)
    }

    /// As [`LinkedChild::wait_until`], `deadline` on `clock` (a driver's own, so a task's
    /// deadline and this wait agree).
    pub fn wait_until_on(&self, deadline: Deadline, clock: &impl Clock) -> Result<Ended, LinkError> {
        if let Some(ended) = self.ended.get() {
            return Ok(ended);
        }
        match self.exited_by(deadline, clock)? {
            true => self.reap_now(),
            false => Ok(Ended::Deadline),
        }
    }

    /// `SIGKILL` to its whole group, then its reap: nothing it started is left. Refused for a
    /// child started without a group of its own ([`LinkOp::NotOwnGroup`]).
    pub fn kill_group(&self) -> Result<(), LinkError> {
        self.kill_with(LinkPlant::None)
    }

    pub(crate) fn kill_with(&self, plant: LinkPlant) -> Result<(), LinkError> {
        if !self.own_group {
            return Err(self.error(LinkOp::NotOwnGroup, 0));
        }
        let target = match plant {
            LinkPlant::None => -self.pid,
            LinkPlant::KillLeaderOnly => self.pid,
        };
        // SAFETY: no pointers. The group's id is the leader's pid, which is unreaped (so no
        // one else's) until `reap_now`; after its reap the id stays the group's while any
        // member lives (POSIX: a group's id is not reused within the group's lifetime), and
        // with no member left the kill finds none (ESRCH).
        let sent = unsafe { libc::kill(target, libc::SIGKILL) };
        if sent != 0 {
            let os = io::Error::last_os_error().raw_os_error().unwrap_or(0);
            // No member left: what was asked holds.
            if os != libc::ESRCH {
                return Err(self.error(LinkOp::KillGroup, os));
            }
        }
        if self.ended.get().is_none() {
            self.reap_now()?;
        }
        Ok(())
    }

    fn error(&self, op: LinkOp, os: i32) -> LinkError {
        // clone: an error path, the program's path.
        LinkError { program: self.program.clone(), op, os }
    }

    /// Whether it exited by `deadline` on `clock`, leaving it unreaped.
    #[cfg(target_os = "linux")]
    fn exited_by(&self, deadline: Deadline, clock: &impl Clock) -> Result<bool, LinkError> {
        // wakes: the kernel, at the child's exit (`waitid`), or the deadline.
        match crate::wait::exited("a linked child's exit", self.pid, deadline.finite(), clock) {
            Ok(crate::wait::Waited::Ready(())) => Ok(true),
            Ok(crate::wait::Waited::Deadline) => Ok(false),
            Err(error) => Err(self.error(LinkOp::Wait, wait_os(&error))),
        }
    }

    /// Whether it exited by `deadline` on `clock`, leaving it unreaped: one look at whether it
    /// exited already (the watch may have been made on a zombie), then `kevent` on the watch.
    #[cfg(target_os = "macos")]
    fn exited_by(&self, deadline: Deadline, clock: &impl Clock) -> Result<bool, LinkError> {
        match crate::raw::has_exited(self.pid) {
            Ok(true) => return Ok(true),
            Ok(false) => {}
            Err(error) => return Err(self.error(LinkOp::Wait, os_of(&error))),
        }
        let kq = self.watch.as_raw_fd();
        // wakes: the kernel, at the child's exit (`NOTE_EXIT` on the watch), or the deadline.
        let waited = crate::wait::until("a linked child's exit", clock, deadline.finite(), |left| {
            let timeout = left.map(timespec);
            let at = timeout.as_ref().map_or(std::ptr::null(), |it| it as *const libc::timespec);
            // SAFETY: all zeros is a valid `kevent`; the call writes it.
            let mut event: libc::kevent = unsafe { std::mem::zeroed() };
            // SAFETY: the watch's kqueue; no changes; one event to write into a live local;
            // `at` null or a live `timespec`.
            let got = unsafe { libc::kevent(kq, std::ptr::null(), 0, &mut event, 1, at) };
            match got {
                n if n > 0 => Ok(crate::wait::Once::Ready(())),
                0 => Ok(crate::wait::Once::TimedOut),
                _ => {
                    let error = io::Error::last_os_error();
                    match error.raw_os_error() {
                        Some(libc::EINTR) => Ok(crate::wait::Once::Interrupted),
                        _ => Err(error),
                    }
                }
            }
        });
        match waited {
            Ok(crate::wait::Waited::Ready(())) => Ok(true),
            Ok(crate::wait::Waited::Deadline) => Ok(false),
            Err(error) => Err(self.error(LinkOp::Wait, wait_os(&error))),
        }
    }

    /// No wait for a child's exit by a deadline on this system.
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    fn exited_by(&self, _deadline: Deadline, _clock: &impl Clock) -> Result<bool, LinkError> {
        Err(self.error(LinkOp::Wait, libc::ENOSYS))
    }

    /// Reaps it, which has exited or was killed: how it ended, kept.
    fn reap_now(&self) -> Result<Ended, LinkError> {
        let (status, _used) = super::reap(self.pid).map_err(|error| self.error(LinkOp::Reap, os_of(&error)))?;
        let ended = if libc::WIFEXITED(status) {
            Ended::Exited(libc::WEXITSTATUS(status))
        } else if libc::WIFSIGNALED(status) {
            Ended::Signaled(libc::WTERMSIG(status))
        } else {
            // A reap without WUNTRACED reports only an exit or a signal's end.
            return Err(self.error(LinkOp::Reap, 0));
        };
        self.ended.set(Some(ended));
        Ok(ended)
    }
}

impl Drop for LinkedChild {
    /// Never left running: killed (its group where it has one) and reaped. Drop cannot return
    /// an error: a reap that fails leaves it listed, abandoned, for a stall report to name.
    fn drop(&mut self) {
        if self.ended.get().is_some() {
            return;
        }
        let target = if self.own_group { -self.pid } else { self.pid };
        // SAFETY: no pointers; the pid is unreaped (`ended` is none), so it and its group are
        // this process's child's.
        unsafe { libc::kill(target, libc::SIGKILL) };
        if super::reap(self.pid).is_ok() {
            self.ended.set(Some(Ended::Signaled(libc::SIGKILL)));
        }
        // `listed` drops after this: unreaped, it marks the child abandoned.
        let _ = &self.listed;
    }
}

/// `fd` copied to the lowest free descriptor at `lowest` or above, closed on exec.
fn copy_past(fd: BorrowedFd<'_>, lowest: RawFd) -> io::Result<OwnedFd> {
    // SAFETY: an open descriptor (borrowed for this call); the copy is a descriptor of its own.
    let copied = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_DUPFD_CLOEXEC, lowest) };
    if copied < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: as above, owned once from here.
    Ok(unsafe { OwnedFd::from_raw_fd(copied) })
}

/// A kqueue holding `EVFILT_PROC`/`NOTE_EXIT` of `pid`. A pid that has exited already (a
/// zombie) may be refused with `ESRCH`: then the watch is empty, and [`LinkedChild::exited_by`]
/// sees the exit by its first look (TO VERIFY BY A TEST: which of the two macOS does for a
/// zombie; both are handled).
#[cfg(target_os = "macos")]
fn watch_exit(pid: libc::pid_t) -> io::Result<OwnedFd> {
    // SAFETY: no arguments; returns a descriptor of its own or -1.
    let kq = unsafe { libc::kqueue() };
    if kq < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the call made it; owned once from here.
    let kq = unsafe { OwnedFd::from_raw_fd(kq) };
    crate::net::set_cloexec(kq.as_raw_fd())?;
    // SAFETY: all zeros is a valid `kevent`; its fields are set below.
    let mut change: libc::kevent = unsafe { std::mem::zeroed() };
    change.ident = pid as libc::uintptr_t;
    change.filter = libc::EVFILT_PROC;
    change.flags = libc::EV_ADD | libc::EV_ONESHOT;
    change.fflags = libc::NOTE_EXIT;
    // SAFETY: the kqueue made above; one change read from a live local; no events asked.
    let added = unsafe { libc::kevent(kq.as_raw_fd(), &change, 1, std::ptr::null_mut(), 0, std::ptr::null()) };
    if added < 0 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::ESRCH) {
            return Err(error);
        }
    }
    Ok(kq)
}

#[cfg(target_os = "macos")]
fn timespec(left: Duration) -> libc::timespec {
    libc::timespec {
        tv_sec: libc::time_t::try_from(left.as_secs()).unwrap_or(libc::time_t::MAX),
        tv_nsec: libc::c_long::from(left.subsec_nanos() as i32),
    }
}

fn os_of(error: &io::Error) -> i32 {
    error.raw_os_error().unwrap_or(0)
}

fn wait_os(error: &crate::wait::WaitError) -> i32 {
    error.io_error().and_then(io::Error::raw_os_error).unwrap_or(0)
}
