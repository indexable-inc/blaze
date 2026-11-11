//! Sockets: what `Effect::Listen`, `Connect`, `Recv`, `Send` and `Hangup` use.
//!
//! Slice 1 sends and receives bytes only: no peer address is returned by [`accept4`] or
//! [`recvmsg`], and no control message (a passed descriptor, credentials) is carried. Each is
//! a field of [`MsgHdr`] left null, and gets its typed form when a driver needs it.

use super::{
    fs::{OwnedFd, arg, owned},
    nr, raw,
};
use crate::{Errno, Fd};

/// An address family.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Family {
    /// `AF_UNIX`.
    Unix,
    /// `AF_INET`.
    V4,
    /// `AF_INET6`.
    V6,
}

impl Family {
    const fn raw(self) -> u16 {
        match self {
            Family::Unix => 1,
            Family::V4 => 2,
            Family::V6 => 10,
        }
    }
}

/// A socket's kind.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SockKind {
    /// `SOCK_STREAM`.
    Stream,
    /// `SOCK_DGRAM`.
    Datagram,
    /// `SOCK_SEQPACKET`.
    SeqPacket,
}

flag_set! {
    /// The flags a new socket's descriptor gets (`socket`, `accept4`).
    SockFlags: u32 {
        /// `SOCK_NONBLOCK`.
        NONBLOCK = 0o4000;
        /// `SOCK_CLOEXEC`.
        CLOSE_ON_EXEC = 0o2000000;
    }
}

/// `socket`: a new socket of `family` and `kind`, of the family's default protocol.
#[blaze::noalloc]
#[blaze::nopanic]
pub fn socket(family: Family, kind: SockKind, flags: SockFlags) -> Result<OwnedFd, Errno> {
    let kind: u32 = match kind {
        SockKind::Stream => 1,
        SockKind::Datagram => 2,
        SockKind::SeqPacket => 5,
    };
    // SAFETY: the call takes numbers and no memory.
    owned(unsafe {
        raw::syscall3(nr::SOCKET, family.raw() as usize, (kind | flags.bits()) as usize, 0)
    })
}

/// `struct sockaddr_un`: a path of up to 107 bytes and its NUL.
#[derive(Clone, Copy)]
#[repr(C)]
pub struct UnixAddr {
    family: u16,
    path: [u8; 108],
}

/// `struct sockaddr_in`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(C)]
pub struct V4Addr {
    family: u16,
    port_be: [u8; 2],
    ip: [u8; 4],
    zero: [u8; 8],
}

/// `struct sockaddr_in6`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(C)]
pub struct V6Addr {
    family: u16,
    port_be: [u8; 2],
    flow_be: [u8; 4],
    ip: [u8; 16],
    scope: u32,
}

/// A socket address, as `connect` and `bind` take it.
#[derive(Clone, Copy)]
pub enum SockAddr {
    /// A path in the file system.
    Unix(UnixAddr),
    /// An IPv4 address and port.
    V4(V4Addr),
    /// An IPv6 address and port.
    V6(V6Addr),
}

impl SockAddr {
    /// The Unix socket at `path`: [`Errno::NameTooLong`] over 107 bytes, [`Errno::Inval`]
    /// for an empty path or one holding a NUL.
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub fn unix(path: &[u8]) -> Result<SockAddr, Errno> {
        if path.is_empty() {
            return Err(Errno::Inval);
        }
        let mut address = UnixAddr { family: Family::Unix.raw(), path: [0; 108] };
        // One byte is left for the NUL.
        let Some(room) = address.path.get_mut(..path.len()).filter(|_| path.len() < 108) else {
            return Err(Errno::NameTooLong);
        };
        // By index with `get`: `contains` and a slice's iterator both have a check that
        // aborts, which no nopanic function may reach. `room` is as long as `path`.
        let mut at = 0usize;
        while let (Some(&byte), Some(to)) = (path.get(at), room.get_mut(at)) {
            if byte == 0 {
                return Err(Errno::Inval);
            }
            *to = byte;
            // Below `path.len()`, so it cannot wrap; the wrapping form has no overflow check.
            at = at.wrapping_add(1);
        }
        Ok(SockAddr::Unix(address))
    }

    /// The IPv4 address `ip` and `port`.
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub const fn v4(ip: [u8; 4], port: u16) -> SockAddr {
        SockAddr::V4(V4Addr { family: 2, port_be: port.to_be_bytes(), ip, zero: [0; 8] })
    }

    /// The IPv6 address `ip` and `port`, in the scope `scope` (an interface's index, for a
    /// link-local address; else 0).
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub const fn v6(ip: [u8; 16], port: u16, scope: u32) -> SockAddr {
        SockAddr::V6(V6Addr { family: 10, port_be: port.to_be_bytes(), flow_be: [0; 4], ip, scope })
    }

    /// The address's bytes and their length, as the kernel reads them.
    #[blaze::noalloc]
    #[blaze::nopanic]
    fn raw(&self) -> (usize, usize) {
        match self {
            SockAddr::Unix(it) => (core::ptr::from_ref(it) as usize, size_of::<UnixAddr>()),
            SockAddr::V4(it) => (core::ptr::from_ref(it) as usize, size_of::<V4Addr>()),
            SockAddr::V6(it) => (core::ptr::from_ref(it) as usize, size_of::<V6Addr>()),
        }
    }
}

/// `connect`: the socket connected to `to`. On a non-blocking socket [`Errno::InProgress`]
/// says it is under way, and a poll for writing says when it ended.
#[blaze::noalloc]
#[blaze::nopanic]
pub fn connect(socket: Fd, to: &SockAddr) -> Result<(), Errno> {
    let (address, len) = to.raw();
    // SAFETY: `address` is `len` live bytes of a socket address the kernel reads.
    raw::check(unsafe { raw::syscall3(nr::CONNECT, arg(socket), address, len) }).map(|_| ())
}

/// `bind`: the socket given the address `at`.
#[blaze::noalloc]
#[blaze::nopanic]
pub fn bind(socket: Fd, at: &SockAddr) -> Result<(), Errno> {
    let (address, len) = at.raw();
    // SAFETY: as `connect`.
    raw::check(unsafe { raw::syscall3(nr::BIND, arg(socket), address, len) }).map(|_| ())
}

/// `listen`: the socket takes connections, up to `backlog` waiting to be accepted.
#[blaze::noalloc]
#[blaze::nopanic]
pub fn listen(socket: Fd, backlog: u32) -> Result<(), Errno> {
    // SAFETY: the call takes numbers and no memory.
    raw::check(unsafe { raw::syscall2(nr::LISTEN, arg(socket), backlog as usize) }).map(|_| ())
}

/// `accept4`: the next connection of a listening socket, its descriptor with `flags`. The
/// peer's address is not asked for. [`Errno::Again`] on a non-blocking socket with none.
#[blaze::noalloc]
#[blaze::nopanic]
pub fn accept4(socket: Fd, flags: SockFlags) -> Result<OwnedFd, Errno> {
    // SAFETY: no address is asked for (both pointers null), so the kernel writes no memory.
    owned(unsafe { raw::syscall4(nr::ACCEPT4, arg(socket), 0, 0, flags.bits() as usize) })
}

flag_set! {
    /// `sendmsg`'s and `recvmsg`'s flags.
    MsgFlags: u32 {
        /// `MSG_PEEK`.
        PEEK = 0x02;
        /// `MSG_DONTWAIT`.
        DONT_WAIT = 0x40;
        /// `MSG_WAITALL`.
        WAIT_ALL = 0x100;
        /// `MSG_NOSIGNAL`: a send to a closed peer is [`Errno::Pipe`] and no `SIGPIPE`.
        NO_SIGNAL = 0x4000;
        /// `MSG_CMSG_CLOEXEC`.
        CMSG_CLOSE_ON_EXEC = 0x4000_0000;
    }
}

/// `struct iovec` over bytes to send.
#[derive(Clone, Copy)]
#[repr(C)]
pub struct IoSlice<'a> {
    base: *const u8,
    len: usize,
    bytes: core::marker::PhantomData<&'a [u8]>,
}

impl<'a> IoSlice<'a> {
    /// `bytes`, lent for a send.
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub const fn new(bytes: &'a [u8]) -> IoSlice<'a> {
        IoSlice { base: bytes.as_ptr(), len: bytes.len(), bytes: core::marker::PhantomData }
    }
}

/// `struct iovec` over room to receive into.
#[repr(C)]
pub struct IoSliceMut<'a> {
    base: *mut u8,
    len: usize,
    bytes: core::marker::PhantomData<&'a mut [u8]>,
}

impl<'a> IoSliceMut<'a> {
    /// `into`, lent for a receive.
    #[blaze::noalloc]
    #[blaze::nopanic]
    pub const fn new(into: &'a mut [u8]) -> IoSliceMut<'a> {
        IoSliceMut { base: into.as_mut_ptr(), len: into.len(), bytes: core::marker::PhantomData }
    }
}

/// `struct msghdr` on a 64-bit target (56 bytes).
#[repr(C)]
struct MsgHdr {
    name: usize,
    name_len: u32,
    iov: usize,
    iov_len: usize,
    control: usize,
    control_len: usize,
    flags: i32,
}

/// `sendmsg`: the bytes of `parts`, in order, sent on the socket: how many were sent, which
/// may be fewer than all (the caller continues). `Effect::Send`.
#[blaze::noalloc]
#[blaze::nopanic]
pub fn sendmsg(socket: Fd, parts: &[IoSlice<'_>], flags: MsgFlags) -> Result<usize, Errno> {
    let message = MsgHdr {
        name: 0,
        name_len: 0,
        iov: parts.as_ptr() as usize,
        iov_len: parts.len(),
        control: 0,
        control_len: 0,
        flags: 0,
    };
    // SAFETY: `message` is a live `msghdr` naming `parts.len()` live `iovec`s, each over
    // live bytes the kernel only reads; no name and no control (null); nothing is kept.
    raw::check(unsafe {
        raw::syscall3(nr::SENDMSG, arg(socket), core::ptr::from_ref(&message) as usize, flags.bits() as usize)
    })
}

/// `recvmsg`: bytes received into `parts`, in order: how many, 0 once a stream's peer has
/// closed. `Effect::Recv`.
#[blaze::noalloc]
#[blaze::nopanic]
pub fn recvmsg(socket: Fd, parts: &mut [IoSliceMut<'_>], flags: MsgFlags) -> Result<usize, Errno> {
    let mut message = MsgHdr {
        name: 0,
        name_len: 0,
        iov: parts.as_mut_ptr() as usize,
        iov_len: parts.len(),
        control: 0,
        control_len: 0,
        flags: 0,
    };
    // SAFETY: `message` is a live `msghdr` naming `parts.len()` live `iovec`s, each over
    // memory this call holds exclusively and the kernel writes at most its length of; no
    // name and no control (null); nothing is kept.
    raw::check(unsafe {
        raw::syscall3(nr::RECVMSG, arg(socket), core::ptr::from_mut(&mut message) as usize, flags.bits() as usize)
    })
}

/// Which directions a `shutdown` closes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Shutdown {
    /// `SHUT_RD`.
    Read,
    /// `SHUT_WR`.
    Write,
    /// `SHUT_RDWR`.
    Both,
}

/// `shutdown`: the socket's `which` directions closed; the peer reads the end.
/// `Effect::Hangup`.
#[blaze::noalloc]
#[blaze::nopanic]
pub fn shutdown(socket: Fd, which: Shutdown) -> Result<(), Errno> {
    let how = match which {
        Shutdown::Read => 0,
        Shutdown::Write => 1,
        Shutdown::Both => 2,
    };
    // SAFETY: the call takes numbers and no memory.
    raw::check(unsafe { raw::syscall2(nr::SHUTDOWN, arg(socket), how) }).map(|_| ())
}
