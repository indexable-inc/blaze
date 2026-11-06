//! `poll`, the wait on descriptors of every Unix.

use std::io;

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
    let n = libc::nfds_t::try_from(fds.len()).map_err(|_| {
        WaitError::io(what, io::Error::new(io::ErrorKind::InvalidInput, "too many descriptors"))
    })?;
    until(what, clock, deadline, |left| {
        let ms = match left {
            None => -1,
            // Rounded up, so a wait shorter than a millisecond is not a spin.
            Some(left) => libc::c_int::try_from(left.as_nanos().div_ceil(1_000_000))
                .unwrap_or(libc::c_int::MAX),
        };
        let ready = poll(fds, n, ms);
        if ready > 0 {
            return Ok(Once::Ready(ready as usize));
        }
        if ready == 0 {
            return Ok(Once::TimedOut);
        }
        let error = io::Error::last_os_error();
        match error.raw_os_error() {
            Some(libc::EINTR) => Ok(Once::Interrupted),
            // The kernel could not allocate for the call this time.
            Some(libc::EAGAIN) => Ok(Once::Spurious),
            _ => Err(error),
        }
    })
}

/// One `poll` of `fds` (`n` of them) for up to `ms` milliseconds (-1: no limit).
#[expect(
    clippy::disallowed_methods,
    reason = "the one `poll` of `wait::readable`, its timeout recomputed from the deadline"
)]
fn poll(fds: &mut [libc::pollfd], n: libc::nfds_t, ms: libc::c_int) -> libc::c_int {
    // SAFETY: `fds` is a live, exclusive slice of `n` pollfds; the kernel writes only their
    // `revents` and keeps no pointer after the call returns.
    unsafe { libc::poll(fds.as_mut_ptr(), n, ms) }
}

/// A relative wait in the shape a `timespec` takes, saturating at its end.
#[cfg(target_os = "macos")]
pub(super) fn timespec(left: std::time::Duration) -> libc::timespec {
    libc::timespec {
        tv_sec: libc::time_t::try_from(left.as_secs()).unwrap_or(libc::time_t::MAX),
        tv_nsec: left.subsec_nanos().into(),
    }
}
