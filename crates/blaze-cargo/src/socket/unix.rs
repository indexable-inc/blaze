//! Whose this process and the peer of a socket are on Unix: by user id. std has neither
//! (`UnixStream::peer_cred` is unstable), so each is the one system call that asks.

use std::{io, os::fd::AsRawFd as _, os::unix::net::UnixStream};

/// The effective user of this process.
pub(super) fn user() -> u32 {
    // SAFETY: `geteuid` takes nothing, touches no memory of the caller's and can't fail.
    unsafe { libc::geteuid() }
}

/// The user at the other end of `stream`.
#[cfg(any(target_os = "linux", target_os = "android"))]
pub(super) fn peer_user(stream: &UnixStream) -> io::Result<u32> {
    let mut peer = libc::ucred { pid: 0, uid: 0, gid: 0 };
    let mut len = size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: `SO_PEERCRED` writes a `ucred`, at most `len` bytes of it; `peer` is a live,
    // writable `ucred` and `len` its size, so the call writes no further than `peer`. A `len`
    // larger than `peer` would have it write past its end. The descriptor is `stream`'s, open
    // for as long as the borrow.
    let failed = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&raw mut peer).cast(),
            &raw mut len,
        )
    };
    if failed != 0 { Err(io::Error::last_os_error()) } else { Ok(peer.uid) }
}

/// The user at the other end of `stream`.
#[cfg(not(any(target_os = "linux", target_os = "android")))]
pub(super) fn peer_user(stream: &UnixStream) -> io::Result<u32> {
    let (mut uid, mut gid) = (0, 0);
    // SAFETY: `uid` and `gid` are live, writable integers of the types `getpeereid` writes,
    // and the descriptor is `stream`'s, open for as long as the borrow.
    let failed = unsafe { libc::getpeereid(stream.as_raw_fd(), &raw mut uid, &raw mut gid) };
    if failed != 0 { Err(io::Error::last_os_error()) } else { Ok(uid) }
}
