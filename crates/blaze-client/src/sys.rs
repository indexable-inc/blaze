//! The three system calls the client makes that std has no safe form of: a write to the
//! daemon's socket that raises no `SIGPIPE` in the process that holds the client (a host of the
//! C ABI may not ignore it, as a Rust program does), and a descriptor inherited by one child.
//! The crate's one unsafe module.

use std::{
    io,
    os::{
        fd::{AsRawFd as _, RawFd},
        unix::net::UnixStream,
    },
};

/// Has a write to `stream` whose peer is gone fail with `EPIPE` and raise no signal, where
/// that is the socket's to say (`SO_NOSIGPIPE`, macOS and the BSDs); on Linux each
/// [`send`] says it.
pub(crate) fn no_sigpipe(stream: &UnixStream) -> io::Result<()> {
    #[cfg(any(target_os = "macos", target_os = "ios", target_os = "freebsd"))]
    {
        let on: libc::c_int = 1;
        // SAFETY: `stream` is an open socket for the call; the option's value is `on`, which
        // lives across it, and its length is `on`'s size.
        let set = unsafe {
            libc::setsockopt(
                stream.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_NOSIGPIPE,
                std::ptr::from_ref(&on).cast(),
                size_of::<libc::c_int>() as libc::socklen_t,
            )
        };
        if set != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    let _ = stream;
    Ok(())
}

/// One write of `bytes` to `stream`, which is non-blocking: how many it took, `WouldBlock`
/// when it has no room, `BrokenPipe` (and no signal) when the daemon is gone.
pub(crate) fn send(stream: &UnixStream, bytes: &[u8]) -> io::Result<usize> {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    const FLAGS: libc::c_int = libc::MSG_NOSIGNAL;
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    const FLAGS: libc::c_int = 0;
    // SAFETY: `stream` is an open socket for the call, and `bytes` is readable for its length,
    // which is all `send` reads.
    let sent = unsafe { libc::send(stream.as_raw_fd(), bytes.as_ptr().cast(), bytes.len(), FLAGS) };
    match usize::try_from(sent) {
        Ok(sent) => Ok(sent),
        Err(_) => Err(io::Error::last_os_error()),
    }
}

/// Has the process `command` starts inherit `fd` under its own number: it is cleared of
/// close-on-exec in the child alone, so no other process this one starts gets it. As
/// blaze-cargo's `raw::process::inherit_in_child`, which the CLI starts the daemon with.
pub(crate) fn inherit_in_child(command: &mut std::process::Command, fd: RawFd) {
    use std::os::unix::process::CommandExt as _;
    // SAFETY: runs in the child between its fork and its exec, and calls only `fcntl`, which
    // is async-signal-safe and touches no memory; `fd` is open there, the child's copy of the
    // parent's, as the caller keeps it open until the spawn.
    unsafe {
        command.pre_exec(move || match libc::fcntl(fd, libc::F_SETFD, 0) {
            -1 => Err(io::Error::last_os_error()),
            _ => Ok(()),
        })
    };
}

/// The columns of the terminal stderr is, as the system says (`TIOCGWINSZ`); `None` when it
/// is none or says no width. As blaze-cargo's `cli::place::stderr_columns`.
pub(crate) fn stderr_columns() -> Option<u16> {
    // SAFETY: `winsize` is plain integers, for which zeroes are a value.
    let mut size: libc::winsize = unsafe { std::mem::zeroed() };
    // SAFETY: descriptor 2 is this process's stderr, or closed (then the call fails); the
    // request writes one `winsize` at the pointer, which is `size`, alive across the call.
    let asked = unsafe { libc::ioctl(libc::STDERR_FILENO, libc::TIOCGWINSZ, &raw mut size) };
    (asked == 0 && size.ws_col > 0).then_some(size.ws_col)
}
