//! Sockets through the C library's surface (D180): the `#[repr(C)]` address, message and
//! option types with the headers' field names, the constants per system, the `CMSG_*` macros
//! as functions, and typed calls (`Result<_, Errno>`) over both systems.
//!
//! On Linux every call is the kernel's, by raw system call, and no C-library symbol is named
//! (D180): [`socket`], [`bind`], [`connect`], [`listen`], [`send`] and [`recv`] go to
//! `crate::linux::{socket, bind, connect, listen, sendmsg, recvmsg}`, the rest
//! ([`socketpair`], [`getsockname`], the options, the whole-header [`sendmsg`] / [`recvmsg`],
//! [`sendmmsg`], [`recvmmsg`]) to `crate::linux::sock`. On macOS each call is libSystem's
//! (the raw declarations are in `c`, a macOS-only module), through `crate::macos::cold`
//! where that table has it.
//!
//! The seccomp filter's classic BPF opcodes and return actions are here too (Linux), beside
//! `crate::linux::{SockFilter, seccomp_filter}`, which are the replacements of `libc::sock_filter`,
//! `libc::sock_fprog` and the `prctl(PR_SET_SECCOMP)` / `seccomp(SECCOMP_SET_MODE_FILTER)` call.
//!
//! Every item says what it replaces (`/// replaces libc::<name>`), so a port greps for it.

#![allow(non_camel_case_types, non_snake_case, reason = "the C headers' names, so a port is by name")]

#[cfg(target_os = "macos")]
pub mod c;

use core::ffi::{c_char, c_int, c_uint};
use core::ptr;

use super::io::IoSliceMut;
#[cfg(target_os = "macos")]
use super::types::pid_t;
#[cfg(target_os = "linux")]
use super::types::size_t;
use super::types::{sa_family_t, socklen_t};
use crate::{Errno, Fd};

// ---------------------------------------------------------------------------------------------
// Addresses.
// ---------------------------------------------------------------------------------------------

/// `struct in_addr` (netinet/in.h): an IPv4 address, in network byte order.
/// replaces libc::in_addr
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
#[repr(C)]
pub struct in_addr {
    /// `in_addr_t`, network byte order.
    pub s_addr: u32,
}

/// `struct in6_addr` (netinet/in.h): an IPv6 address; aligned to 4 as the headers' union of
/// `uint32_t`s makes it.
/// replaces libc::in6_addr
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
#[repr(C, align(4))]
pub struct in6_addr {
    /// The sixteen bytes, network order.
    pub s6_addr: [u8; 16],
}

/// `struct sockaddr` (sys/socket.h): the generic address every call's prototype takes; a
/// caller passes a [`sockaddr_storage`] or a family's own address cast to it.
/// replaces libc::sockaddr
#[derive(Clone, Copy)]
#[repr(C)]
pub struct sockaddr {
    /// macOS: the address's length.
    #[cfg(target_os = "macos")]
    pub sa_len: u8,
    /// `AF_*`.
    pub sa_family: sa_family_t,
    /// The family's bytes.
    pub sa_data: [c_char; 14],
}

/// `struct sockaddr_storage` (sys/socket.h): room for any family's address, aligned for each.
/// replaces libc::sockaddr_storage
#[derive(Clone, Copy)]
#[repr(C)]
pub struct sockaddr_storage {
    /// macOS: the address's length.
    #[cfg(target_os = "macos")]
    pub ss_len: u8,
    /// `AF_*`: which family's address this holds.
    pub ss_family: sa_family_t,
    #[cfg(target_os = "linux")]
    pad: [u8; 118],
    #[cfg(target_os = "macos")]
    pad1: [u8; 6],
    #[cfg(target_os = "linux")]
    align: u64,
    #[cfg(target_os = "macos")]
    align: i64,
    #[cfg(target_os = "macos")]
    pad2: [u8; 112],
}

impl sockaddr_storage {
    /// All zeros: no address (`AF_UNSPEC`), the room a call writes one into.
    #[cfg(target_os = "linux")]
    pub const ZERO: sockaddr_storage = sockaddr_storage { ss_family: 0, pad: [0; 118], align: 0 };
    /// All zeros: no address (`AF_UNSPEC`), the room a call writes one into.
    #[cfg(target_os = "macos")]
    pub const ZERO: sockaddr_storage =
        sockaddr_storage { ss_len: 0, ss_family: 0, pad1: [0; 6], align: 0, pad2: [0; 112] };
}

/// `struct sockaddr_in` (netinet/in.h): an IPv4 address and port.
/// replaces libc::sockaddr_in
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(C)]
pub struct sockaddr_in {
    /// macOS: the address's length (16).
    #[cfg(target_os = "macos")]
    pub sin_len: u8,
    /// `AF_INET`.
    pub sin_family: sa_family_t,
    /// `in_port_t`, network byte order.
    pub sin_port: u16,
    /// The address.
    pub sin_addr: in_addr,
    /// Zeros.
    #[cfg(target_os = "linux")]
    pub sin_zero: [u8; 8],
    /// Zeros.
    #[cfg(target_os = "macos")]
    pub sin_zero: [c_char; 8],
}

impl sockaddr_in {
    /// All zeros.
    pub const ZERO: sockaddr_in = sockaddr_in {
        #[cfg(target_os = "macos")]
        sin_len: 0,
        sin_family: 0,
        sin_port: 0,
        sin_addr: in_addr { s_addr: 0 },
        sin_zero: [0; 8],
    };
}

/// `struct sockaddr_in6` (netinet/in.h): an IPv6 address, port, flow label and scope.
/// replaces libc::sockaddr_in6
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(C)]
pub struct sockaddr_in6 {
    /// macOS: the address's length (28).
    #[cfg(target_os = "macos")]
    pub sin6_len: u8,
    /// `AF_INET6`.
    pub sin6_family: sa_family_t,
    /// `in_port_t`, network byte order.
    pub sin6_port: u16,
    /// The flow information, network byte order.
    pub sin6_flowinfo: u32,
    /// The address.
    pub sin6_addr: in6_addr,
    /// The scope: an interface's index for a link-local address.
    pub sin6_scope_id: u32,
}

impl sockaddr_in6 {
    /// All zeros.
    pub const ZERO: sockaddr_in6 = sockaddr_in6 {
        #[cfg(target_os = "macos")]
        sin6_len: 0,
        sin6_family: 0,
        sin6_port: 0,
        sin6_flowinfo: 0,
        sin6_addr: in6_addr { s6_addr: [0; 16] },
        sin6_scope_id: 0,
    };
}

/// `struct sockaddr_un` (sys/un.h): a path in the file system. 108 bytes of path on Linux,
/// 104 on macOS, a NUL included.
/// replaces libc::sockaddr_un
#[derive(Clone, Copy)]
#[repr(C)]
pub struct sockaddr_un {
    /// macOS: the address's length.
    #[cfg(target_os = "macos")]
    pub sun_len: u8,
    /// `AF_UNIX`.
    pub sun_family: sa_family_t,
    /// The path and its NUL.
    #[cfg(target_os = "linux")]
    pub sun_path: [c_char; 108],
    /// The path and its NUL.
    #[cfg(target_os = "macos")]
    pub sun_path: [c_char; 104],
}

impl sockaddr_un {
    /// All zeros.
    pub const ZERO: sockaddr_un = sockaddr_un {
        #[cfg(target_os = "macos")]
        sun_len: 0,
        sun_family: 0,
        #[cfg(target_os = "linux")]
        sun_path: [0; 108],
        #[cfg(target_os = "macos")]
        sun_path: [0; 104],
    };
}

/// `struct in_pktinfo` (netinet/in.h): `IP_PKTINFO`'s control message, the interface and
/// the address a datagram arrived at or leaves from.
/// replaces libc::in_pktinfo
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(C)]
pub struct in_pktinfo {
    /// The interface's index (`int` on Linux, `unsigned int` on macOS).
    #[cfg(target_os = "linux")]
    pub ipi_ifindex: c_int,
    /// The interface's index (`int` on Linux, `unsigned int` on macOS).
    #[cfg(target_os = "macos")]
    pub ipi_ifindex: c_uint,
    /// The local address the reply leaves from.
    pub ipi_spec_dst: in_addr,
    /// The header's destination address.
    pub ipi_addr: in_addr,
}

impl in_pktinfo {
    /// All zeros.
    pub const ZERO: in_pktinfo =
        in_pktinfo { ipi_ifindex: 0, ipi_spec_dst: in_addr { s_addr: 0 }, ipi_addr: in_addr { s_addr: 0 } };
}

/// `struct in6_pktinfo` (netinet/in.h; `netinet6/in6.h` on macOS): `IPV6_PKTINFO`'s
/// control message.
/// replaces libc::in6_pktinfo
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(C)]
pub struct in6_pktinfo {
    /// The address.
    pub ipi6_addr: in6_addr,
    /// The interface's index.
    pub ipi6_ifindex: c_uint,
}

impl in6_pktinfo {
    /// All zeros.
    pub const ZERO: in6_pktinfo = in6_pktinfo { ipi6_addr: in6_addr { s6_addr: [0; 16] }, ipi6_ifindex: 0 };
}

/// `struct linger` (sys/socket.h): `SO_LINGER`'s value (`SO_LINGER_SEC`'s on macOS).
/// replaces libc::linger
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
#[repr(C)]
pub struct linger {
    /// Whether a close lingers.
    pub l_onoff: c_int,
    /// How long: seconds on Linux and for `SO_LINGER_SEC`; clock ticks for macOS's `SO_LINGER`.
    pub l_linger: c_int,
}

/// `struct ip_mreq` (netinet/in.h): `IP_ADD_MEMBERSHIP`'s value — an IPv4 multicast group and
/// the local interface address to join it on.
/// replaces libc::ip_mreq
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
#[repr(C)]
pub struct ip_mreq {
    /// IPv4 multicast group address, network byte order.
    pub imr_multiaddr: in_addr,
    /// Local interface IPv4 address (`INADDR_ANY` = 0 for default), network byte order.
    pub imr_interface: in_addr,
}

// ---------------------------------------------------------------------------------------------
// Messages and control messages.
// ---------------------------------------------------------------------------------------------

/// `struct msghdr` (sys/socket.h): one message's address, parts and control messages. Its
/// lengths are `size_t` on Linux (glibc and musl alike on 64 bits) and `int` / `socklen_t` on
/// macOS.
/// replaces libc::msghdr
#[derive(Clone, Copy)]
#[repr(C)]
pub struct msghdr {
    /// The peer's address, or null.
    pub msg_name: *mut core::ffi::c_void,
    /// Its length.
    pub msg_namelen: socklen_t,
    /// The parts: `struct iovec`s, which are [`IoSliceMut`] (one layout for sending and
    /// receiving; the borrow is the caller's to keep live, as the `unsafe` calls say).
    pub msg_iov: *mut IoSliceMut<'static>,
    /// How many parts.
    #[cfg(target_os = "linux")]
    pub msg_iovlen: size_t,
    /// How many parts.
    #[cfg(target_os = "macos")]
    pub msg_iovlen: c_int,
    /// The control messages' buffer, or null.
    pub msg_control: *mut core::ffi::c_void,
    /// Its length.
    #[cfg(target_os = "linux")]
    pub msg_controllen: size_t,
    /// Its length.
    #[cfg(target_os = "macos")]
    pub msg_controllen: socklen_t,
    /// What the receive found (`MSG_TRUNC`, ...).
    pub msg_flags: c_int,
}

impl msghdr {
    /// No address, no part, no control message.
    pub const ZERO: msghdr = msghdr {
        msg_name: ptr::null_mut(),
        msg_namelen: 0,
        msg_iov: ptr::null_mut(),
        msg_iovlen: 0,
        msg_control: ptr::null_mut(),
        msg_controllen: 0,
        msg_flags: 0,
    };
}

/// `struct mmsghdr` (sys/socket.h, Linux): one message of `sendmmsg` or `recvmmsg` and the
/// bytes it moved.
/// replaces libc::mmsghdr
#[cfg(target_os = "linux")]
#[derive(Clone, Copy)]
#[repr(C)]
pub struct mmsghdr {
    /// The message.
    pub msg_hdr: msghdr,
    /// The bytes sent or received.
    pub msg_len: c_uint,
}

#[cfg(target_os = "linux")]
impl mmsghdr {
    /// An empty message.
    pub const ZERO: mmsghdr = mmsghdr { msg_hdr: msghdr::ZERO, msg_len: 0 };
}

/// `struct cmsghdr` (sys/socket.h): one control message's header; its data follows at
/// [`CMSG_DATA`].
/// replaces libc::cmsghdr
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(C)]
pub struct cmsghdr {
    /// The header's and the data's length, unpadded ([`CMSG_LEN`]).
    #[cfg(target_os = "linux")]
    pub cmsg_len: size_t,
    /// The header's and the data's length, unpadded ([`CMSG_LEN`]).
    #[cfg(target_os = "macos")]
    pub cmsg_len: socklen_t,
    /// The protocol's level (`IPPROTO_IP`, ...).
    pub cmsg_level: c_int,
    /// The message's kind (`IP_TOS`, ...).
    pub cmsg_type: c_int,
}

/// The alignment of a control message and its data: `size_t`'s on Linux (glibc's
/// `CMSG_ALIGN`), 4 on macOS (`__DARWIN_ALIGN32`).
#[cfg(target_os = "linux")]
const CMSG_ALIGNMENT: usize = size_of::<usize>();
#[cfg(target_os = "macos")]
const CMSG_ALIGNMENT: usize = 4;

/// `len` rounded up to a control message's alignment. Saturates instead of wrapping: no
/// control buffer is within an alignment of `usize::MAX`.
#[blaze::noalloc]
#[blaze::nopanic]
const fn cmsg_align(len: usize) -> usize {
    len.saturating_add(CMSG_ALIGNMENT - 1) & !(CMSG_ALIGNMENT - 1)
}

/// The header's length, aligned: where its data starts.
const CMSG_HEADER: usize = cmsg_align(size_of::<cmsghdr>());

/// `CMSG_LEN(len)`: a control message's `cmsg_len` for `len` bytes of data.
/// replaces libc::CMSG_LEN
#[blaze::noalloc]
#[blaze::nopanic]
pub const fn CMSG_LEN(len: c_uint) -> c_uint {
    (CMSG_HEADER as c_uint).saturating_add(len)
}

/// `CMSG_SPACE(len)`: the bytes of control buffer a message of `len` bytes of data takes,
/// padding included.
/// replaces libc::CMSG_SPACE
#[blaze::noalloc]
#[blaze::nopanic]
pub const fn CMSG_SPACE(len: c_uint) -> c_uint {
    let space = CMSG_HEADER.saturating_add(cmsg_align(len as usize));
    if space > c_uint::MAX as usize { c_uint::MAX } else { space as c_uint }
}

/// `CMSG_FIRSTHDR(mhdr)`: the first control message of `mhdr`'s buffer, or null when the
/// buffer cannot hold a header.
/// replaces libc::CMSG_FIRSTHDR
#[blaze::noalloc]
#[blaze::nopanic]
pub const fn CMSG_FIRSTHDR(mhdr: &msghdr) -> *mut cmsghdr {
    if (mhdr.msg_controllen as usize) >= size_of::<cmsghdr>() {
        mhdr.msg_control.cast()
    } else {
        ptr::null_mut()
    }
}

/// `CMSG_DATA(cmsg)`: where the control message's data starts. Address arithmetic only:
/// nothing is read.
/// replaces libc::CMSG_DATA
#[blaze::noalloc]
#[blaze::nopanic]
pub const fn CMSG_DATA(cmsg: *const cmsghdr) -> *mut u8 {
    cmsg.cast::<u8>().wrapping_add(CMSG_HEADER).cast_mut()
}

/// `CMSG_NXTHDR(mhdr, cmsg)`: the control message after `cmsg` in `mhdr`'s buffer, or null
/// when there is none. As glibc's `__cmsg_nxthdr`, on both systems: null for a header whose
/// `cmsg_len` is below a header's (macOS's macro would step by 0 and never end), for a next
/// header that does not fit the buffer, and for one whose own length runs past it.
/// replaces libc::CMSG_NXTHDR
///
/// # Safety
///
/// `cmsg` is a header within `mhdr`'s control buffer (from [`CMSG_FIRSTHDR`] or this), and
/// the buffer is `msg_controllen` live bytes.
#[blaze::noalloc]
#[blaze::nopanic]
pub unsafe fn CMSG_NXTHDR(mhdr: &msghdr, cmsg: *const cmsghdr) -> *mut cmsghdr {
    if cmsg.is_null() {
        return CMSG_FIRSTHDR(mhdr);
    }
    // SAFETY: the caller's: `cmsg` is a header within the live buffer.
    let len = unsafe { (*cmsg).cmsg_len } as usize;
    if len < size_of::<cmsghdr>() {
        return ptr::null_mut();
    }
    let start = mhdr.msg_control as usize;
    let Some(end) = start.checked_add(mhdr.msg_controllen as usize) else {
        return ptr::null_mut();
    };
    let Some(next) = (cmsg as usize).checked_add(cmsg_align(len)) else {
        return ptr::null_mut();
    };
    match next.checked_add(size_of::<cmsghdr>()) {
        Some(header_end) if header_end <= end => {}
        _ => return ptr::null_mut(),
    }
    let next_header = cmsg.cast::<u8>().wrapping_add(cmsg_align(len)).cast::<cmsghdr>().cast_mut();
    // SAFETY: `next_header` and its whole header lie within the live buffer (checked above).
    let next_len = unsafe { (*next_header).cmsg_len } as usize;
    match next.checked_add(cmsg_align(next_len)) {
        Some(next_end) if next_end <= end => next_header,
        _ => ptr::null_mut(),
    }
}

// ---------------------------------------------------------------------------------------------
// Constants: families, kinds, levels, options and flags, per system.
// ---------------------------------------------------------------------------------------------

/// `AF_UNIX` (sys/socket.h).
/// replaces libc::AF_UNIX
pub const AF_UNIX: c_int = 1;
/// `AF_INET` (sys/socket.h).
/// replaces libc::AF_INET
pub const AF_INET: c_int = 2;
/// `AF_INET6` (sys/socket.h).
/// replaces libc::AF_INET6
#[cfg(target_os = "linux")]
pub const AF_INET6: c_int = 10;
/// `AF_INET6` (sys/socket.h).
/// replaces libc::AF_INET6
#[cfg(target_os = "macos")]
pub const AF_INET6: c_int = 30;

/// `SOCK_STREAM` (sys/socket.h).
/// replaces libc::SOCK_STREAM
pub const SOCK_STREAM: c_int = 1;
/// `SOCK_DGRAM` (sys/socket.h).
/// replaces libc::SOCK_DGRAM
pub const SOCK_DGRAM: c_int = 2;
/// `SOCK_CLOEXEC` (bits/socket_type.h, Linux): or'd into a socket's kind.
/// replaces libc::SOCK_CLOEXEC
#[cfg(target_os = "linux")]
pub const SOCK_CLOEXEC: c_int = 0o2000000;
/// `SOCK_NONBLOCK` (bits/socket_type.h, Linux): or'd into a socket's kind; the same bit as
/// `O_NONBLOCK` on aarch64 and x86-64. macOS has none (`fionbio` after the call).
/// replaces libc::SOCK_NONBLOCK (blaze_sys::posix::net::SOCK_NONBLOCK, or linux::SockFlags::NONBLOCK in the typed calls; Linux only)
#[cfg(target_os = "linux")]
pub const SOCK_NONBLOCK: c_int = 0o4000;

/// `SOMAXCONN` (sys/socket.h): the largest backlog `listen` keeps.
/// replaces libc::SOMAXCONN
#[cfg(target_os = "linux")]
pub const SOMAXCONN: c_int = 4096;
/// `SOMAXCONN` (sys/socket.h): the largest backlog `listen` keeps.
/// replaces libc::SOMAXCONN
#[cfg(target_os = "macos")]
pub const SOMAXCONN: c_int = 128;

/// `SHUT_RDWR` (sys/socket.h): both directions; `crate::linux::Shutdown::Both` in the typed
/// call. Kept for an io_uring `shutdown`'s operand.
/// replaces libc::SHUT_RDWR
pub const SHUT_RDWR: c_int = 2;

/// `IPPROTO_IP` (netinet/in.h).
/// replaces libc::IPPROTO_IP
pub const IPPROTO_IP: c_int = 0;
/// `IPPROTO_TCP` (netinet/in.h).
/// replaces libc::IPPROTO_TCP
pub const IPPROTO_TCP: c_int = 6;
/// `IPPROTO_IPV6` (netinet/in.h).
/// replaces libc::IPPROTO_IPV6
pub const IPPROTO_IPV6: c_int = 41;
/// `SOL_UDP` (sys/socket.h, Linux): `IPPROTO_UDP` as an option level.
/// replaces libc::SOL_UDP
#[cfg(target_os = "linux")]
pub const SOL_UDP: c_int = 17;
/// `SOL_LOCAL` (sys/un.h, macOS): a Unix socket's own options.
/// replaces libc::SOL_LOCAL
#[cfg(target_os = "macos")]
pub const SOL_LOCAL: c_int = 0;
/// `LOCAL_PEERPID` (sys/un.h, macOS): the peer's process id, a `pid_t` ([`peer_pid`]).
/// replaces libc::LOCAL_PEERPID
#[cfg(target_os = "macos")]
pub const LOCAL_PEERPID: c_int = 0x002;

/// The `SOL_SOCKET` options: asm-generic/socket.h on Linux (aarch64 and x86-64 alike),
/// sys/socket.h on macOS.
#[cfg(target_os = "linux")]
mod sol {
    use core::ffi::c_int;
    pub const SOL_SOCKET: c_int = 1;
    pub const SO_REUSEADDR: c_int = 2;
    pub const SO_TYPE: c_int = 3;
    pub const SO_ERROR: c_int = 4;
    pub const SO_BROADCAST: c_int = 6;
    pub const SO_SNDBUF: c_int = 7;
    pub const SO_RCVBUF: c_int = 8;
    pub const SO_KEEPALIVE: c_int = 9;
    pub const SO_LINGER: c_int = 13;
    pub const SO_REUSEPORT: c_int = 15;
    pub const SO_ACCEPTCONN: c_int = 30;
}
#[cfg(target_os = "macos")]
mod sol {
    use core::ffi::c_int;
    pub const SOL_SOCKET: c_int = 0xffff;
    pub const SO_ACCEPTCONN: c_int = 0x0002;
    pub const SO_REUSEADDR: c_int = 0x0004;
    pub const SO_KEEPALIVE: c_int = 0x0008;
    pub const SO_BROADCAST: c_int = 0x0020;
    pub const SO_LINGER: c_int = 0x0080;
    pub const SO_REUSEPORT: c_int = 0x0200;
    pub const SO_SNDBUF: c_int = 0x1001;
    pub const SO_RCVBUF: c_int = 0x1002;
    pub const SO_ERROR: c_int = 0x1007;
    pub const SO_TYPE: c_int = 0x1008;
    pub const SO_NOSIGPIPE: c_int = 0x1022;
    pub const SO_LINGER_SEC: c_int = 0x1080;
}

/// `SOL_SOCKET`.
/// replaces libc::SOL_SOCKET
pub const SOL_SOCKET: c_int = sol::SOL_SOCKET;
/// `SO_REUSEADDR`.
/// replaces libc::SO_REUSEADDR
pub const SO_REUSEADDR: c_int = sol::SO_REUSEADDR;
/// `SO_TYPE`: the socket's kind (`SOCK_STREAM`, ...).
/// replaces libc::SO_TYPE
pub const SO_TYPE: c_int = sol::SO_TYPE;
/// `SO_ERROR`: the pending error, cleared by the read ([`pending_error`]).
/// replaces libc::SO_ERROR
pub const SO_ERROR: c_int = sol::SO_ERROR;
/// `SO_BROADCAST`: permit sending broadcast datagrams on a datagram socket.
/// replaces libc::SO_BROADCAST
pub const SO_BROADCAST: c_int = sol::SO_BROADCAST;
/// `SO_SNDBUF`.
/// replaces libc::SO_SNDBUF
pub const SO_SNDBUF: c_int = sol::SO_SNDBUF;
/// `SO_RCVBUF`.
/// replaces libc::SO_RCVBUF
pub const SO_RCVBUF: c_int = sol::SO_RCVBUF;
/// `SO_KEEPALIVE`.
/// replaces libc::SO_KEEPALIVE
pub const SO_KEEPALIVE: c_int = sol::SO_KEEPALIVE;
/// `SO_LINGER`: a [`linger`] in seconds on Linux, in clock ticks on macOS (use
/// [`SO_LINGER_SEC`] there).
/// replaces libc::SO_LINGER
pub const SO_LINGER: c_int = sol::SO_LINGER;
/// `SO_REUSEPORT`.
/// replaces libc::SO_REUSEPORT
pub const SO_REUSEPORT: c_int = sol::SO_REUSEPORT;
/// `SO_ACCEPTCONN`: whether the socket listens.
/// replaces libc::SO_ACCEPTCONN
pub const SO_ACCEPTCONN: c_int = sol::SO_ACCEPTCONN;
/// `SO_NOSIGPIPE` (macOS): a write to a closed peer is `EPIPE` and no `SIGPIPE`.
/// replaces libc::SO_NOSIGPIPE
#[cfg(target_os = "macos")]
pub const SO_NOSIGPIPE: c_int = sol::SO_NOSIGPIPE;
/// `SO_LINGER_SEC` (macOS): `SO_LINGER` in seconds.
/// replaces libc::SO_LINGER_SEC
#[cfg(target_os = "macos")]
pub const SO_LINGER_SEC: c_int = sol::SO_LINGER_SEC;

/// `TCP_NODELAY` (netinet/tcp.h).
/// replaces libc::TCP_NODELAY
pub const TCP_NODELAY: c_int = 1;
/// `TCP_KEEPIDLE` (netinet/tcp.h, Linux): idle seconds before the first probe (macOS:
/// [`TCP_KEEPALIVE`]).
/// replaces libc::TCP_KEEPIDLE
#[cfg(target_os = "linux")]
pub const TCP_KEEPIDLE: c_int = 4;
/// `TCP_KEEPALIVE` (netinet/tcp.h, macOS): Linux's `TCP_KEEPIDLE`.
/// replaces libc::TCP_KEEPALIVE
#[cfg(target_os = "macos")]
pub const TCP_KEEPALIVE: c_int = 0x10;
/// `TCP_KEEPINTVL` (netinet/tcp.h).
/// replaces libc::TCP_KEEPINTVL
#[cfg(target_os = "linux")]
pub const TCP_KEEPINTVL: c_int = 5;
/// `TCP_KEEPINTVL` (netinet/tcp.h).
/// replaces libc::TCP_KEEPINTVL
#[cfg(target_os = "macos")]
pub const TCP_KEEPINTVL: c_int = 0x101;
/// `TCP_KEEPCNT` (netinet/tcp.h).
/// replaces libc::TCP_KEEPCNT
#[cfg(target_os = "linux")]
pub const TCP_KEEPCNT: c_int = 6;
/// `TCP_KEEPCNT` (netinet/tcp.h).
/// replaces libc::TCP_KEEPCNT
#[cfg(target_os = "macos")]
pub const TCP_KEEPCNT: c_int = 0x102;
/// `TCP_QUICKACK` (netinet/tcp.h, Linux).
/// replaces libc::TCP_QUICKACK
#[cfg(target_os = "linux")]
pub const TCP_QUICKACK: c_int = 12;
/// `TCP_CONGESTION` (netinet/tcp.h, Linux): the congestion control's name, bytes.
/// replaces libc::TCP_CONGESTION
#[cfg(target_os = "linux")]
pub const TCP_CONGESTION: c_int = 13;
/// `TCP_FASTOPEN` (netinet/tcp.h).
/// replaces libc::TCP_FASTOPEN
#[cfg(target_os = "linux")]
pub const TCP_FASTOPEN: c_int = 23;
/// `TCP_FASTOPEN` (netinet/tcp.h).
/// replaces libc::TCP_FASTOPEN
#[cfg(target_os = "macos")]
pub const TCP_FASTOPEN: c_int = 0x105;
/// `TCP_NOTSENT_LOWAT` (netinet/tcp.h).
/// replaces libc::TCP_NOTSENT_LOWAT
#[cfg(target_os = "linux")]
pub const TCP_NOTSENT_LOWAT: c_int = 25;
/// `TCP_NOTSENT_LOWAT` (netinet/tcp.h, macOS 10.11; the `libc` crate lacks it, which is why
/// blaze-core declared `MACOS_TCP_NOTSENT_LOWAT`).
/// replaces blaze_core::sockopt::MACOS_TCP_NOTSENT_LOWAT
#[cfg(target_os = "macos")]
pub const TCP_NOTSENT_LOWAT: c_int = 0x201;
/// `TCP_FASTOPEN_CONNECT` (netinet/tcp.h, Linux 4.11).
/// replaces libc::TCP_FASTOPEN_CONNECT
#[cfg(target_os = "linux")]
pub const TCP_FASTOPEN_CONNECT: c_int = 30;

/// `IP_TOS` (netinet/in.h).
/// replaces libc::IP_TOS
#[cfg(target_os = "linux")]
pub const IP_TOS: c_int = 1;
/// `IP_TOS` (netinet/in.h).
/// replaces libc::IP_TOS
#[cfg(target_os = "macos")]
pub const IP_TOS: c_int = 3;
/// `IP_RECVDSTADDR` (netinet/in.h, macOS): a datagram's destination address as an
/// [`in_addr`] control message.
/// replaces libc::IP_RECVDSTADDR
#[cfg(target_os = "macos")]
pub const IP_RECVDSTADDR: c_int = 7;
/// `IP_PKTINFO` (netinet/in.h): an [`in_pktinfo`] control message.
/// replaces libc::IP_PKTINFO
#[cfg(target_os = "linux")]
pub const IP_PKTINFO: c_int = 8;
/// `IP_PKTINFO` (netinet/in.h): an [`in_pktinfo`] control message.
/// replaces libc::IP_PKTINFO
#[cfg(target_os = "macos")]
pub const IP_PKTINFO: c_int = 26;
/// `IP_MTU_DISCOVER` (bits/in.h, Linux): the path MTU discovery mode, an `IP_PMTUDISC_*`.
/// replaces libc::IP_MTU_DISCOVER
#[cfg(target_os = "linux")]
pub const IP_MTU_DISCOVER: c_int = 10;
/// `IP_RECVTOS` (netinet/in.h): each datagram's TOS byte as a control message.
/// replaces libc::IP_RECVTOS
#[cfg(target_os = "linux")]
pub const IP_RECVTOS: c_int = 13;
/// `IP_RECVTOS` (netinet/in.h): each datagram's TOS byte as a control message.
/// replaces libc::IP_RECVTOS
#[cfg(target_os = "macos")]
pub const IP_RECVTOS: c_int = 27;
/// `IP_DONTFRAG` (netinet/in.h, macOS): the don't-fragment bit.
/// replaces libc::IP_DONTFRAG
#[cfg(target_os = "macos")]
pub const IP_DONTFRAG: c_int = 28;
/// `IP_MULTICAST_TTL` (netinet/in.h): hop limit for outgoing IPv4 multicast datagrams.
/// replaces libc::IP_MULTICAST_TTL
#[cfg(target_os = "linux")]
pub const IP_MULTICAST_TTL: c_int = 33;
/// `IP_MULTICAST_TTL` (netinet/in.h): hop limit for outgoing IPv4 multicast datagrams.
/// replaces libc::IP_MULTICAST_TTL
#[cfg(target_os = "macos")]
pub const IP_MULTICAST_TTL: c_int = 10;
/// `IP_MULTICAST_LOOP` (netinet/in.h): whether outgoing IPv4 multicast datagrams loop back to local sockets.
/// replaces libc::IP_MULTICAST_LOOP
#[cfg(target_os = "linux")]
pub const IP_MULTICAST_LOOP: c_int = 34;
/// `IP_MULTICAST_LOOP` (netinet/in.h): whether outgoing IPv4 multicast datagrams loop back to local sockets.
/// replaces libc::IP_MULTICAST_LOOP
#[cfg(target_os = "macos")]
pub const IP_MULTICAST_LOOP: c_int = 11;
/// `IP_ADD_MEMBERSHIP` (netinet/in.h): join an IPv4 multicast group ([`ip_mreq`]).
/// replaces libc::IP_ADD_MEMBERSHIP
#[cfg(target_os = "linux")]
pub const IP_ADD_MEMBERSHIP: c_int = 35;
/// `IP_ADD_MEMBERSHIP` (netinet/in.h): join an IPv4 multicast group ([`ip_mreq`]).
/// replaces libc::IP_ADD_MEMBERSHIP
#[cfg(target_os = "macos")]
pub const IP_ADD_MEMBERSHIP: c_int = 12;

/// `IP_PMTUDISC_DONT` (bits/in.h, Linux).
/// replaces libc::IP_PMTUDISC_DONT
#[cfg(target_os = "linux")]
pub const IP_PMTUDISC_DONT: c_int = 0;
/// `IP_PMTUDISC_WANT` (bits/in.h, Linux).
/// replaces libc::IP_PMTUDISC_WANT
#[cfg(target_os = "linux")]
pub const IP_PMTUDISC_WANT: c_int = 1;
/// `IP_PMTUDISC_DO` (bits/in.h, Linux).
/// replaces libc::IP_PMTUDISC_DO
#[cfg(target_os = "linux")]
pub const IP_PMTUDISC_DO: c_int = 2;
/// `IP_PMTUDISC_PROBE` (bits/in.h, Linux).
/// replaces libc::IP_PMTUDISC_PROBE
#[cfg(target_os = "linux")]
pub const IP_PMTUDISC_PROBE: c_int = 3;
/// `IP_PMTUDISC_INTERFACE` (bits/in.h, Linux).
/// replaces libc::IP_PMTUDISC_INTERFACE
#[cfg(target_os = "linux")]
pub const IP_PMTUDISC_INTERFACE: c_int = 4;
/// `IP_PMTUDISC_OMIT` (bits/in.h, Linux).
/// replaces libc::IP_PMTUDISC_OMIT
#[cfg(target_os = "linux")]
pub const IP_PMTUDISC_OMIT: c_int = 5;

/// `IPV6_MTU_DISCOVER` (bits/in.h, Linux).
/// replaces libc::IPV6_MTU_DISCOVER
#[cfg(target_os = "linux")]
pub const IPV6_MTU_DISCOVER: c_int = 23;
/// `IPV6_PMTUDISC_PROBE` (bits/in.h, Linux).
/// replaces libc::IPV6_PMTUDISC_PROBE
#[cfg(target_os = "linux")]
pub const IPV6_PMTUDISC_PROBE: c_int = 3;
/// `IPV6_V6ONLY` (bits/in.h, Linux): an `int` option of `IPPROTO_IPV6`; an IPv6 socket that
/// takes no IPv4-mapped peers ([`setsockopt_int`]).
/// replaces libc::IPV6_V6ONLY (blaze_sys::posix::net::IPV6_V6ONLY with setsockopt_int; both systems, each its own number)
#[cfg(target_os = "linux")]
pub const IPV6_V6ONLY: c_int = 26;
/// `IPV6_V6ONLY` (netinet6/in6.h, macOS).
/// replaces libc::IPV6_V6ONLY (blaze_sys::posix::net::IPV6_V6ONLY with setsockopt_int; both systems, each its own number)
#[cfg(target_os = "macos")]
pub const IPV6_V6ONLY: c_int = 27;
/// `IPV6_TCLASS` (netinet/in.h): the traffic class, IPv6's TOS byte.
/// replaces libc::IPV6_TCLASS
#[cfg(target_os = "linux")]
pub const IPV6_TCLASS: c_int = 67;
/// `IPV6_TCLASS` (netinet6/in6.h).
/// replaces libc::IPV6_TCLASS
#[cfg(target_os = "macos")]
pub const IPV6_TCLASS: c_int = 36;
/// `IPV6_RECVTCLASS` (netinet/in.h).
/// replaces libc::IPV6_RECVTCLASS
#[cfg(target_os = "linux")]
pub const IPV6_RECVTCLASS: c_int = 66;
/// `IPV6_RECVTCLASS` (netinet6/in6.h).
/// replaces libc::IPV6_RECVTCLASS
#[cfg(target_os = "macos")]
pub const IPV6_RECVTCLASS: c_int = 35;
/// `IPV6_PKTINFO` (netinet/in.h): an [`in6_pktinfo`] control message.
/// replaces libc::IPV6_PKTINFO
#[cfg(target_os = "linux")]
pub const IPV6_PKTINFO: c_int = 50;
/// `IPV6_PKTINFO` (netinet6/in6.h): an [`in6_pktinfo`] control message.
/// replaces libc::IPV6_PKTINFO
#[cfg(target_os = "macos")]
pub const IPV6_PKTINFO: c_int = 46;
/// `IPV6_RECVPKTINFO` (netinet/in.h).
/// replaces libc::IPV6_RECVPKTINFO
#[cfg(target_os = "linux")]
pub const IPV6_RECVPKTINFO: c_int = 49;
/// `IPV6_RECVPKTINFO` (netinet6/in6.h).
/// replaces libc::IPV6_RECVPKTINFO
#[cfg(target_os = "macos")]
pub const IPV6_RECVPKTINFO: c_int = 61;
/// `IPV6_DONTFRAG` (netinet/in.h; netinet6/in6.h): the same number on both.
/// replaces libc::IPV6_DONTFRAG
pub const IPV6_DONTFRAG: c_int = 62;

/// `UDP_SEGMENT` (netinet/udp.h, Linux 4.18): GSO's segment size, a `u16` control message.
/// replaces libc::UDP_SEGMENT
#[cfg(target_os = "linux")]
pub const UDP_SEGMENT: c_int = 103;
/// `UDP_GRO` (netinet/udp.h, Linux 5.0): coalesced receives; the segment size as an `int`
/// control message.
/// replaces libc::UDP_GRO
#[cfg(target_os = "linux")]
pub const UDP_GRO: c_int = 104;

/// `MSG_TRUNC` (sys/socket.h): a datagram was longer than its room (in `msg_flags`).
/// replaces libc::MSG_TRUNC
#[cfg(target_os = "linux")]
pub const MSG_TRUNC: c_int = 0x20;
/// `MSG_TRUNC` (sys/socket.h): a datagram was longer than its room (in `msg_flags`).
/// replaces libc::MSG_TRUNC
#[cfg(target_os = "macos")]
pub const MSG_TRUNC: c_int = 0x10;
/// `MSG_NOSIGNAL` (sys/socket.h): a send to a closed peer is `EPIPE` and no `SIGPIPE`.
/// `MsgFlags::NO_SIGNAL` in the typed calls.
/// replaces libc::MSG_NOSIGNAL
#[cfg(target_os = "linux")]
pub const MSG_NOSIGNAL: c_int = 0x4000;
/// `MSG_NOSIGNAL` (sys/socket.h, macOS 10.14): as Linux's.
/// replaces libc::MSG_NOSIGNAL
#[cfg(target_os = "macos")]
pub const MSG_NOSIGNAL: c_int = 0x80000;

/// `MSG_CTRUNC` (sys/socket.h): the control messages were longer than their room (in
/// `msg_flags`).
/// replaces libc::MSG_CTRUNC
#[cfg(target_os = "linux")]
pub const MSG_CTRUNC: c_int = 0x8;
/// `MSG_CTRUNC` (sys/socket.h): the control messages were longer than their room (in
/// `msg_flags`).
/// replaces libc::MSG_CTRUNC
#[cfg(target_os = "macos")]
pub const MSG_CTRUNC: c_int = 0x20;
/// `MSG_CMSG_CLOEXEC` (sys/socket.h, Linux): descriptors received by `SCM_RIGHTS` are
/// close-on-exec. `MsgFlags::CMSG_CLOSE_ON_EXEC` in the typed calls.
/// replaces libc::MSG_CMSG_CLOEXEC
#[cfg(target_os = "linux")]
pub const MSG_CMSG_CLOEXEC: c_int = 0x4000_0000;
/// `SCM_RIGHTS` (sys/socket.h): a control message of descriptors (`int`s), at level
/// `SOL_SOCKET`.
/// replaces libc::SCM_RIGHTS
pub const SCM_RIGHTS: c_int = 0x01;
/// `SO_PEERCRED` (asm-generic/socket.h, Linux): a Unix socket's peer as a [`ucred`]
/// ([`peer_cred`]).
/// replaces libc::SO_PEERCRED
#[cfg(target_os = "linux")]
pub const SO_PEERCRED: c_int = 17;
/// `SO_RCVTIMEO` (asm-generic/socket.h: `SO_RCVTIMEO_OLD`, the 64-bit `time_t` one on both
/// 64-bit targets): a receive's timeout, a `struct timeval`.
/// replaces libc::SO_RCVTIMEO
#[cfg(target_os = "linux")]
pub const SO_RCVTIMEO: c_int = 20;
/// `SO_RCVTIMEO` (sys/socket.h): a receive's timeout, a `struct timeval`.
/// replaces libc::SO_RCVTIMEO
#[cfg(target_os = "macos")]
pub const SO_RCVTIMEO: c_int = 0x1006;
/// `SO_SNDTIMEO` (asm-generic/socket.h: `SO_SNDTIMEO_OLD`): a send's timeout, a
/// `struct timeval`.
/// replaces libc::SO_SNDTIMEO
#[cfg(target_os = "linux")]
pub const SO_SNDTIMEO: c_int = 21;
/// `SO_SNDTIMEO` (sys/socket.h): a send's timeout, a `struct timeval`.
/// replaces libc::SO_SNDTIMEO
#[cfg(target_os = "macos")]
pub const SO_SNDTIMEO: c_int = 0x1005;
/// `LOCAL_PEERTOKEN` (sys/un.h, macOS): the peer's audit token, eight `u32`s
/// ([`peer_token`]).
/// replaces libc::LOCAL_PEERTOKEN
#[cfg(target_os = "macos")]
pub const LOCAL_PEERTOKEN: c_int = 0x006;

/// `struct ucred` (sys/socket.h, Linux, `_GNU_SOURCE`): a Unix socket peer's process, user
/// and group.
/// replaces libc::ucred
#[cfg(target_os = "linux")]
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
#[repr(C)]
pub struct ucred {
    /// The process.
    pub pid: super::types::pid_t,
    /// Its user.
    pub uid: super::types::uid_t,
    /// Its group.
    pub gid: super::types::gid_t,
}

// sys/socket.h (bits/socket.h), Linux aarch64 and x86-64 alike.
#[cfg(target_os = "linux")]
const _: () = {
    assert!(size_of::<ucred>() == 12 && align_of::<ucred>() == 4);
    assert!(core::mem::offset_of!(ucred, uid) == 4);
    assert!(core::mem::offset_of!(ucred, gid) == 8);
};

/// `send`'s and `recv`'s flags: Linux's typed set.
#[cfg(target_os = "linux")]
pub use crate::linux::MsgFlags;

#[cfg(target_os = "macos")]
flag_set! {
    /// `send`'s and `recv`'s flags (sys/socket.h, macOS).
    MsgFlags: u32 {
        /// `MSG_PEEK`.
        PEEK = 0x2;
        /// `MSG_WAITALL`.
        WAIT_ALL = 0x40;
        /// `MSG_DONTWAIT`.
        DONT_WAIT = 0x80;
        /// `MSG_NOSIGNAL`: a send to a closed peer is `EPIPE` and no `SIGPIPE`.
        NO_SIGNAL = 0x80000;
    }
}

// ---------------------------------------------------------------------------------------------
// Classic BPF and seccomp (Linux): the opcodes and actions a filter is written in. The
// instruction is `crate::linux::SockFilter` and the call `crate::linux::seccomp_filter`.
// ---------------------------------------------------------------------------------------------

/// `BPF_LD` (linux/bpf_common.h): the load class. A `u16`, the type of
/// `crate::linux::SockFilter::code`, where libc has `u32`.
/// replaces libc::BPF_LD
#[cfg(target_os = "linux")]
pub const BPF_LD: u16 = 0x00;
/// `BPF_JMP` (linux/bpf_common.h): the jump class.
/// replaces libc::BPF_JMP
#[cfg(target_os = "linux")]
pub const BPF_JMP: u16 = 0x05;
/// `BPF_RET` (linux/bpf_common.h): the return class.
/// replaces libc::BPF_RET
#[cfg(target_os = "linux")]
pub const BPF_RET: u16 = 0x06;
/// `BPF_W` (linux/bpf_common.h): a 32-bit word.
/// replaces libc::BPF_W
#[cfg(target_os = "linux")]
pub const BPF_W: u16 = 0x00;
/// `BPF_ABS` (linux/bpf_common.h): at an absolute offset (of `struct seccomp_data`).
/// replaces libc::BPF_ABS
#[cfg(target_os = "linux")]
pub const BPF_ABS: u16 = 0x20;
/// `BPF_JEQ` (linux/bpf_common.h): jump if equal.
/// replaces libc::BPF_JEQ
#[cfg(target_os = "linux")]
pub const BPF_JEQ: u16 = 0x10;
/// `BPF_K` (linux/bpf_common.h): the operand is the instruction's `k`.
/// replaces libc::BPF_K
#[cfg(target_os = "linux")]
pub const BPF_K: u16 = 0x00;
/// `SECCOMP_RET_ERRNO` (linux/seccomp.h): fail the call with the errno in the low 16 bits.
/// replaces libc::SECCOMP_RET_ERRNO
#[cfg(target_os = "linux")]
pub const SECCOMP_RET_ERRNO: u32 = 0x0005_0000;
/// `SECCOMP_RET_ALLOW` (linux/seccomp.h): make the call.
/// replaces libc::SECCOMP_RET_ALLOW
#[cfg(target_os = "linux")]
pub const SECCOMP_RET_ALLOW: u32 = 0x7fff_0000;

// ---------------------------------------------------------------------------------------------
// Layouts, checked at compile time.
// ---------------------------------------------------------------------------------------------

// Linux, aarch64 and x86-64 alike (LP64): netinet/in.h, sys/socket.h, sys/un.h,
// bits/socket.h (glibc; musl's 64-bit layouts are the same sizes and offsets).
#[cfg(target_os = "linux")]
const _: () = {
    use core::mem::offset_of;
    assert!(size_of::<in_addr>() == 4 && align_of::<in_addr>() == 4);
    assert!(size_of::<in6_addr>() == 16 && align_of::<in6_addr>() == 4);
    assert!(size_of::<sockaddr>() == 16 && align_of::<sockaddr>() == 2);
    assert!(size_of::<sockaddr_storage>() == 128 && align_of::<sockaddr_storage>() == 8);
    assert!(offset_of!(sockaddr_storage, ss_family) == 0);
    assert!(size_of::<sockaddr_in>() == 16 && align_of::<sockaddr_in>() == 4);
    assert!(offset_of!(sockaddr_in, sin_family) == 0);
    assert!(offset_of!(sockaddr_in, sin_port) == 2);
    assert!(offset_of!(sockaddr_in, sin_addr) == 4);
    assert!(size_of::<sockaddr_in6>() == 28 && align_of::<sockaddr_in6>() == 4);
    assert!(offset_of!(sockaddr_in6, sin6_family) == 0);
    assert!(offset_of!(sockaddr_in6, sin6_port) == 2);
    assert!(offset_of!(sockaddr_in6, sin6_flowinfo) == 4);
    assert!(offset_of!(sockaddr_in6, sin6_addr) == 8);
    assert!(offset_of!(sockaddr_in6, sin6_scope_id) == 24);
    assert!(size_of::<sockaddr_un>() == 110 && align_of::<sockaddr_un>() == 2);
    assert!(offset_of!(sockaddr_un, sun_path) == 2);
    assert!(size_of::<in_pktinfo>() == 12 && align_of::<in_pktinfo>() == 4);
    assert!(offset_of!(in_pktinfo, ipi_spec_dst) == 4);
    assert!(offset_of!(in_pktinfo, ipi_addr) == 8);
    assert!(size_of::<in6_pktinfo>() == 20 && align_of::<in6_pktinfo>() == 4);
    assert!(offset_of!(in6_pktinfo, ipi6_ifindex) == 16);
    assert!(size_of::<linger>() == 8 && align_of::<linger>() == 4);
    assert!(offset_of!(linger, l_linger) == 4);
    assert!(size_of::<msghdr>() == 56 && align_of::<msghdr>() == 8);
    assert!(offset_of!(msghdr, msg_namelen) == 8);
    assert!(offset_of!(msghdr, msg_iov) == 16);
    assert!(offset_of!(msghdr, msg_iovlen) == 24);
    assert!(offset_of!(msghdr, msg_control) == 32);
    assert!(offset_of!(msghdr, msg_controllen) == 40);
    assert!(offset_of!(msghdr, msg_flags) == 48);
    assert!(size_of::<mmsghdr>() == 64 && align_of::<mmsghdr>() == 8);
    assert!(offset_of!(mmsghdr, msg_len) == 56);
    assert!(size_of::<cmsghdr>() == 16 && align_of::<cmsghdr>() == 8);
    assert!(offset_of!(cmsghdr, cmsg_level) == 8);
    assert!(offset_of!(cmsghdr, cmsg_type) == 12);
    assert!(CMSG_HEADER == 16);
};

// macOS arm64: netinet/in.h, netinet6/in6.h, sys/socket.h, sys/un.h (the BSD layouts with a
// length byte before a one-byte family).
#[cfg(target_os = "macos")]
const _: () = {
    use core::mem::offset_of;
    assert!(size_of::<in_addr>() == 4 && align_of::<in_addr>() == 4);
    assert!(size_of::<in6_addr>() == 16 && align_of::<in6_addr>() == 4);
    assert!(size_of::<sockaddr>() == 16 && align_of::<sockaddr>() == 1);
    assert!(size_of::<sockaddr_storage>() == 128 && align_of::<sockaddr_storage>() == 8);
    assert!(offset_of!(sockaddr_storage, ss_len) == 0);
    assert!(offset_of!(sockaddr_storage, ss_family) == 1);
    assert!(size_of::<sockaddr_in>() == 16 && align_of::<sockaddr_in>() == 4);
    assert!(offset_of!(sockaddr_in, sin_family) == 1);
    assert!(offset_of!(sockaddr_in, sin_port) == 2);
    assert!(offset_of!(sockaddr_in, sin_addr) == 4);
    assert!(size_of::<sockaddr_in6>() == 28 && align_of::<sockaddr_in6>() == 4);
    assert!(offset_of!(sockaddr_in6, sin6_family) == 1);
    assert!(offset_of!(sockaddr_in6, sin6_port) == 2);
    assert!(offset_of!(sockaddr_in6, sin6_flowinfo) == 4);
    assert!(offset_of!(sockaddr_in6, sin6_addr) == 8);
    assert!(offset_of!(sockaddr_in6, sin6_scope_id) == 24);
    assert!(size_of::<sockaddr_un>() == 106 && align_of::<sockaddr_un>() == 1);
    assert!(offset_of!(sockaddr_un, sun_family) == 1);
    assert!(offset_of!(sockaddr_un, sun_path) == 2);
    assert!(size_of::<in_pktinfo>() == 12 && align_of::<in_pktinfo>() == 4);
    assert!(offset_of!(in_pktinfo, ipi_spec_dst) == 4);
    assert!(offset_of!(in_pktinfo, ipi_addr) == 8);
    assert!(size_of::<in6_pktinfo>() == 20 && align_of::<in6_pktinfo>() == 4);
    assert!(offset_of!(in6_pktinfo, ipi6_ifindex) == 16);
    assert!(size_of::<linger>() == 8 && align_of::<linger>() == 4);
    assert!(offset_of!(linger, l_linger) == 4);
    assert!(size_of::<msghdr>() == 48 && align_of::<msghdr>() == 8);
    assert!(offset_of!(msghdr, msg_namelen) == 8);
    assert!(offset_of!(msghdr, msg_iov) == 16);
    assert!(offset_of!(msghdr, msg_iovlen) == 24);
    assert!(offset_of!(msghdr, msg_control) == 32);
    assert!(offset_of!(msghdr, msg_controllen) == 40);
    assert!(offset_of!(msghdr, msg_flags) == 44);
    assert!(size_of::<cmsghdr>() == 12 && align_of::<cmsghdr>() == 4);
    assert!(offset_of!(cmsghdr, cmsg_level) == 4);
    assert!(offset_of!(cmsghdr, cmsg_type) == 8);
    assert!(CMSG_HEADER == 12);
};

// ---------------------------------------------------------------------------------------------
// Typed calls.
// ---------------------------------------------------------------------------------------------

/// A C call's `int` result: the value, or the calling thread's `errno` for a negative one.
/// macOS only: on Linux the kernel's result carries its error.
#[cfg(target_os = "macos")]
#[blaze::noalloc]
#[blaze::nopanic]
fn said(result: c_int) -> Result<c_int, Errno> {
    if result < 0 { Err(super::signal::errno()) } else { Ok(result) }
}

/// A C call's `ssize_t` result: the count, or `errno` for a negative one.
#[cfg(target_os = "macos")]
#[blaze::noalloc]
#[blaze::nopanic]
fn said_len(result: isize) -> Result<usize, Errno> {
    match usize::try_from(result) {
        Ok(len) => Ok(len),
        Err(_) => Err(super::signal::errno()),
    }
}

/// `len` checked to fit a [`sockaddr_storage`]: [`Errno::Inval`] past it.
#[blaze::noalloc]
#[blaze::nopanic]
const fn within_storage(len: socklen_t) -> Result<socklen_t, Errno> {
    if len as usize > size_of::<sockaddr_storage>() { Err(Errno::Inval) } else { Ok(len) }
}

/// `socket`: a new socket of `domain` (`AF_*`) and `kind` (`SOCK_*`, on Linux with
/// `SOCK_CLOEXEC` or `SOCK_NONBLOCK` or'd in), of `protocol` (0: the family's default). The
/// caller owns the descriptor and closes it. On Linux, `crate::linux::socket`: a protocol
/// other than 0 is [`Errno::ProtoNoSupport`], a family other than `AF_UNIX`, `AF_INET` and
/// `AF_INET6` [`Errno::AfNoSupport`], before any call.
/// replaces libc::socket
#[blaze::noalloc]
#[blaze::nopanic]
pub fn socket(domain: c_int, kind: c_int, protocol: c_int) -> Result<Fd, Errno> {
    #[cfg(target_os = "linux")]
    {
        use crate::linux::{Family, SockFlags, SockKind};
        /// `SOCK_TYPE_MASK` (linux/net.h).
        const TYPE_MASK: c_int = 0xf;
        if protocol != 0 {
            return Err(Errno::ProtoNoSupport);
        }
        let family = match domain {
            AF_UNIX => Family::Unix,
            AF_INET => Family::V4,
            AF_INET6 => Family::V6,
            _ => return Err(Errno::AfNoSupport),
        };
        let base = match kind & TYPE_MASK {
            SOCK_STREAM => SockKind::Stream,
            SOCK_DGRAM => SockKind::Datagram,
            5 => SockKind::SeqPacket,
            _ => return Err(Errno::Inval),
        };
        let flags = SockFlags::from_bits((kind & !TYPE_MASK) as u32);
        if !SockFlags::NONBLOCK.with(SockFlags::CLOSE_ON_EXEC).contains(flags) {
            return Err(Errno::Inval);
        }
        crate::linux::socket(family, base, flags).map(|owned| Fd::new(owned.into_raw()))
    }
    #[cfg(target_os = "macos")]
    {
        // SAFETY: the call takes numbers and no memory.
        said(unsafe { c::socket(domain, kind, protocol) }).map(Fd::new)
    }
}

/// `socketpair`: two connected sockets of `domain` and `kind`, of `protocol`; the caller owns
/// both and closes them. On Linux, `crate::linux::sock::socketpair`.
/// replaces libc::socketpair
#[blaze::noalloc]
#[blaze::nopanic]
pub fn socketpair(domain: c_int, kind: c_int, protocol: c_int) -> Result<[Fd; 2], Errno> {
    #[cfg(target_os = "linux")]
    {
        let [a, b] = crate::linux::sock::socketpair(domain, kind, protocol)?;
        Ok([Fd::new(a.into_raw()), Fd::new(b.into_raw())])
    }
    #[cfg(target_os = "macos")]
    {
        let mut ends: [c_int; 2] = [-1, -1];
        // SAFETY: `ends` is two live `int`s the call writes and does not keep.
        said(unsafe { c::socketpair(domain, kind, protocol, ends.as_mut_ptr()) })?;
        let [a, b] = ends;
        Ok([Fd::new(a), Fd::new(b)])
    }
}

/// The address in `at`'s first `len` bytes as Linux's typed [`crate::linux::SockAddr`]:
/// [`Errno::AfNoSupport`] for another family, [`Errno::Inval`] for a length too short for
/// its family, a flow label (the typed address has none), or a Unix path the typed address
/// refuses (empty, which an abstract name's leading NUL is).
#[cfg(target_os = "linux")]
#[blaze::noalloc]
#[blaze::nopanic]
fn linux_addr(at: &sockaddr_storage, len: socklen_t) -> Result<crate::linux::SockAddr, Errno> {
    use crate::linux::SockAddr;
    let len = within_storage(len)? as usize;
    match at.ss_family as c_int {
        AF_INET => {
            if len < size_of::<sockaddr_in>() {
                return Err(Errno::Inval);
            }
            // SAFETY: a `sockaddr_storage` is larger and more aligned than a `sockaddr_in`,
            // and every bit pattern of its plain integer fields is one.
            let v4 = unsafe { &*ptr::from_ref(at).cast::<sockaddr_in>() };
            Ok(SockAddr::v4(v4.sin_addr.s_addr.to_ne_bytes(), u16::from_be(v4.sin_port)))
        }
        AF_INET6 => {
            if len < size_of::<sockaddr_in6>() {
                return Err(Errno::Inval);
            }
            // SAFETY: as for `sockaddr_in`.
            let v6 = unsafe { &*ptr::from_ref(at).cast::<sockaddr_in6>() };
            if v6.sin6_flowinfo != 0 {
                return Err(Errno::Inval);
            }
            Ok(SockAddr::v6(v6.sin6_addr.s6_addr, u16::from_be(v6.sin6_port), v6.sin6_scope_id))
        }
        AF_UNIX => {
            // SAFETY: as for `sockaddr_in`.
            let un = unsafe { &*ptr::from_ref(at).cast::<sockaddr_un>() };
            let given = len.saturating_sub(core::mem::offset_of!(sockaddr_un, sun_path));
            let mut path = [0u8; 108];
            // The path ends at its NUL or at the length given, whichever is first.
            let mut end = 0usize;
            while let (true, Some(&byte), Some(to)) = (end < given, un.sun_path.get(end), path.get_mut(end)) {
                if byte == 0 {
                    break;
                }
                *to = byte as u8;
                // Below 108, so it cannot wrap; the wrapping form has no overflow check.
                end = end.wrapping_add(1);
            }
            match path.get(..end) {
                Some(path) => SockAddr::unix(path),
                None => Err(Errno::Inval),
            }
        }
        _ => Err(Errno::AfNoSupport),
    }
}

/// `bind`: the socket given the address in `at`'s first `len` bytes. [`Errno::Inval`] for a
/// `len` past a `sockaddr_storage`. On Linux, `crate::linux::bind` (see [`connect`] for what
/// it refuses).
/// replaces libc::bind
#[blaze::noalloc]
#[blaze::nopanic]
pub fn bind(socket: Fd, at: &sockaddr_storage, len: socklen_t) -> Result<(), Errno> {
    #[cfg(target_os = "linux")]
    {
        crate::linux::bind(socket, &linux_addr(at, len)?)
    }
    #[cfg(target_os = "macos")]
    {
        let len = within_storage(len)?;
        // SAFETY: `at` is a live `sockaddr_storage` of which the call reads `len` bytes, at
        // most all of it, and keeps nothing.
        said(unsafe { c::bind(socket.raw(), ptr::from_ref(at).cast(), len) }).map(|_| ())
    }
}

/// `connect`: the socket connected to the address in `at`'s first `len` bytes. On a
/// non-blocking socket [`Errno::InProgress`] says it is under way. On Linux,
/// `crate::linux::connect`: an address it has no typed form for (a family other than Unix,
/// IPv4 and IPv6, an abstract Unix name, an IPv6 flow label) is a refusal before the call.
/// replaces libc::connect
#[blaze::noalloc]
#[blaze::nopanic]
pub fn connect(socket: Fd, at: &sockaddr_storage, len: socklen_t) -> Result<(), Errno> {
    #[cfg(target_os = "linux")]
    {
        crate::linux::connect(socket, &linux_addr(at, len)?)
    }
    #[cfg(target_os = "macos")]
    {
        let len = within_storage(len)?;
        // SAFETY: as `bind`.
        said(unsafe { c::connect(socket.raw(), ptr::from_ref(at).cast(), len) }).map(|_| ())
    }
}

/// Opens an `AF_UNIX` `SOCK_STREAM` socket with `FD_CLOEXEC` and connects it to `path`.
///
/// Refuses empty paths, NUL bytes inside `path`, or paths that do not fit `sockaddr_un::sun_path`
/// with [`Errno::Inval`] / [`Errno::NameTooLong`] before any syscall. The returned [`super::io::OwnedFd`]
/// closes automatically on drop.
#[blaze::noalloc]
#[blaze::nopanic]
pub fn connect_unix(path: &[u8]) -> Result<super::io::OwnedFd, Errno> {
    if path.is_empty() {
        return Err(Errno::Inval);
    }
    let mut un = sockaddr_un::ZERO;
    if path.len() >= un.sun_path.len() {
        return Err(Errno::NameTooLong);
    }
    let mut i = 0usize;
    while let Some(&byte) = path.get(i) {
        if byte == 0 {
            return Err(Errno::Inval);
        }
        if let Some(slot) = un.sun_path.get_mut(i) {
            *slot = c_char::from_ne_bytes([byte]);
        }
        i = i.wrapping_add(1);
    }
    un.sun_family = AF_UNIX as sa_family_t;
    let addr_len = core::mem::offset_of!(sockaddr_un, sun_path).wrapping_add(path.len()).wrapping_add(1);
    #[cfg(target_os = "macos")]
    {
        un.sun_len = match u8::try_from(addr_len) {
            Ok(len) => len,
            Err(_) => return Err(Errno::NameTooLong),
        };
    }
    let mut storage = sockaddr_storage::ZERO;
    // SAFETY: `sockaddr_storage` is strictly larger and at least as aligned as `sockaddr_un`,
    // and both are plain-old-data C structs.
    unsafe {
        ptr::copy_nonoverlapping(ptr::from_ref(&un).cast::<u8>(), ptr::from_mut(&mut storage).cast::<u8>(), size_of::<sockaddr_un>());
    }
    #[cfg(target_os = "linux")]
    let raw = socket(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0)?;
    #[cfg(target_os = "macos")]
    let raw = socket(AF_UNIX, SOCK_STREAM, 0)?;
    // SAFETY: `socket` just returned a newly owned open file descriptor.
    let owned = unsafe { super::io::OwnedFd::from_raw(raw.raw()) };
    #[cfg(target_os = "macos")]
    super::io::set_close_on_exec(owned.fd(), true)?;
    let socklen = match socklen_t::try_from(addr_len) {
        Ok(len) => len,
        Err(_) => return Err(Errno::NameTooLong),
    };
    connect(owned.fd(), &storage, socklen)?;
    Ok(owned)
}

/// `listen`: the socket takes connections, up to `backlog` waiting ([`SOMAXCONN`] at most).
/// [`Errno::Inval`] for a negative backlog.
/// replaces libc::listen
#[blaze::noalloc]
#[blaze::nopanic]
pub fn listen(socket: Fd, backlog: c_int) -> Result<(), Errno> {
    #[cfg(target_os = "linux")]
    {
        match u32::try_from(backlog) {
            Ok(backlog) => crate::linux::listen(socket, backlog),
            Err(_) => Err(Errno::Inval),
        }
    }
    #[cfg(target_os = "macos")]
    {
        if backlog < 0 {
            return Err(Errno::Inval);
        }
        // SAFETY: the call takes numbers and no memory.
        said(unsafe { c::listen(socket.raw(), backlog) }).map(|_| ())
    }
}

/// `getsockname`: the socket's own address and its length. On Linux,
/// `crate::linux::sock::getsockname`.
/// replaces libc::getsockname
#[blaze::noalloc]
#[blaze::nopanic]
pub fn getsockname(socket: Fd) -> Result<(sockaddr_storage, socklen_t), Errno> {
    #[cfg(target_os = "linux")]
    let (at, len) = crate::linux::sock::getsockname(socket)?;
    #[cfg(target_os = "macos")]
    let (at, len) = {
        let mut at = sockaddr_storage::ZERO;
        let mut len = size_of::<sockaddr_storage>() as socklen_t;
        // SAFETY: `at` is a live `sockaddr_storage` and `len` its size; the call writes at
        // most that many bytes and the length, and keeps nothing.
        said(unsafe { c::getsockname(socket.raw(), ptr::from_mut(&mut at).cast(), &raw mut len) })?;
        (at, len)
    };
    Ok((at, len.min(size_of::<sockaddr_storage>() as socklen_t)))
}

/// `send`: `bytes` sent on the socket: how many, which may be fewer than all. On Linux,
/// `crate::linux::sendmsg` of one part.
/// replaces libc::send
#[blaze::noalloc]
#[blaze::nopanic]
pub fn send(socket: Fd, bytes: &[u8], flags: MsgFlags) -> Result<usize, Errno> {
    #[cfg(target_os = "linux")]
    {
        crate::linux::sendmsg(socket, &[crate::linux::IoSlice::new(bytes)], flags)
    }
    #[cfg(target_os = "macos")]
    {
        // SAFETY: `bytes` is live for the call, which reads at most its length and keeps
        // nothing.
        said_len(unsafe { c::send(socket.raw(), bytes.as_ptr().cast(), bytes.len(), flags.bits() as c_int) })
    }
}

/// `recv`: bytes received into `into`: how many, 0 once a stream's peer has closed. On
/// Linux, `crate::linux::recvmsg` into one part.
/// replaces libc::recv
#[blaze::noalloc]
#[blaze::nopanic]
pub fn recv(socket: Fd, into: &mut [u8], flags: MsgFlags) -> Result<usize, Errno> {
    #[cfg(target_os = "linux")]
    {
        crate::linux::recvmsg(socket, &mut [crate::linux::IoSliceMut::new(into)], flags)
    }
    #[cfg(target_os = "macos")]
    {
        // SAFETY: `into` is held exclusively for the call, which writes at most its length
        // and keeps nothing.
        said_len(unsafe { c::recv(socket.raw(), into.as_mut_ptr().cast(), into.len(), flags.bits() as c_int) })
    }
}

/// `setsockopt` of `len` bytes at `value`: on Linux `crate::linux::sock::setsockopt_raw`,
/// on macOS the C call.
///
/// # Safety
///
/// `value` is `len` live bytes.
#[blaze::noalloc]
#[blaze::nopanic]
unsafe fn set_raw(socket: Fd, level: c_int, name: c_int, value: *const u8, len: usize) -> Result<(), Errno> {
    let Ok(len) = socklen_t::try_from(len) else {
        return Err(Errno::Inval);
    };
    #[cfg(target_os = "linux")]
    {
        // SAFETY: the caller's: `value` is `len` live bytes.
        unsafe { crate::linux::sock::setsockopt_raw(socket, level, name, value, len) }
    }
    #[cfg(target_os = "macos")]
    {
        // SAFETY: the caller's: `value` is `len` live bytes; the call reads them and keeps
        // nothing.
        said(unsafe { c::setsockopt(socket.raw(), level, name, value.cast(), len) }).map(|_| ())
    }
}

/// `getsockopt` into `len` bytes at `into`: how many it wrote. On Linux
/// `crate::linux::sock::getsockopt_raw`, on macOS the C call.
///
/// # Safety
///
/// `into` is `len` live bytes held exclusively.
#[blaze::noalloc]
#[blaze::nopanic]
unsafe fn get_raw(socket: Fd, level: c_int, name: c_int, into: *mut u8, len: usize) -> Result<usize, Errno> {
    let Ok(mut got) = socklen_t::try_from(len) else {
        return Err(Errno::Inval);
    };
    #[cfg(target_os = "linux")]
    {
        // SAFETY: the caller's: `into` is `len` (`got`) live bytes held exclusively.
        got = unsafe { crate::linux::sock::getsockopt_raw(socket, level, name, into, got) }?;
    }
    #[cfg(target_os = "macos")]
    {
        // SAFETY: the caller's: `into` is `len` live bytes the call writes at most `got` of,
        // with the length; it keeps nothing.
        said(unsafe { c::getsockopt(socket.raw(), level, name, into.cast(), &raw mut got) })?;
    }
    Ok((got as usize).min(len))
}

/// `setsockopt` of an `int` option. (On Linux the options blaze's TCP sockets set have the
/// typed `crate::linux::setsockopt`; this is the one for a level and name a table holds,
/// as blaze-core's `sockopt::knob`, on both systems.)
/// replaces libc::setsockopt
#[blaze::noalloc]
#[blaze::nopanic]
pub fn setsockopt_int(socket: Fd, level: c_int, name: c_int, value: c_int) -> Result<(), Errno> {
    // SAFETY: `value` is a live `int` for the call.
    unsafe { set_raw(socket, level, name, ptr::from_ref(&value).cast(), size_of::<c_int>()) }
}

/// `setsockopt` of a [`linger`] option (`SO_LINGER`, macOS's `SO_LINGER_SEC`).
/// replaces libc::setsockopt
#[blaze::noalloc]
#[blaze::nopanic]
pub fn setsockopt_linger(socket: Fd, level: c_int, name: c_int, value: linger) -> Result<(), Errno> {
    // SAFETY: `value` is a live `struct linger` for the call.
    unsafe { set_raw(socket, level, name, ptr::from_ref(&value).cast(), size_of::<linger>()) }
}

/// `setsockopt` of an option whose value is bytes (`TCP_CONGESTION`'s name).
/// replaces libc::setsockopt
#[blaze::noalloc]
#[blaze::nopanic]
pub fn setsockopt_bytes(socket: Fd, level: c_int, name: c_int, value: &[u8]) -> Result<(), Errno> {
    // SAFETY: `value` is live for the call.
    unsafe { set_raw(socket, level, name, value.as_ptr(), value.len()) }
}

/// `getsockopt` of an `int` option. [`Errno::Inval`] when the system wrote other than an
/// `int`'s bytes.
/// replaces libc::getsockopt
#[blaze::noalloc]
#[blaze::nopanic]
pub fn getsockopt_int(socket: Fd, level: c_int, name: c_int) -> Result<c_int, Errno> {
    let mut value: c_int = 0;
    // SAFETY: `value` is a live `int` held for the call.
    let got = unsafe { get_raw(socket, level, name, ptr::from_mut(&mut value).cast(), size_of::<c_int>()) }?;
    if got != size_of::<c_int>() {
        return Err(Errno::Inval);
    }
    Ok(value)
}

/// `getsockopt` of a [`linger`] option. [`Errno::Inval`] when the system wrote other than a
/// `struct linger`.
/// replaces libc::getsockopt
#[blaze::noalloc]
#[blaze::nopanic]
pub fn getsockopt_linger(socket: Fd, level: c_int, name: c_int) -> Result<linger, Errno> {
    let mut value = linger::default();
    // SAFETY: `value` is a live `struct linger` held for the call.
    let got = unsafe { get_raw(socket, level, name, ptr::from_mut(&mut value).cast(), size_of::<linger>()) }?;
    if got != size_of::<linger>() {
        return Err(Errno::Inval);
    }
    Ok(value)
}

/// `getsockopt` of an option whose value is bytes, into `into`: how many were written.
/// replaces libc::getsockopt
#[blaze::noalloc]
#[blaze::nopanic]
pub fn getsockopt_bytes(socket: Fd, level: c_int, name: c_int, into: &mut [u8]) -> Result<usize, Errno> {
    // SAFETY: `into` is held exclusively for the call.
    unsafe { get_raw(socket, level, name, into.as_mut_ptr(), into.len()) }
}

/// `getsockopt(SOL_SOCKET, SO_ERROR)`: the socket's pending error, cleared by the read
/// (`None` when there was none). What a non-blocking `connect` ended in.
/// replaces libc::getsockopt (SO_ERROR)
#[blaze::noalloc]
#[blaze::nopanic]
pub fn pending_error(socket: Fd) -> Result<Option<Errno>, Errno> {
    match getsockopt_int(socket, SOL_SOCKET, SO_ERROR)? {
        0 => Ok(None),
        code => match u16::try_from(code) {
            Ok(code) => Ok(Some(Errno::from_raw(code))),
            Err(_) => Err(Errno::Inval),
        },
    }
}

/// `getsockopt(SOL_LOCAL, LOCAL_PEERPID)` (macOS): the process id of a connected Unix
/// socket's peer, as it was when it connected.
/// replaces libc::getsockopt (LOCAL_PEERPID)
#[cfg(target_os = "macos")]
#[blaze::noalloc]
#[blaze::nopanic]
pub fn peer_pid(socket: Fd) -> Result<pid_t, Errno> {
    getsockopt_int(socket, SOL_LOCAL, LOCAL_PEERPID).map(pid_t::from)
}

/// `setsockopt` of a value of the option's own C type (`struct timeval` for `SO_RCVTIMEO`
/// and `SO_SNDTIMEO`, ...).
/// replaces libc::setsockopt (an option of a struct type)
///
/// # Safety
///
/// `T` is the C type the option takes, `#[repr(C)]` and with no padding the system would
/// read uninitialised.
#[blaze::noalloc]
#[blaze::nopanic]
pub unsafe fn setsockopt_value<T: Copy>(socket: Fd, level: c_int, name: c_int, value: &T) -> Result<(), Errno> {
    // SAFETY: `value` is a live `T` for the call; the caller's for its type.
    unsafe { set_raw(socket, level, name, ptr::from_ref(value).cast(), size_of::<T>()) }
}

/// `getsockopt` of a value of the option's own C type. [`Errno::Inval`] when the system
/// wrote other than a whole `T`.
/// replaces libc::getsockopt (an option of a struct type)
///
/// # Safety
///
/// `T` is the C type the option gives, `#[repr(C)]`, and every bit pattern is one.
#[blaze::noalloc]
#[blaze::nopanic]
pub unsafe fn getsockopt_value<T: Copy>(socket: Fd, level: c_int, name: c_int, into: &mut T) -> Result<(), Errno> {
    // SAFETY: `into` is a live `T` held for the call; the caller's for its type.
    let got = unsafe { get_raw(socket, level, name, ptr::from_mut(into).cast(), size_of::<T>()) }?;
    if got != size_of::<T>() {
        return Err(Errno::Inval);
    }
    Ok(())
}

/// `getsockopt(SOL_SOCKET, SO_PEERCRED)` (Linux): a connected Unix socket's peer, as it was
/// when it connected.
/// replaces libc::getsockopt (SO_PEERCRED)
#[cfg(target_os = "linux")]
#[blaze::noalloc]
#[blaze::nopanic]
pub fn peer_cred(socket: Fd) -> Result<ucred, Errno> {
    let mut cred = ucred::default();
    // SAFETY: `SO_PEERCRED` gives a `struct ucred`: three plain integers, no padding.
    unsafe { getsockopt_value(socket, SOL_SOCKET, SO_PEERCRED, &mut cred) }?;
    Ok(cred)
}

/// `getpeereid` (macOS): a connected Unix socket's peer's effective user and group.
/// replaces libc::getpeereid
#[cfg(target_os = "macos")]
#[blaze::noalloc]
#[blaze::nopanic]
pub fn peer_ids(socket: Fd) -> Result<(super::types::uid_t, super::types::gid_t), Errno> {
    let (mut uid, mut gid) = (0, 0);
    // SAFETY: `uid` and `gid` are live and writable for the call, which keeps nothing.
    said(unsafe { c::getpeereid(socket.raw(), &raw mut uid, &raw mut gid) })?;
    Ok((uid, gid))
}

/// `getsockopt(SOL_LOCAL, LOCAL_PEERTOKEN)` (macOS): a connected Unix socket's peer's audit
/// token (`audit_token_t`, eight `u32`s), as it was when it connected.
/// replaces libc::getsockopt (LOCAL_PEERTOKEN)
#[cfg(target_os = "macos")]
#[blaze::noalloc]
#[blaze::nopanic]
pub fn peer_token(socket: Fd) -> Result<[u32; 8], Errno> {
    let mut token = [0u32; 8];
    // SAFETY: `LOCAL_PEERTOKEN` gives an `audit_token_t`, `unsigned int val[8]`.
    unsafe { getsockopt_value(socket, SOL_LOCAL, LOCAL_PEERTOKEN, &mut token) }?;
    Ok(token)
}

/// `sendmsg` with the whole header: an address, parts and control messages (`SCM_RIGHTS`),
/// which Linux's typed `crate::linux::sendmsg` does not carry: the bytes sent.
/// replaces libc::sendmsg
///
/// # Safety
///
/// `message`'s pointers (`msg_name`, `msg_iov` and each part, `msg_control`) are live for
/// their lengths across the call.
#[blaze::noalloc]
#[blaze::nopanic]
pub unsafe fn sendmsg(socket: Fd, message: &msghdr, flags: MsgFlags) -> Result<usize, Errno> {
    #[cfg(target_os = "linux")]
    {
        // SAFETY: the caller's for the header's pointers.
        unsafe { crate::linux::sock::sendmsg_header(socket, message, flags) }
    }
    #[cfg(target_os = "macos")]
    {
        // SAFETY: the caller's for the header's pointers; the call reads them and keeps
        // nothing.
        said_len(unsafe { c::sendmsg(socket.raw(), ptr::from_ref(message), flags.bits() as c_int) })
    }
}

/// `recvmsg` with the whole header: the peer's address, parts and control messages, and
/// `msg_flags` (`MSG_TRUNC`, `MSG_CTRUNC`) written back: the bytes received, 0 once a
/// stream's peer has closed.
/// replaces libc::recvmsg
///
/// # Safety
///
/// `message`'s pointers (`msg_name`, `msg_iov` and each part, `msg_control`) are live and
/// held exclusively for their lengths across the call.
#[blaze::noalloc]
#[blaze::nopanic]
pub unsafe fn recvmsg(socket: Fd, message: &mut msghdr, flags: MsgFlags) -> Result<usize, Errno> {
    #[cfg(target_os = "linux")]
    {
        // SAFETY: the caller's for the header's pointers; `message` is held exclusively.
        unsafe { crate::linux::sock::recvmsg_header(socket, message, flags) }
    }
    #[cfg(target_os = "macos")]
    {
        // SAFETY: the caller's for the header's pointers; `message` is held exclusively and
        // the call writes its lengths and flags; it keeps nothing.
        said_len(unsafe { c::recvmsg(socket.raw(), ptr::from_mut(message), flags.bits() as c_int) })
    }
}

/// `sendmmsg` (Linux): the messages of `messages` sent, in order: how many were (at least
/// one), each one's `msg_len` set to its bytes. [`Errno::Inval`] for more than `c_uint`
/// holds. `crate::linux::sock::sendmmsg`.
/// replaces libc::sendmmsg
///
/// # Safety
///
/// Every header's pointers (`msg_name`, `msg_iov` and each part, `msg_control`) are live for
/// their lengths across the call.
#[cfg(target_os = "linux")]
#[blaze::noalloc]
#[blaze::nopanic]
pub unsafe fn sendmmsg(socket: Fd, messages: &mut [mmsghdr], flags: MsgFlags) -> Result<usize, Errno> {
    // SAFETY: the caller's for each header's pointers.
    unsafe { crate::linux::sock::sendmmsg(socket, messages, flags) }
}

/// `recvmmsg` (Linux): datagrams received into `messages`, in order: how many, each one's
/// `msg_len` its bytes and its `msg_flags` what was found (`MSG_TRUNC`). With `timeout`,
/// the call returns after it once a first datagram came (`recvmmsg(2)`'s caveat: it is
/// checked only after each datagram). `crate::linux::sock::recvmmsg`.
/// replaces libc::recvmmsg
///
/// # Safety
///
/// Every header's pointers (`msg_name`, `msg_iov` and each part, `msg_control`) are live and
/// held exclusively for their lengths across the call.
#[cfg(target_os = "linux")]
#[blaze::noalloc]
#[blaze::nopanic]
pub unsafe fn recvmmsg(
    socket: Fd,
    messages: &mut [mmsghdr],
    flags: MsgFlags,
    timeout: Option<&mut crate::Timespec>,
) -> Result<usize, Errno> {
    // SAFETY: the caller's for each header's pointers.
    unsafe { crate::linux::sock::recvmmsg(socket, messages, flags, timeout) }
}

/// The comparison with the `libc` crate's definitions: every type's size, alignment and
/// field offsets, every constant's value, the C declarations' signatures, and the typed
/// calls on real sockets. Deleted with the `libc` dev-dependency (D180).
#[cfg(test)]
mod tests {
    use core::ffi::c_int;
    use core::mem::offset_of;

    use super::*;

    macro_rules! same_layout {
        ($ty:ident: $($field:ident),*) => {{
            assert_eq!(size_of::<super::$ty>(), size_of::<libc::$ty>(), concat!("size of ", stringify!($ty)));
            assert_eq!(align_of::<super::$ty>(), align_of::<libc::$ty>(), concat!("align of ", stringify!($ty)));
            $(assert_eq!(
                offset_of!(super::$ty, $field),
                offset_of!(libc::$ty, $field),
                concat!(stringify!($ty), ".", stringify!($field))
            );)*
        }};
    }

    macro_rules! same_value {
        ($($name:ident),* $(,)?) => {
            $(assert_eq!(i64::from(super::$name), libc::$name as i64, stringify!($name));)*
        };
    }

    #[test]
    fn layouts_are_libc_s() {
        same_layout!(in_addr: s_addr);
        same_layout!(in6_addr: s6_addr);
        same_layout!(sockaddr: sa_family, sa_data);
        same_layout!(sockaddr_storage: ss_family);
        same_layout!(sockaddr_in: sin_family, sin_port, sin_addr, sin_zero);
        same_layout!(sockaddr_in6: sin6_family, sin6_port, sin6_flowinfo, sin6_addr, sin6_scope_id);
        same_layout!(sockaddr_un: sun_family, sun_path);
        same_layout!(in_pktinfo: ipi_ifindex, ipi_spec_dst, ipi_addr);
        same_layout!(in6_pktinfo: ipi6_addr, ipi6_ifindex);
        same_layout!(linger: l_onoff, l_linger);
        same_layout!(ip_mreq: imr_multiaddr, imr_interface);
        same_layout!(msghdr: msg_name, msg_namelen, msg_iov, msg_iovlen, msg_control, msg_controllen, msg_flags);
        same_layout!(cmsghdr: cmsg_len, cmsg_level, cmsg_type);
        #[cfg(target_os = "linux")]
        same_layout!(mmsghdr: msg_hdr, msg_len);
        #[cfg(target_os = "linux")]
        same_layout!(ucred: pid, uid, gid);
        #[cfg(target_os = "macos")]
        {
            same_layout!(sockaddr: sa_len);
            same_layout!(sockaddr_storage: ss_len);
            same_layout!(sockaddr_in: sin_len);
            same_layout!(sockaddr_in6: sin6_len);
            same_layout!(sockaddr_un: sun_len);
        }
    }

    #[test]
    fn constants_are_libc_s() {
        same_value!(
            AF_UNIX, AF_INET, AF_INET6, SOCK_STREAM, SOCK_DGRAM, SOMAXCONN, SHUT_RDWR, IPPROTO_IP,
            IPPROTO_TCP, IPPROTO_IPV6, SOL_SOCKET, SO_REUSEADDR, SO_TYPE, SO_ERROR, SO_BROADCAST,
            SO_SNDBUF, SO_RCVBUF, SO_KEEPALIVE, SO_LINGER, SO_REUSEPORT, SO_ACCEPTCONN,
            TCP_NODELAY, TCP_KEEPINTVL, TCP_KEEPCNT, TCP_FASTOPEN, IP_TOS, IP_PKTINFO, IP_RECVTOS,
            IP_MULTICAST_TTL, IP_MULTICAST_LOOP, IP_ADD_MEMBERSHIP, IPV6_TCLASS, IPV6_RECVTCLASS,
            IPV6_PKTINFO, IPV6_RECVPKTINFO, IPV6_DONTFRAG, MSG_TRUNC, MSG_NOSIGNAL, MSG_CTRUNC,
            SCM_RIGHTS, SO_RCVTIMEO, SO_SNDTIMEO,
        );
        // Lane D2's rows.
        same_value!(IPV6_V6ONLY);
        #[cfg(target_os = "linux")]
        {
            same_value!(SOCK_NONBLOCK);
            assert_eq!(crate::linux::SockFlags::NONBLOCK.bits() as c_int, libc::SOCK_NONBLOCK);
        }
        #[cfg(target_os = "linux")]
        {
            same_value!(MSG_CMSG_CLOEXEC, SO_PEERCRED);
            assert_eq!(MsgFlags::CMSG_CLOSE_ON_EXEC.bits() as c_int, libc::MSG_CMSG_CLOEXEC);
        }
        #[cfg(target_os = "macos")]
        same_value!(LOCAL_PEERTOKEN);
        #[cfg(target_os = "linux")]
        same_value!(
            SOCK_CLOEXEC, SOL_UDP, TCP_KEEPIDLE, TCP_QUICKACK, TCP_CONGESTION, TCP_NOTSENT_LOWAT,
            TCP_FASTOPEN_CONNECT, IP_MTU_DISCOVER, IP_PMTUDISC_DONT, IP_PMTUDISC_WANT, IP_PMTUDISC_DO,
            IP_PMTUDISC_PROBE, IP_PMTUDISC_INTERFACE, IP_PMTUDISC_OMIT, IPV6_MTU_DISCOVER,
            IPV6_PMTUDISC_PROBE, UDP_SEGMENT, UDP_GRO, BPF_LD, BPF_JMP, BPF_RET, BPF_W, BPF_ABS,
            BPF_JEQ, BPF_K, SECCOMP_RET_ERRNO, SECCOMP_RET_ALLOW,
        );
        #[cfg(target_os = "macos")]
        same_value!(
            SOL_LOCAL, LOCAL_PEERPID, SO_NOSIGPIPE, SO_LINGER_SEC, TCP_KEEPALIVE, IP_RECVDSTADDR,
            IP_DONTFRAG,
        );
        // The typed flags are the constants' bits.
        assert_eq!(MsgFlags::NO_SIGNAL.bits() as c_int, libc::MSG_NOSIGNAL);
        assert_eq!(MsgFlags::DONT_WAIT.bits() as c_int, libc::MSG_DONTWAIT);
        assert_eq!(MsgFlags::PEEK.bits() as c_int, libc::MSG_PEEK);
        assert_eq!(MsgFlags::WAIT_ALL.bits() as c_int, libc::MSG_WAITALL);
    }

    /// macOS's C declarations (the module `c` exists on macOS only; Linux declares none).
    #[cfg(target_os = "macos")]
    #[test]
    fn declarations_have_libc_s_signatures() {
        let _: unsafe extern "C" fn(c_int, c_int, c_int, *mut c_int) -> c_int = c::socketpair;
        let _: unsafe extern "C" fn(c_int, c_int, c_int, *mut c_int) -> c_int = libc::socketpair;
        let _: unsafe extern "C" fn(c_int, *mut sockaddr, *mut socklen_t) -> c_int = c::getsockname;
        let _: unsafe extern "C" fn(c_int, *mut libc::sockaddr, *mut libc::socklen_t) -> c_int = libc::getsockname;
        let _: unsafe extern "C" fn(c_int, c_int, c_int, *mut core::ffi::c_void, *mut socklen_t) -> c_int =
            c::getsockopt;
        let _: unsafe extern "C" fn(c_int, c_int, c_int, *const core::ffi::c_void, socklen_t) -> c_int =
            c::setsockopt;
        let _: unsafe extern "C" fn(c_int, c_int, c_int) -> c_int = c::socket;
        let _: unsafe extern "C" fn(c_int, c_int, c_int) -> c_int = libc::socket;
        let _: unsafe extern "C" fn(c_int, c_int) -> c_int = c::listen;
        let _: unsafe extern "C" fn(c_int, c_int) -> c_int = libc::listen;
        let _: unsafe extern "C" fn(c_int, *const sockaddr, socklen_t) -> c_int = c::bind;
        let _: unsafe extern "C" fn(c_int, *const sockaddr, socklen_t) -> c_int = c::connect;
        let _: unsafe extern "C" fn(c_int, *const msghdr, c_int) -> isize = c::sendmsg;
        let _: unsafe extern "C" fn(c_int, *mut msghdr, c_int) -> isize = c::recvmsg;
    }

    /// Linux: two datagrams sent by one `sendmmsg` on a Unix datagram pair arrive by one
    /// `recvmmsg`, each with its length, and a descriptor sent by a whole-header `sendmsg`
    /// (`SCM_RIGHTS`) arrives by `recvmsg`; the kernel's calls, checked by libc's `fstat` of
    /// what arrived.
    #[cfg(target_os = "linux")]
    #[test]
    fn the_message_calls_round_trip() {
        use super::super::io::IoSliceMut;
        let [a, b] = socketpair(AF_UNIX, SOCK_DGRAM | SOCK_CLOEXEC, 0).expect("a datagram pair");
        let (mut one, mut two) = (*b"one", *b"second");
        let mut parts = [IoSliceMut::new(&mut one), IoSliceMut::new(&mut two)];
        let mut out = [mmsghdr::ZERO; 2];
        for (message, part) in out.iter_mut().zip(parts.iter_mut()) {
            message.msg_hdr.msg_iov = ptr::from_mut(part).cast();
            message.msg_hdr.msg_iovlen = 1;
        }
        // SAFETY: each header names one live part for the call.
        assert_eq!(unsafe { sendmmsg(a, &mut out, MsgFlags::NONE) }, Ok(2));
        let (mut room_one, mut room_two) = ([0u8; 16], [0u8; 16]);
        let mut rooms = [IoSliceMut::new(&mut room_one), IoSliceMut::new(&mut room_two)];
        let mut into = [mmsghdr::ZERO; 2];
        for (message, room) in into.iter_mut().zip(rooms.iter_mut()) {
            message.msg_hdr.msg_iov = ptr::from_mut(room).cast();
            message.msg_hdr.msg_iovlen = 1;
        }
        // SAFETY: each header names one live room held for the call.
        assert_eq!(unsafe { recvmmsg(b, &mut into, MsgFlags::NONE, None) }, Ok(2));
        assert_eq!((into[0].msg_len, into[1].msg_len), (3, 6));
        assert_eq!((&room_one[..3], &room_two[..6]), (&b"one"[..], &b"second"[..]));

        // A descriptor (`a` itself) passed by `SCM_RIGHTS`.
        #[repr(C, align(8))]
        struct Control([u8; 64]);
        let mut control = Control([0; 64]);
        let mut byte = [7u8];
        let mut part = IoSliceMut::new(&mut byte);
        let mut message = msghdr::ZERO;
        message.msg_iov = ptr::from_mut(&mut part).cast();
        message.msg_iovlen = 1;
        message.msg_control = control.0.as_mut_ptr().cast();
        message.msg_controllen = CMSG_SPACE(size_of::<c_int>() as c_uint) as usize;
        let header = CMSG_FIRSTHDR(&message);
        assert!(!header.is_null());
        // SAFETY: the header and its `int` of data lie within `control`, as `CMSG_SPACE` sized.
        unsafe {
            (*header).cmsg_len = CMSG_LEN(size_of::<c_int>() as c_uint) as usize;
            (*header).cmsg_level = SOL_SOCKET;
            (*header).cmsg_type = SCM_RIGHTS;
            CMSG_DATA(header).cast::<c_int>().write_unaligned(a.raw());
        }
        // SAFETY: the part and the control buffer are live for the call.
        assert_eq!(unsafe { sendmsg(a, &message, MsgFlags::NONE) }, Ok(1));
        let mut got_control = Control([0; 64]);
        let mut got_byte = [0u8];
        let mut got_part = IoSliceMut::new(&mut got_byte);
        let mut got = msghdr::ZERO;
        got.msg_iov = ptr::from_mut(&mut got_part).cast();
        got.msg_iovlen = 1;
        got.msg_control = got_control.0.as_mut_ptr().cast();
        got.msg_controllen = got_control.0.len();
        // SAFETY: the part and the control buffer are live and held for the call.
        assert_eq!(unsafe { recvmsg(b, &mut got, MsgFlags::CMSG_CLOSE_ON_EXEC) }, Ok(1));
        assert_eq!(got_byte, [7]);
        let header = CMSG_FIRSTHDR(&got);
        assert!(!header.is_null());
        // SAFETY: the kernel wrote one `SCM_RIGHTS` header and its `int` into `got_control`.
        let passed = unsafe {
            assert_eq!(((*header).cmsg_level, (*header).cmsg_type), (SOL_SOCKET, SCM_RIGHTS));
            CMSG_DATA(header).cast::<c_int>().read_unaligned()
        };
        // SAFETY: an all-zero `stat` is one; `fstat` writes it for each descriptor.
        let (mut ours, mut theirs): (libc::stat, libc::stat) = unsafe { (core::mem::zeroed(), core::mem::zeroed()) };
        // SAFETY: both are open descriptors of this test and `stat`s live for the calls.
        unsafe {
            assert_eq!(libc::fstat(a.raw(), &mut ours), 0);
            assert_eq!(libc::fstat(passed, &mut theirs), 0);
        }
        assert_eq!((ours.st_dev, ours.st_ino), (theirs.st_dev, theirs.st_ino), "the same socket arrived");
        // SAFETY: the three descriptors are this test's own.
        unsafe {
            libc::close(passed);
            libc::close(a.raw());
            libc::close(b.raw());
        }
    }

    /// `CMSG_LEN` and `CMSG_SPACE` are libc's for every small length, and a walk of a buffer
    /// of three messages finds the headers libc's walk finds.
    #[test]
    fn cmsg_functions_are_libc_s() {
        for len in 0..64 {
            // SAFETY: libc's forms are `unsafe` only by declaration: arithmetic.
            let (libc_len, libc_space) = unsafe { (libc::CMSG_LEN(len), libc::CMSG_SPACE(len)) };
            assert_eq!(CMSG_LEN(len), libc_len, "CMSG_LEN({len})");
            assert_eq!(CMSG_SPACE(len), libc_space, "CMSG_SPACE({len})");
        }
        #[repr(C, align(8))]
        struct Buffer([u8; 256]);
        let mut buffer = Buffer([0; 256]);
        let data_lens = [4u32, 20, 1];
        let used: u32 = data_lens.iter().map(|&len| CMSG_SPACE(len)).sum();
        let mut ours = msghdr::ZERO;
        ours.msg_control = buffer.0.as_mut_ptr().cast();
        ours.msg_controllen = used as _;
        // SAFETY: each header is written where the walk said it fits within `buffer`.
        unsafe {
            let mut at = CMSG_FIRSTHDR(&ours);
            for &len in &data_lens {
                assert!(!at.is_null());
                (*at).cmsg_len = CMSG_LEN(len) as _;
                (*at).cmsg_level = IPPROTO_IP;
                (*at).cmsg_type = IP_TOS;
                at = CMSG_NXTHDR(&ours, at);
            }
            assert!(at.is_null(), "a fourth header past the buffer's used length");
        }
        // SAFETY: an all-zero `msghdr` is one; its control is set to the same buffer.
        let mut theirs: libc::msghdr = unsafe { core::mem::zeroed() };
        theirs.msg_control = buffer.0.as_mut_ptr().cast();
        theirs.msg_controllen = used as _;
        // SAFETY: both walk the same live buffer of headers written above.
        unsafe {
            let (mut a, mut b) = (CMSG_FIRSTHDR(&ours), libc::CMSG_FIRSTHDR(&theirs));
            while !b.is_null() {
                assert_eq!(a as usize, b as usize);
                assert_eq!(CMSG_DATA(a) as usize, libc::CMSG_DATA(b.cast()) as usize);
                a = CMSG_NXTHDR(&ours, a);
                b = libc::CMSG_NXTHDR(&theirs, b);
            }
            assert!(a.is_null());
        }
    }

    /// A pair of stream sockets: its kind reads back, bytes sent arrive, and an `int`
    /// option set reads back.
    #[test]
    fn a_socket_pair_round_trips() {
        let [a, b] = socketpair(AF_UNIX, SOCK_STREAM, 0).expect("a socket pair");
        assert_eq!(getsockopt_int(a, SOL_SOCKET, SO_TYPE), Ok(SOCK_STREAM));
        assert_eq!(pending_error(a), Ok(None));
        assert_eq!(send(a, b"hello", MsgFlags::NO_SIGNAL), Ok(5));
        let mut into = [0u8; 8];
        assert_eq!(recv(b, &mut into, MsgFlags::NONE), Ok(5));
        assert_eq!(&into[..5], b"hello");
        setsockopt_int(a, SOL_SOCKET, SO_SNDBUF, 65536).expect("SO_SNDBUF");
        assert!(getsockopt_int(a, SOL_SOCKET, SO_SNDBUF).expect("SO_SNDBUF") >= 65536);
        #[cfg(target_os = "macos")]
        {
            assert_eq!(peer_pid(b), Ok(std::process::id() as pid_t));
            // SAFETY: `geteuid` has no preconditions.
            assert_eq!(peer_ids(b).map(|(uid, _)| uid), Ok(unsafe { libc::geteuid() }));
            assert!(peer_token(b).is_ok());
        }
        #[cfg(target_os = "linux")]
        assert_eq!(peer_cred(b).map(|cred| cred.pid as u32), Ok(std::process::id()));
        // SAFETY: both descriptors are this test's own.
        unsafe {
            libc::close(a.raw());
            libc::close(b.raw());
        }
    }

    /// A datagram socket bound to the loopback's port 0 reads back an address of its family
    /// on the loopback, with the port the system chose.
    #[test]
    fn a_bound_socket_names_its_address() {
        let socket = socket(AF_INET, SOCK_DGRAM, 0).expect("a socket");
        let mut at = sockaddr_storage::ZERO;
        {
            // SAFETY: a `sockaddr_storage` is larger and more aligned than a `sockaddr_in`.
            let v4 = unsafe { &mut *ptr::from_mut(&mut at).cast::<sockaddr_in>() };
            #[cfg(target_os = "macos")]
            {
                v4.sin_len = size_of::<sockaddr_in>() as u8;
            }
            v4.sin_family = AF_INET as sa_family_t;
            v4.sin_addr = in_addr { s_addr: u32::from_ne_bytes([127, 0, 0, 1]) };
        }
        bind(socket, &at, size_of::<sockaddr_in>() as socklen_t).expect("the bind");
        let (named, len) = getsockname(socket).expect("its name");
        assert_eq!(len as usize, size_of::<sockaddr_in>());
        assert_eq!(named.ss_family as c_int, AF_INET);
        // SAFETY: as above.
        let v4 = unsafe { &*ptr::from_ref(&named).cast::<sockaddr_in>() };
        assert_eq!(v4.sin_addr.s_addr.to_ne_bytes(), [127, 0, 0, 1]);
        assert_ne!(v4.sin_port, 0);
        assert_eq!(bind(socket, &at, 129), Err(Errno::Inval), "a length past the storage");
        // SAFETY: the descriptor is this test's own.
        unsafe { libc::close(socket.raw()) };
    }
}
