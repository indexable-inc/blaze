//! A child's wait: the `W*` options and status macros, `waitpid`, `waitid` and `wait4`.
//!
//! The status macros are the same bits on Linux and macOS (sys/wait.h on both: the low seven
//! bits are the signal, 0 for an exit and 0x7f for a stop; the next eight the exit code).

use core::ffi::{c_int, c_uint};

#[cfg(target_os = "macos")]
use super::said;
use crate::{Errno, posix::types::pid_t};

/// `WNOHANG` (sys/wait.h): look, and wait for nothing.
/// replaces libc::WNOHANG
pub const WNOHANG: c_int = 0x1;

/// `WEXITED` (sys/wait.h): `waitid` reports children that have exited.
/// replaces libc::WEXITED
pub const WEXITED: c_int = 0x4;

/// `WNOWAIT` (sys/wait.h): leave the child to be waited for again.
/// replaces libc::WNOWAIT
#[cfg(target_os = "linux")]
pub const WNOWAIT: c_int = 0x0100_0000;

/// `WNOWAIT` (sys/wait.h): leave the child to be waited for again.
/// replaces libc::WNOWAIT
#[cfg(target_os = "macos")]
pub const WNOWAIT: c_int = 0x20;

/// `idtype_t` (sys/wait.h): which kind of id a `waitid` names.
/// replaces libc::idtype_t
#[allow(non_camel_case_types, reason = "the C type's name, so a port is a change of path")]
pub type idtype_t = c_uint;

/// `P_PID` (sys/wait.h): the id is one child's pid. Also io_uring's `IORING_OP_WAITID` `len`.
/// replaces libc::P_PID
pub const P_PID: idtype_t = 1;

/// `WIFEXITED(status)`: the child exited (its low seven bits are 0).
/// replaces libc::WIFEXITED
#[allow(non_snake_case, reason = "the C macro's name, so a port is a change of path")]
#[blaze::noalloc]
#[blaze::nopanic]
pub const fn WIFEXITED(status: c_int) -> bool {
    status & 0x7f == 0
}

/// `WEXITSTATUS(status)`: the exit code of a child that exited.
/// replaces libc::WEXITSTATUS
#[allow(non_snake_case, reason = "the C macro's name, so a port is a change of path")]
#[blaze::noalloc]
#[blaze::nopanic]
pub const fn WEXITSTATUS(status: c_int) -> c_int {
    (status >> 8) & 0xff
}

/// `WIFSIGNALED(status)`: a signal ended the child (its low seven bits are neither 0, an exit,
/// nor 0x7f, a stop). Linux's form (`((s & 0x7f) + 1) >> 1 > 0` as a signed byte) and macOS's
/// (`(s & 0x7f) != 0x7f && (s & 0x7f) != 0`) say the same.
/// replaces libc::WIFSIGNALED
#[allow(non_snake_case, reason = "the C macro's name, so a port is a change of path")]
#[blaze::noalloc]
#[blaze::nopanic]
pub const fn WIFSIGNALED(status: c_int) -> bool {
    let low = status & 0x7f;
    low != 0 && low != 0x7f
}

/// `WTERMSIG(status)`: the signal that ended a child [`WIFSIGNALED`] says a signal ended.
/// replaces libc::WTERMSIG
#[allow(non_snake_case, reason = "the C macro's name, so a port is a change of path")]
#[blaze::noalloc]
#[blaze::nopanic]
pub const fn WTERMSIG(status: c_int) -> c_int {
    status & 0x7f
}

flag_set! {
    /// A wait's options: the bits of `waitpid`, `waitid` and `wait4` on this system.
    WaitFlags: c_int {
        /// `WNOHANG`.
        NO_HANG = WNOHANG;
        /// `WEXITED` (`waitid` only).
        EXITED = WEXITED;
        /// `WNOWAIT` (`waitid` only).
        NO_WAIT = WNOWAIT;
    }
}

/// A wait status as `waitpid` and `wait4` write it, read by the macros above.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[repr(transparent)]
pub struct WaitStatus(pub c_int);

impl WaitStatus {
    /// The exit code, where the child exited.
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub const fn exit_code(self) -> Option<c_int> {
        if WIFEXITED(self.0) { Some(WEXITSTATUS(self.0)) } else { None }
    }

    /// The signal, where one ended the child.
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub const fn signal(self) -> Option<c_int> {
        if WIFSIGNALED(self.0) { Some(WTERMSIG(self.0)) } else { None }
    }
}

#[cfg(target_os = "macos")]
unsafe extern "C" {
    /// `pid_t waitpid(pid_t pid, int *status, int options)` (sys/wait.h): a child's end
    /// (`pid` > 0 one child, -1 any), its status written to `status` (may be null); 0 with
    /// `WNOHANG` where none has ended; -1 and `errno`.
    /// replaces libc::waitpid
    pub fn waitpid(pid: pid_t, status: *mut c_int, options: c_int) -> pid_t;
}

/// `waitpid` on Linux: the kernel's `wait4` with no usage, typed
/// (`waitpid(pid, WaitOptions) -> Result<Option<(pid, status)>, Errno>`); [`wait_pid`] is the
/// call on both systems.
/// replaces libc::waitpid
#[cfg(target_os = "linux")]
pub use crate::linux::task::waitpid;

/// `waitid` (sys/wait.h), from the table of calls found at first use:
/// `int waitid(idtype_t, id_t, siginfo_t *, int)`, `info` a `siginfo_t`. On Linux the typed
/// call is `crate::linux::waitid` (and [`wait_id`] on both).
/// replaces libc::waitid
#[cfg(target_os = "macos")]
pub use crate::macos::cold::waitid;

/// `wait4` (sys/wait.h), from the table of calls found at first use:
/// `pid_t wait4(pid_t, int *, int, struct rusage *)`, `usage` a [`crate::macos::Rusage`]. On
/// Linux the typed call is `crate::linux::wait4_now` (`WNOHANG`, the one form Linux's callers
/// use).
/// replaces libc::wait4
#[cfg(target_os = "macos")]
pub use crate::macos::cold::wait4;

/// How a child ended, and the child a [`wait_id`] found: Linux's own types.
#[cfg(target_os = "linux")]
pub use crate::linux::{Ended, Waited};

/// How a child ended (`si_code` of a `SIGCHLD`): the same type as Linux's.
#[cfg(target_os = "macos")]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Ended {
    /// `CLD_EXITED`, with its exit status.
    Exited(i32),
    /// `CLD_KILLED` or `CLD_DUMPED`, with the signal.
    Signaled(i32),
    /// Another `si_code`, with it and `si_status`.
    Other(i32, i32),
}

/// A child a [`wait_id`] found: the same type as Linux's.
#[cfg(target_os = "macos")]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Waited {
    /// The child's pid.
    pub pid: i32,
    /// How it ended.
    pub ended: Ended,
}

/// macOS's `siginfo_t` (sys/signal.h, `struct __siginfo`): 104 bytes. For a `SIGCHLD`:
/// `si_signo` at 0, `si_errno` at 4, `si_code` at 8, `si_pid` at 12, `si_uid` at 16,
/// `si_status` at 20. Private: read only here; the full type is lane L1's `siginfo_t`.
#[cfg(target_os = "macos")]
#[repr(C, align(8))]
struct ChildInfo([i32; 26]);

// sys/signal.h, macOS arm64: `struct __siginfo` is 104 bytes, aligned to its pointer.
#[cfg(target_os = "macos")]
const _: () = {
    assert!(size_of::<ChildInfo>() == 104);
    assert!(align_of::<ChildInfo>() == 8);
};

/// One `waitpid(pid, options)`: the child and its status, or `None` where
/// [`WaitFlags::NO_HANG`] found none ended. [`Errno::Child`] where there is no such child;
/// [`Errno::Intr`] is the caller's to retry (its loop is the bounded one).
#[cfg(target_os = "linux")]
#[blaze::noalloc]
#[blaze::nopanic]
pub fn wait_pid(pid: pid_t, options: WaitFlags) -> Result<Option<(pid_t, WaitStatus)>, Errno> {
    let reaped = crate::linux::task::waitpid(pid, crate::linux::WaitOptions::from_bits(options.bits() as u32))?;
    Ok(reaped.map(|(reaped, status)| (reaped, WaitStatus(status))))
}

/// One `waitpid(pid, options)`: the child and its status, or `None` where
/// [`WaitFlags::NO_HANG`] found none ended. [`Errno::Child`] where there is no such child;
/// [`Errno::Intr`] is the caller's to retry (its loop is the bounded one).
#[cfg(target_os = "macos")]
#[blaze::noalloc]
#[blaze::nopanic]
pub fn wait_pid(pid: pid_t, options: WaitFlags) -> Result<Option<(pid_t, WaitStatus)>, Errno> {
    let mut status: c_int = 0;
    // SAFETY: `status` is a live `int` the call writes and does not keep; the options are
    // `waitpid`'s own bits.
    let reaped = said(unsafe { waitpid(pid, &raw mut status, options.bits()) })?;
    Ok(match reaped {
        0 => None,
        reaped => Some((reaped, WaitStatus(status))),
    })
}

/// One `waitid(P_PID, pid, options)`: the child it found, or `None` where
/// [`WaitFlags::NO_HANG`] found none ready; with [`WaitFlags::NO_WAIT`] it is left to be
/// waited for again, else reaped. [`Errno::Child`]; [`Errno::Intr`]. On Linux this is
/// `crate::linux::waitid` by system call.
#[cfg(target_os = "linux")]
#[blaze::noalloc]
#[blaze::nopanic]
pub fn wait_id(pid: pid_t, options: WaitFlags) -> Result<Option<Waited>, Errno> {
    crate::linux::waitid(
        crate::linux::Children::Pid(pid),
        crate::linux::WaitOptions::from_bits(options.bits() as u32),
    )
}

/// One `waitid(P_PID, pid, options)`: the child it found, or `None` where
/// [`WaitFlags::NO_HANG`] found none ready; with [`WaitFlags::NO_WAIT`] it is left to be
/// waited for again, else reaped. [`Errno::Child`]; [`Errno::Intr`].
#[cfg(target_os = "macos")]
#[blaze::noalloc]
#[blaze::nopanic]
pub fn wait_id(pid: pid_t, options: WaitFlags) -> Result<Option<Waited>, Errno> {
    // Zeroed, so that a child still running, of which the call writes nothing, reads as pid 0.
    let mut info = ChildInfo([0; 26]);
    // SAFETY: `info` is 104 live bytes of integers (any bytes valid) the call writes one
    // `siginfo_t` into and does not keep.
    said(unsafe { waitid(P_PID, pid as crate::posix::types::id_t, (&raw mut info).cast(), options.bits()) })?;
    let ChildInfo(words) = info;
    let [_signo, _errno, code, child, _uid, status, ..] = words;
    if child == 0 {
        return Ok(None);
    }
    let ended = match code {
        1 => Ended::Exited(status),
        2 | 3 => Ended::Signaled(status),
        other => Ended::Other(other, status),
    };
    Ok(Some(Waited { pid: child, ended }))
}

/// One `wait4(pid, options)`: the child, its status and what it used, or `None` where
/// [`WaitFlags::NO_HANG`] found it running. [`Errno::Child`]; [`Errno::Intr`]. On Linux the
/// call is `crate::linux::wait4_now`.
#[cfg(target_os = "macos")]
#[blaze::noalloc]
#[blaze::nopanic]
pub fn wait_usage(
    pid: pid_t,
    options: WaitFlags,
) -> Result<Option<(pid_t, WaitStatus, crate::macos::Rusage)>, Errno> {
    let mut status: c_int = 0;
    let mut usage = crate::macos::Rusage::default();
    // SAFETY: `status` (an `int`) and `usage` (a `struct rusage` of integers, the layout
    // `crate::macos::Rusage` asserts) are live locals the call writes and does not keep.
    let reaped = said(unsafe { wait4(pid, &raw mut status, options.bits(), (&raw mut usage).cast()) })?;
    Ok(match reaped {
        0 => None,
        reaped => Some((reaped, WaitStatus(status), usage)),
    })
}
