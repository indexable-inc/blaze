//! `poll`, the wait on descriptors of every Unix.

use blaze_sys::{Errno, Timespec, posix::{io::{PollFd, poll}, types::nfds_t}};
use std::{io, time::Duration};

use super::{Clock, Deadline, Once, WaitError, Waited, until};

/// Waits for one of `fds` to be ready for its `events`, until `deadline` on `clock` (forever
/// for `None`): how many are ready, their `revents` set. With [`Deadline::PASSED`] it makes
/// exactly one `poll` with a timeout of 0, which looks and never blocks.
pub fn readable(
    what: &'static str,
    fds: &mut [libc::pollfd],
    deadline: Option<Deadline>,
    clock: &impl Clock,
) -> Result<Waited<usize>, WaitError> {
    nfds_t::try_from(fds.len()).map_err(|_| {
        WaitError::io(what, io::Error::new(io::ErrorKind::InvalidInput, "too many descriptors"))
    })?;
    // SAFETY: both pollfd types are repr(C) with the same int fd and short events and
    // revents, in order. Every bit pattern is valid. The slice stays exclusively borrowed.
    let fds = unsafe { core::slice::from_raw_parts_mut(fds.as_mut_ptr().cast::<PollFd>(), fds.len()) };
    until(what, clock, deadline, |left| {
        let timeout = left.map(|left| {
            // Rounded up, so a wait shorter than a millisecond is not a spin.
            let ms = core::ffi::c_int::try_from(left.as_nanos().div_ceil(1_000_000)).unwrap_or(core::ffi::c_int::MAX);
            Timespec::from_duration(Duration::from_millis(ms as u64))
        });
        match poll(fds, timeout) {
            Ok(0) => Ok(Once::TimedOut),
            Ok(ready) => Ok(Once::Ready(ready)),
            Err(Errno::Intr) => Ok(Once::Interrupted),
            // The kernel could not allocate for the call this time.
            Err(Errno::Again) => Ok(Once::Spurious),
            Err(error) => Err(io::Error::from_raw_os_error(i32::from(error.raw()))),
        }
    })
}

