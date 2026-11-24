//! The spawn of a linked child ([`crate::process::linked::run::start_run`]): its standard
//! streams as its caller chose them ([`Streams`]), its own `argv[0]` where given, and on Linux
//! the kernel's kill at its parent's death. The same child code as [`super::spawn_with`]
//! (Linux: `clone` with CLONE_VM | CLONE_VFORK and [`super::run`]; macOS: `posix_spawn`): only
//! the three descriptors it is handed differ.
//!
//! An inherited stream is a copy of this process's descriptor 0, 1 or 2 made past 2 and closed
//! on exec here, which the child's `dup2` puts back on its own 0, 1 or 2 without close-on-exec
//! (POSIX dup2: "the FD_CLOEXEC flag associated with the new file descriptor shall be
//! cleared"). A copy rather than the descriptor itself, so that no `dup2(fd, fd)` is asked
//! (which POSIX leaves "without changing" the flag) and so a parent whose own descriptor is
//! closed fails here, naming it, never starting a child with a stream missing.

use std::{
    ffi::OsStr,
    io,
    os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd, RawFd},
};

use super::{Plan, Program, StartError, open, pipe};
#[cfg(target_os = "linux")]
use crate::busy::while_busy;
use crate::process::linked::run::{Input, Output, Streams};

/// What a linked spawn is asked besides its program.
pub(in crate::process) struct Start<'a> {
    pub(in crate::process) arg0: Option<&'a OsStr>,
    pub(in crate::process) streams: Streams,
    /// Each `(source, target)`: as [`super::spawn_with`]'s `extra` says.
    pub(in crate::process) extra: &'a [(RawFd, RawFd)],
    pub(in crate::process) group: bool,
    /// Starts the child in a new session (`setsid` on Linux, `POSIX_SPAWN_SETSID` on macOS).
    pub(in crate::process) session: bool,
    /// Linux only: `PR_SET_PDEATHSIG(SIGKILL)` in the child; refused elsewhere.
    pub(in crate::process) parent_death: bool,
    /// The confinement the child enters last before `execve` (D177; `None` elsewhere than
    /// Linux, where none can be made).
    pub(in crate::process) confine: Option<&'a super::super::linked::run::ChildConfine<'a>>,
}

pub(in crate::process) struct Started {
    pub(in crate::process) pid: libc::pid_t,
    #[cfg(target_os = "linux")]
    pub(in crate::process) pidfd: OwnedFd,
    /// The read ends of the streams asked as [`Output::Pipe`].
    pub(in crate::process) stdout: Option<OwnedFd>,
    pub(in crate::process) stderr: Option<OwnedFd>,
}

/// `program` started as `start` says. A refused confinement is its own error, naming the step.
pub(in crate::process) fn spawn_streams(program: &Program<'_>, start: &Start<'_>) -> Result<Started, StartError> {
    let plan = Plan::named(program, start.arg0)?;
    let stdin_child = match start.streams.stdin {
        Input::Null => open(c"/dev/null", libc::O_RDONLY)?,
        Input::Inherit => inherited(0)?,
    };
    let (stdout, stdout_child) = output(start.streams.stdout, 1)?;
    let (stderr, stderr_child) = output(start.streams.stderr, 2)?;
    let fds = [stdin_child.as_raw_fd(), stdout_child.as_raw_fd(), stderr_child.as_raw_fd()];
    // The child's ends close when this returns: by then the child has its own copies.
    #[cfg(target_os = "linux")]
    {
        // The pid the child checks `getppid` against after arming; 0: not armed.
        // SAFETY: no arguments; always succeeds (getpid(2)).
        let parent = if start.parent_death { unsafe { libc::getpid() } } else { 0 };
        let child = super::ChildAsk { procs: -1, extra: start.extra, group: start.group, session: start.session, parent, confine: start.confine };
        match while_busy(|| super::clone(&plan, fds, &child))? {
            super::Cloned::Started { pid, pidfd } => Ok(Started { pid, pidfd, stdout, stderr }),
            super::Cloned::ConfineRefused { step, errno } => Err(StartError::Confine { step, errno }),
        }
    }
    #[cfg(target_os = "macos")]
    {
        if start.parent_death {
            // `start_run` refuses it by name first; this is its second guard, never reached.
            return Err(io::Error::new(io::ErrorKind::Unsupported, "macOS has no kill at a parent's death").into());
        }
        if start.confine.is_some() {
            // No value of the type exists here (`linked::run::ChildConfine`); a second guard.
            return Err(io::Error::new(io::ErrorKind::Unsupported, "macOS: no confinement is entered by posix_spawn").into());
        }
        let pid = super::posix_spawn(&plan, fds, start.extra, start.group, start.session)?;
        Ok(Started { pid, stdout, stderr })
    }
}

/// One output stream: the caller's read end (a pipe only) and the child's end.
fn output(chosen: Output, own: RawFd) -> io::Result<(Option<OwnedFd>, OwnedFd)> {
    match chosen {
        Output::Pipe => {
            let (read, write) = pipe()?;
            Ok((Some(read), write))
        }
        Output::Null => Ok((None, open(c"/dev/null", libc::O_WRONLY)?)),
        Output::Inherit => Ok((None, inherited(own)?)),
    }
}

/// A copy of this process's descriptor `own` (0, 1 or 2) past 2, closed on exec.
fn inherited(own: RawFd) -> io::Result<OwnedFd> {
    // SAFETY: no pointers; on a closed `own` the call fails with EBADF, which is returned.
    let copied = unsafe { libc::fcntl(own, libc::F_DUPFD_CLOEXEC, 3) };
    if copied < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the call made it; owned once from here.
    Ok(unsafe { OwnedFd::from_raw_fd(copied) })
}

#[cfg(test)]
mod tests {
    use std::{ffi::OsString, path::Path};

    use super::*;

    /// Prints "spawn plan: argv[0] is the name asked, the path otherwise". execve(2): "argv is
    /// an array of argument strings passed to the new program. By convention, the first of
    /// these strings (i.e., argv[0]) should contain the filename associated with the file being
    /// executed": a convention the caller may set otherwise (the run parent: the binary
    /// target's name), never the program's path forced in its place.
    #[test]
    fn argv0_is_the_name_asked() {
        let args = [OsString::from("one")];
        let env: [(OsString, OsString); 0] = [];
        let program = Program { program: Path::new("/bin/sh"), args: &args, env: &env, dir: Path::new("/") };
        let named = Plan::named(&program, Some(OsStr::new("tempo"))).expect("a plan");
        // SAFETY: argv[0] and argv[1] are NUL-terminated strings the plan keeps.
        let first = unsafe { std::ffi::CStr::from_ptr(named.argv[0]) };
        // SAFETY: as above.
        let second = unsafe { std::ffi::CStr::from_ptr(named.argv[1]) };
        assert_eq!(first.to_bytes(), b"tempo");
        assert_eq!(second.to_bytes(), b"one");
        let plain = Plan::named(&program, None).expect("a plan");
        // SAFETY: as above.
        let first = unsafe { std::ffi::CStr::from_ptr(plain.argv[0]) };
        assert_eq!(first.to_bytes(), b"/bin/sh");
        assert!(Plan::named(&program, Some(OsStr::new("a\0b"))).is_err(), "a NUL in argv[0] is refused");
        println!("spawn plan: argv[0] is the name asked, the path otherwise");
    }
}
