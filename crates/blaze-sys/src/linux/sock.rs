//! Sockets beyond [`super::net`]'s typed calls, by raw system call: a socket pair, a socket's
//! own address, an option of any level and name as bytes, and `sendmsg` / `recvmsg` /
//! `sendmmsg` / `recvmmsg` with the whole C header (an address, parts and control messages).
//! What `crate::posix::net` calls on Linux, so that layer names no C-library symbol (D180).
//!
//! The headers' layouts have one home, `crate::posix::net` ([`msghdr`], [`mmsghdr`],
//! [`sockaddr_storage`]), checked there against the headers at compile time. On both 64-bit
//! targets glibc's `struct msghdr` is the kernel's `struct user_msghdr` (include/linux/socket.h:
//! `int msg_namelen`, `__kernel_size_t msg_iovlen` and `msg_controllen`), so the kernel reads
//! the same bytes the C library's wrapper would hand it.
//!
//! Each call returns the kernel's result as `Result<_, Errno>` ([`raw::check`]): no `errno`.

use super::{
    fs::{OwnedFd, arg},
    net::MsgFlags,
    nr, raw,
};
use crate::posix::net::{mmsghdr, msghdr, sockaddr_storage};
use crate::{Errno, Fd, Timespec};

/// An `int` argument as the kernel reads it: sign-extended, the kernel takes the low 32 bits.
#[blaze::noalloc]
#[blaze::nopanic]
const fn int(value: i32) -> usize {
    value as isize as usize
}

/// `socketpair`: two connected sockets of `domain` (`AF_*`), `kind` (`SOCK_*`, with
/// `SOCK_CLOEXEC` or `SOCK_NONBLOCK` or'd in) and `protocol` (0: the family's default), each
/// owned. The kernel refuses what its family lacks (`AF_UNIX` is the one family with pairs).
/// replaces libc::socketpair (Linux)
#[blaze::noalloc]
#[blaze::nopanic]
pub fn socketpair(domain: i32, kind: i32, protocol: i32) -> Result<[OwnedFd; 2], Errno> {
    let mut ends: [i32; 2] = [-1, -1];
    // SAFETY: `ends` is two live `int`s the kernel writes (`int sv[2]`) and does not keep.
    raw::check(unsafe {
        raw::syscall4(nr::SOCKETPAIR, int(domain), int(kind), int(protocol), ends.as_mut_ptr() as usize)
    })?;
    let [a, b] = ends;
    // SAFETY: the call succeeded, so the kernel wrote two new descriptors, this call's alone.
    Ok(unsafe { [OwnedFd::from_raw(a), OwnedFd::from_raw(b)] })
}

/// `getsockname`: the socket's own address and its length as the kernel gives it (which is
/// the address's whole length: past a [`sockaddr_storage`] the bytes written stop at its end;
/// no family's address is that long).
/// replaces libc::getsockname (Linux)
#[blaze::noalloc]
#[blaze::nopanic]
pub fn getsockname(socket: Fd) -> Result<(sockaddr_storage, u32), Errno> {
    let mut at = sockaddr_storage::ZERO;
    let mut len = size_of::<sockaddr_storage>() as u32;
    // SAFETY: `at` is a live `sockaddr_storage` and `len` (an `int` the kernel reads and
    // writes) its size; the kernel writes at most that many bytes of address, and the length,
    // and keeps neither pointer.
    raw::check(unsafe {
        raw::syscall3(
            nr::GETSOCKNAME,
            arg(socket),
            core::ptr::from_mut(&mut at) as usize,
            core::ptr::from_mut(&mut len) as usize,
        )
    })?;
    Ok((at, len))
}

/// `setsockopt` of `len` bytes at `value`, at any `level` and `name`: the raw form, for an
/// option the typed [`super::setsockopt`] does not name (`IP_TOS`, `UDP_GRO`, a struct-typed
/// option). An option the socket's protocol lacks is the kernel's refusal, as it gives it.
/// replaces libc::setsockopt (Linux)
///
/// # Safety
///
/// `value` is `len` live, initialised bytes for the call.
#[blaze::noalloc]
#[blaze::nopanic]
pub unsafe fn setsockopt_raw(socket: Fd, level: i32, name: i32, value: *const u8, len: u32) -> Result<(), Errno> {
    // SAFETY: the caller's: `value` is `len` live bytes; the kernel reads them and keeps
    // nothing.
    raw::check(unsafe {
        raw::syscall5(nr::SETSOCKOPT, arg(socket), int(level), int(name), value as usize, len as usize)
    })
    .map(|_| ())
}

/// `getsockopt` into `len` bytes at `into`, at any `level` and `name`: how many bytes the
/// kernel wrote (at most `len`).
/// replaces libc::getsockopt (Linux)
///
/// # Safety
///
/// `into` is `len` live bytes held exclusively for the call.
#[blaze::noalloc]
#[blaze::nopanic]
pub unsafe fn getsockopt_raw(socket: Fd, level: i32, name: i32, into: *mut u8, len: u32) -> Result<u32, Errno> {
    let mut got = len;
    // SAFETY: the caller's for `into`'s `len` bytes, of which the kernel writes at most `got`;
    // `got` is a live `int` it reads and writes; it keeps neither pointer.
    raw::check(unsafe {
        raw::syscall5(
            nr::GETSOCKOPT,
            arg(socket),
            int(level),
            int(name),
            into as usize,
            core::ptr::from_mut(&mut got) as usize,
        )
    })?;
    Ok(got.min(len))
}

/// `sendmsg` with the whole header: an address, parts and control messages (`SCM_RIGHTS`),
/// which [`super::sendmsg`] does not carry: the bytes sent.
/// replaces libc::sendmsg (Linux)
///
/// # Safety
///
/// `message`'s pointers (`msg_name`, `msg_iov` and each part, `msg_control`) are live for
/// their lengths across the call.
#[blaze::noalloc]
#[blaze::nopanic]
pub unsafe fn sendmsg_header(socket: Fd, message: &msghdr, flags: MsgFlags) -> Result<usize, Errno> {
    // SAFETY: `message` is a live `struct user_msghdr` (the module's note); the caller's for
    // its pointers; the kernel reads them and keeps nothing.
    raw::check(unsafe {
        raw::syscall3(nr::SENDMSG, arg(socket), core::ptr::from_ref(message) as usize, flags.bits() as usize)
    })
}

/// `recvmsg` with the whole header: the peer's address, parts and control messages, and
/// `msg_namelen`, `msg_controllen` and `msg_flags` (`MSG_TRUNC`, `MSG_CTRUNC`) written back:
/// the bytes received, 0 once a stream's peer has closed.
/// replaces libc::recvmsg (Linux)
///
/// # Safety
///
/// `message`'s pointers (`msg_name`, `msg_iov` and each part, `msg_control`) are live and
/// held exclusively for their lengths across the call.
#[blaze::noalloc]
#[blaze::nopanic]
pub unsafe fn recvmsg_header(socket: Fd, message: &mut msghdr, flags: MsgFlags) -> Result<usize, Errno> {
    // SAFETY: `message` is a live `struct user_msghdr` held exclusively, whose lengths and
    // flags the kernel writes; the caller's for its pointers; nothing is kept.
    raw::check(unsafe {
        raw::syscall3(nr::RECVMSG, arg(socket), core::ptr::from_mut(message) as usize, flags.bits() as usize)
    })
}

/// `sendmmsg`: the messages of `messages` sent, in order: how many were (at least one), each
/// one's `msg_len` set to its bytes. [`Errno::Inval`] for more than a `u32` holds, before the
/// call (the kernel itself sends at most `UIO_MAXIOV`, 1024, and says how many).
/// replaces libc::sendmmsg
///
/// # Safety
///
/// Every header's pointers (`msg_name`, `msg_iov` and each part, `msg_control`) are live for
/// their lengths across the call.
#[blaze::noalloc]
#[blaze::nopanic]
pub unsafe fn sendmmsg(socket: Fd, messages: &mut [mmsghdr], flags: MsgFlags) -> Result<usize, Errno> {
    let Ok(count) = u32::try_from(messages.len()) else {
        return Err(Errno::Inval);
    };
    // SAFETY: `messages` is `count` live `struct mmsghdr`s held exclusively, of which the
    // kernel writes only the `msg_len`s; the caller's for each header's pointers; nothing is
    // kept.
    raw::check(unsafe {
        raw::syscall4(nr::SENDMMSG, arg(socket), messages.as_mut_ptr() as usize, count as usize, flags.bits() as usize)
    })
}

/// `recvmmsg`: datagrams received into `messages`, in order: how many, each one's `msg_len`
/// its bytes and its header's lengths and `msg_flags` what was found (`MSG_TRUNC`). With
/// `timeout` (written back with the time left), the call returns after it once a first
/// datagram came (`recvmmsg(2)`'s caveat: it is checked only after each datagram). The
/// kernel's `struct __kernel_timespec` is [`Timespec`] on both 64-bit targets.
/// [`Errno::Inval`] for more messages than a `u32` holds, before the call.
/// replaces libc::recvmmsg
///
/// # Safety
///
/// Every header's pointers (`msg_name`, `msg_iov` and each part, `msg_control`) are live and
/// held exclusively for their lengths across the call.
#[blaze::noalloc]
#[blaze::nopanic]
pub unsafe fn recvmmsg(
    socket: Fd,
    messages: &mut [mmsghdr],
    flags: MsgFlags,
    timeout: Option<&mut Timespec>,
) -> Result<usize, Errno> {
    let Ok(count) = u32::try_from(messages.len()) else {
        return Err(Errno::Inval);
    };
    let timeout = match timeout {
        Some(timeout) => core::ptr::from_mut(timeout) as usize,
        None => 0,
    };
    // SAFETY: `messages` is `count` live headers and `timeout` null or a live `Timespec`, all
    // held exclusively; the kernel writes the headers' lengths and flags and the time left;
    // the caller's for each header's pointers; nothing is kept.
    raw::check(unsafe {
        raw::syscall5(
            nr::RECVMMSG,
            arg(socket),
            messages.as_mut_ptr() as usize,
            count as usize,
            flags.bits() as usize,
            timeout,
        )
    })
}
