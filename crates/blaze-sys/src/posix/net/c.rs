//! macOS's socket calls in libSystem, raw: each as its header declares it, for the typed
//! forms in [`super`] (prefer them). The module exists on macOS only: on Linux blaze-sys names
//! no C-library symbol (D180), and every call is the kernel's, in `crate::linux::net` and
//! `crate::linux::sock`.
//!
//! The calls already in `crate::macos::cold` (bound at first use) are re-exported from there,
//! never declared twice; the rest are declared against libSystem.

use core::ffi::c_int;

use super::super::types::socklen_t;
use super::{msghdr, sockaddr};

/// sys/socket.h, through macOS's table of calls found at first use.
/// replaces libc::getsockopt (macOS)
/// replaces libc::setsockopt (macOS)
/// replaces libc::send (macOS)
/// replaces libc::recv (macOS)
pub use crate::macos::cold::{getsockopt, recv, send, setsockopt};

unsafe extern "C" {
    /// `int socket(int domain, int type, int protocol)` (sys/socket.h).
    /// replaces libc::socket (macOS)
    pub fn socket(domain: c_int, kind: c_int, protocol: c_int) -> c_int;
    /// `int socketpair(int domain, int type, int protocol, int sv[2])` (sys/socket.h).
    /// replaces libc::socketpair (macOS)
    pub fn socketpair(domain: c_int, kind: c_int, protocol: c_int, ends: *mut c_int) -> c_int;
    /// `int bind(int socket, const struct sockaddr *address, socklen_t address_len)`
    /// (sys/socket.h).
    /// replaces libc::bind (macOS)
    pub fn bind(socket: c_int, address: *const sockaddr, len: socklen_t) -> c_int;
    /// `int connect(int socket, const struct sockaddr *address, socklen_t address_len)`
    /// (sys/socket.h).
    /// replaces libc::connect (macOS)
    pub fn connect(socket: c_int, address: *const sockaddr, len: socklen_t) -> c_int;
    /// `int listen(int socket, int backlog)` (sys/socket.h).
    /// replaces libc::listen (macOS)
    pub fn listen(socket: c_int, backlog: c_int) -> c_int;
    /// `int getsockname(int socket, struct sockaddr *address, socklen_t *address_len)`
    /// (sys/socket.h).
    /// replaces libc::getsockname (macOS)
    pub fn getsockname(socket: c_int, address: *mut sockaddr, len: *mut socklen_t) -> c_int;
    /// `int getpeereid(int socket, uid_t *euid, gid_t *egid)` (unistd.h).
    /// replaces libc::getpeereid
    pub fn getpeereid(
        socket: c_int,
        uid: *mut super::super::types::uid_t,
        gid: *mut super::super::types::gid_t,
    ) -> c_int;
    /// `ssize_t sendmsg(int socket, const struct msghdr *message, int flags)`
    /// (sys/socket.h): the form with an address and control messages.
    /// replaces libc::sendmsg (macOS)
    pub fn sendmsg(socket: c_int, message: *const msghdr, flags: c_int) -> isize;
    /// `ssize_t recvmsg(int socket, struct msghdr *message, int flags)` (sys/socket.h).
    /// replaces libc::recvmsg (macOS)
    pub fn recvmsg(socket: c_int, message: *mut msghdr, flags: c_int) -> isize;
}
