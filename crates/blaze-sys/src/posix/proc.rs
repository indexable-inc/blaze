//! Processes and files (D180, lane L5; no C library on Linux, lane N5): a child's wait and its status,
//! fork and exec and `posix_spawn`, a file's status (`struct stat`, `struct statfs`, the `S_IF*`
//! bits, device numbers), the calls on names and attributes the tree makes through the C
//! library, priorities, and the Mach and ELF constants the tree reads. Each item says what it
//! replaces (`/// replaces libc::<name>`).
//!
//! Where blaze-sys already has the call typed, that is the replacement and nothing is
//! declared twice: on Linux `crate::linux` (`statx`, `fstatfs`, `waitid`, `wait4_now`,
//! `clone3_process`, `execveat`, `unlinkat`, `mkdirat`, `renameat2`, `fchmod`, `utimensat`), on
//! macOS the table of calls found at first use, [`crate::macos::cold`], which this module
//! re-exports by name where the tree calls one (`pub use crate::macos::cold::wait4`), so a port
//! names `blaze_sys::posix::proc::wait4` on both systems where both have it.
//!
//! The raw declarations keep the C names and signatures, so a port of a `libc::` call is a
//! change of path; the typed wrappers beside them (`wait_pid`, `stat_fd`, `stat_at`, ...) take
//! an [`crate::Fd`] and a [`crate::Path`] and return `Result<_, Errno>`, with `errno` read once
//! (macOS) through `crate::posix::signal::errno`.
//!
//! On Linux this module names NO C-library symbol: every call is the kernel's, by raw system
//! call, in `crate::linux::task` (and the linux/ calls it reuses), re-exported or called
//! here under the C names where the C name has a kernel form. Those with none on Linux
//! (`execv`'s `environ`, the variadic `clone`) are not offered there; macOS is unchanged.
//!
//! # The error numbers
//!
//! Each `libc::E*` constant is a variant of [`crate::Errno`] (a `libc::c_int` compared with one
//! is `Errno::from_raw(code as u16)`):
//!
//! - replaces libc::EPERM: [`Errno::Perm`]; replaces libc::ENOENT: [`Errno::NoEnt`];
//!   replaces libc::ESRCH: [`Errno::Srch`]; replaces libc::EINTR: [`Errno::Intr`];
//!   replaces libc::EIO: [`Errno::Io`]; replaces libc::ENXIO: [`Errno::Nxio`];
//!   replaces libc::EBADF: [`Errno::BadF`]; replaces libc::ECHILD: [`Errno::Child`];
//!   replaces libc::EAGAIN: [`Errno::Again`]; replaces libc::EWOULDBLOCK: [`Errno::Again`]
//!   (the same number on Linux and macOS); replaces libc::ENOMEM: [`Errno::NoMem`];
//!   replaces libc::EACCES: [`Errno::Acces`]; replaces libc::EFAULT: [`Errno::Fault`];
//!   replaces libc::EBUSY: [`Errno::Busy`]; replaces libc::EEXIST: [`Errno::Exist`];
//!   replaces libc::EXDEV: [`Errno::XDev`]; replaces libc::ENODEV: [`Errno::NoDev`];
//!   replaces libc::ENOTDIR: [`Errno::NotDir`]; replaces libc::EISDIR: [`Errno::IsDir`];
//!   replaces libc::EINVAL: [`Errno::Inval`]; replaces libc::ENFILE: [`Errno::NFile`];
//!   replaces libc::EMFILE: [`Errno::MFile`]; replaces libc::EFBIG: [`Errno::FBig`];
//!   replaces libc::ENOSPC: [`Errno::NoSpc`]; replaces libc::EPIPE: [`Errno::Pipe`];
//!   replaces libc::ERANGE: [`Errno::Range`]; replaces libc::ENAMETOOLONG: [`Errno::NameTooLong`];
//!   replaces libc::ENOSYS: [`Errno::NoSys`]; replaces libc::ELOOP: [`Errno::Loop`];
//!   replaces libc::EOPNOTSUPP: [`Errno::OpNotSupp`]; replaces libc::ENETDOWN: [`Errno::NetDown`];
//!   replaces libc::ENETUNREACH: [`Errno::NetUnreach`];
//!   replaces libc::ECONNABORTED: [`Errno::ConnAborted`];
//!   replaces libc::ECONNRESET: [`Errno::ConnReset`]; replaces libc::ENOBUFS: [`Errno::NoBufs`];
//!   replaces libc::ENOTCONN: [`Errno::NotConn`]; replaces libc::ETIMEDOUT: [`Errno::TimedOut`];
//!   replaces libc::EHOSTUNREACH: [`Errno::HostUnreach`]; replaces libc::EALREADY:
//!   [`Errno::Already`]; replaces libc::EINPROGRESS: [`Errno::InProgress`];
//!   replaces libc::ESTALE: [`Errno::Stale`]; replaces libc::ECANCELED: [`Errno::Canceled`].
//! - Added for D180: replaces libc::ENODATA: [`Errno::NoData`]; replaces libc::EPROTO:
//!   [`Errno::Proto`]; replaces libc::ENOPROTOOPT: [`Errno::NoProtoOpt`];
//!   replaces libc::EHOSTDOWN: [`Errno::HostDown`]; replaces libc::ENONET: [`Errno::NoNet`]
//!   (Linux only: macOS has none); replaces libc::ENOTSUP: [`Errno::NotSup`] on macOS, and
//!   [`Errno::OpNotSupp`] on Linux, where the two are one number.
//! - replaces libc::ETIME: `crate::linux::ETIME` (Linux: io_uring's wait whose time ran out;
//!   every use is Linux's).
//! - Lane D2 (the variants were in `crate::Errno` already; each line here names one):
//!   replaces libc::E2BIG (blaze_sys::Errno::TooBig)
//!   replaces libc::ENOEXEC (blaze_sys::Errno::NoExec)
//!   replaces libc::ENOTTY (blaze_sys::Errno::NoTty)
//!   replaces libc::ETXTBSY (blaze_sys::Errno::TxtBsy)
//!   replaces libc::ESPIPE (blaze_sys::Errno::SPipe)
//!   replaces libc::EROFS (blaze_sys::Errno::RoFs)
//!   replaces libc::EMLINK (blaze_sys::Errno::MLink)
//!   replaces libc::EDEADLK (blaze_sys::Errno::Deadlk)
//!   replaces libc::ENOLCK (blaze_sys::Errno::NoLck)
//!   replaces libc::ENOTEMPTY (blaze_sys::Errno::NotEmpty)
//!   replaces libc::EOVERFLOW (blaze_sys::Errno::Overflow)
//!   replaces libc::ENOTSOCK (blaze_sys::Errno::NotSock)
//!   replaces libc::EMSGSIZE (blaze_sys::Errno::MsgSize)
//!   replaces libc::EPROTONOSUPPORT (blaze_sys::Errno::ProtoNoSupport)
//!   replaces libc::EAFNOSUPPORT (blaze_sys::Errno::AfNoSupport)
//!   replaces libc::EADDRINUSE (blaze_sys::Errno::AddrInUse)
//!   replaces libc::EADDRNOTAVAIL (blaze_sys::Errno::AddrNotAvail)
//!   replaces libc::EISCONN (blaze_sys::Errno::IsConn)
//!   replaces libc::ECONNREFUSED (blaze_sys::Errno::ConnRefused)
//!   replaces libc::EDQUOT (blaze_sys::Errno::DQuot)
//!   A `libc::c_int` compared with one is `Errno::from_raw(code as u16)`.
//!
//! [`Errno::Perm`]: crate::Errno::Perm
//! [`Errno::NoEnt`]: crate::Errno::NoEnt
//! [`Errno::Srch`]: crate::Errno::Srch
//! [`Errno::Intr`]: crate::Errno::Intr
//! [`Errno::Io`]: crate::Errno::Io
//! [`Errno::Nxio`]: crate::Errno::Nxio
//! [`Errno::BadF`]: crate::Errno::BadF
//! [`Errno::Child`]: crate::Errno::Child
//! [`Errno::Again`]: crate::Errno::Again
//! [`Errno::NoMem`]: crate::Errno::NoMem
//! [`Errno::Acces`]: crate::Errno::Acces
//! [`Errno::Fault`]: crate::Errno::Fault
//! [`Errno::Busy`]: crate::Errno::Busy
//! [`Errno::Exist`]: crate::Errno::Exist
//! [`Errno::XDev`]: crate::Errno::XDev
//! [`Errno::NoDev`]: crate::Errno::NoDev
//! [`Errno::NotDir`]: crate::Errno::NotDir
//! [`Errno::IsDir`]: crate::Errno::IsDir
//! [`Errno::Inval`]: crate::Errno::Inval
//! [`Errno::NFile`]: crate::Errno::NFile
//! [`Errno::MFile`]: crate::Errno::MFile
//! [`Errno::FBig`]: crate::Errno::FBig
//! [`Errno::NoSpc`]: crate::Errno::NoSpc
//! [`Errno::Pipe`]: crate::Errno::Pipe
//! [`Errno::Range`]: crate::Errno::Range
//! [`Errno::NameTooLong`]: crate::Errno::NameTooLong
//! [`Errno::NoSys`]: crate::Errno::NoSys
//! [`Errno::Loop`]: crate::Errno::Loop
//! [`Errno::OpNotSupp`]: crate::Errno::OpNotSupp
//! [`Errno::NetDown`]: crate::Errno::NetDown
//! [`Errno::NetUnreach`]: crate::Errno::NetUnreach
//! [`Errno::ConnAborted`]: crate::Errno::ConnAborted
//! [`Errno::ConnReset`]: crate::Errno::ConnReset
//! [`Errno::NoBufs`]: crate::Errno::NoBufs
//! [`Errno::NotConn`]: crate::Errno::NotConn
//! [`Errno::TimedOut`]: crate::Errno::TimedOut
//! [`Errno::HostUnreach`]: crate::Errno::HostUnreach
//! [`Errno::Already`]: crate::Errno::Already
//! [`Errno::InProgress`]: crate::Errno::InProgress
//! [`Errno::Stale`]: crate::Errno::Stale
//! [`Errno::Canceled`]: crate::Errno::Canceled
//! [`Errno::NoData`]: crate::Errno::NoData
//! [`Errno::Proto`]: crate::Errno::Proto
//! [`Errno::NoProtoOpt`]: crate::Errno::NoProtoOpt
//! [`Errno::HostDown`]: crate::Errno::HostDown
//! [`Errno::NoNet`]: crate::Errno::NoNet
//! [`Errno::NotSup`]: crate::Errno::NotSup

#[cfg(target_os = "macos")]
use core::ffi::c_int;

#[cfg(target_os = "macos")]
use crate::Errno;

mod exec;
mod sched;
// Not `stat`: the module would shadow the `stat` struct its glob re-exports.
mod status;
mod wait;

#[cfg(target_os = "linux")]
mod elf;
#[cfg(target_os = "macos")]
mod mach;

#[cfg(target_os = "linux")]
pub use elf::*;
pub use exec::*;
#[cfg(target_os = "macos")]
pub use mach::*;
pub use sched::*;
pub use status::*;
pub use wait::*;

/// A C call's `int` result: [`Errno`] (read from `errno`) where it is -1, the value otherwise.
/// macOS: on Linux nothing here calls the C library, so there is no `errno` to read.
#[cfg(target_os = "macos")]
#[blaze::noalloc]
#[blaze::nopanic]
fn said(result: c_int) -> Result<c_int, Errno> {
    if result == -1 { Err(crate::posix::signal::errno()) } else { Ok(result) }
}

/// A call that returns its error rather than setting `errno` (`posix_fadvise`,
/// `posix_fallocate`, the `posix_spawn` family): 0 is success, anything else the error. macOS.
#[cfg(target_os = "macos")]
#[blaze::noalloc]
#[blaze::nopanic]
fn returned(result: c_int) -> Result<(), Errno> {
    match result {
        0 => Ok(()),
        code => Err(Errno::from_raw(code as u16)),
    }
}

#[cfg(test)]
mod tests;
