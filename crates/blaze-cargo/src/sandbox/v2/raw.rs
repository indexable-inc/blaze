//! What the launcher asks of a descriptor it was given by number, which std has no call for
//! without owning it (`fstat`, `fcntl(F_GETFL)`): asking changes nothing of the descriptor,
//! whoever owns it, so the function is safe for any number.

/// The file type (`st_mode & S_IFMT`) of what the descriptor numbered `fd` is open on, and
/// whether it is open for reading; `None` where `fd` is not open.
pub(super) fn opened(fd: libc::c_int) -> Option<(libc::mode_t, bool)> {
    // SAFETY: `libc::stat` is integers only, for which all-zero bytes are a valid value.
    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: `stat` is a live `struct stat` of this frame for `fstat` to fill, and the call
    // writes nothing else; a number that is not an open descriptor is an error return.
    if unsafe { libc::fstat(fd, &mut stat) } < 0 {
        return None;
    }
    // SAFETY: `fcntl(F_GETFL)` takes integers only, reads no memory of this process and
    // changes nothing of the descriptor.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    let readable = flags >= 0 && flags & libc::O_ACCMODE != libc::O_WRONLY;
    Some((stat.st_mode & libc::S_IFMT, readable))
}
