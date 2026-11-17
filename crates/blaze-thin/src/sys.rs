//! The one system call the thin `blaze` makes that std has no safe form of: a descriptor kept
//! open across the `exec` by which this process becomes the engine. The program's one unsafe
//! module.

use std::{
    io,
    os::fd::{AsFd, AsRawFd as _},
};

/// Has `fd` stay open in the program this process becomes: its close-on-exec flag is cleared
/// (std sets it on every descriptor it opens). Called only right before the `exec` that hands
/// the descriptor over, so no other program this process starts gets it: it starts none.
pub(crate) fn keep_across_exec(fd: &impl AsFd) -> io::Result<()> {
    // SAFETY: `fd` is borrowed, so it is open for the call, and `F_SETFD` with no flags reads
    // and writes no memory of this process: it clears the descriptor's one flag.
    let set = unsafe { libc::fcntl(fd.as_fd().as_raw_fd(), libc::F_SETFD, 0) };
    match set {
        -1 => Err(io::Error::last_os_error()),
        _ => Ok(()),
    }
}
