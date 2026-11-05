//! macOS's waits: `kevent` for the kqueue driver and `mach_wait_until` for a sleep.

use std::{convert::Infallible, io, os::fd::RawFd};

use super::{Clock, Deadline, Once, WaitError, Waited, unix::timespec, until};

/// `struct mach_timebase_info`: mach time units are `numer / denom` nanoseconds.
#[repr(C)]
#[derive(Default)]
struct Timebase {
    numer: u32,
    denom: u32,
}

// Mach's clock and its absolute wait (rule macos-private-apis). `mach_wait_until` blocks the
// thread until a time on `mach_absolute_time`'s clock, so the kernel holds the deadline itself
// and nothing restarts a relative wait; `nanosleep`, the public sleep, takes a length and hands
// back a remainder to restart from. Resolved at run time by the test below, so an OS that
// drops it fails CI naming it.
unsafe extern "C" {
    fn mach_absolute_time() -> u64;
    fn mach_timebase_info(info: *mut Timebase) -> libc::c_int;
    fn mach_wait_until(deadline: u64) -> libc::c_int;
}

/// `KERN_ABORTED`: the wait was interrupted.
const KERN_ABORTED: libc::c_int = 14;

/// One `kevent` on `kq`: the `nchanges` changes at `changes` made, then up to `room` events
/// written at `events`, waiting for the first until `deadline` on `clock` (forever for
/// `None`). The changes are made by the first call only: the kernel makes them before it
/// waits, interrupted or not, so a retry passes none. `Ready` with how many events came;
/// with `room` 0 the call never waits and is `Ready(0)` once the changes are made.
///
/// # Safety
///
/// `changes` points at `nchanges` `kevent`s to read and `events` at room for `room` to write,
/// both valid until this returns.
pub(crate) unsafe fn kevent(
    what: &'static str,
    kq: RawFd,
    changes: *const libc::kevent,
    nchanges: usize,
    events: *mut libc::kevent,
    room: usize,
    deadline: Option<Deadline>,
    clock: &impl Clock,
) -> Result<Waited<usize>, WaitError> {
    let too_many =
        || WaitError::io(what, io::Error::new(io::ErrorKind::InvalidInput, "too many kevents"));
    let mut nchanges = libc::c_int::try_from(nchanges).map_err(|_| too_many())?;
    let room = libc::c_int::try_from(room).map_err(|_| too_many())?;
    until(what, clock, deadline, |left| {
        let timeout = left.map(timespec);
        let timeout = timeout.as_ref().map_or(std::ptr::null(), std::ptr::from_ref);
        // SAFETY: the caller's changes and room, and a timeout that outlives the call.
        let got = unsafe { kevent_once(kq, changes, nchanges, events, room, timeout) };
        nchanges = 0;
        if got > 0 || (got == 0 && room == 0) {
            return Ok(Once::Ready(got as usize));
        }
        if got == 0 {
            return Ok(Once::TimedOut);
        }
        let error = io::Error::last_os_error();
        match error.raw_os_error() {
            Some(libc::EINTR) => Ok(Once::Interrupted),
            _ => Err(error),
        }
    })
}

/// One `kevent` call.
///
/// # Safety
///
/// As [`kevent`]'s, and `timeout` is null or a timespec valid for the call.
#[expect(
    clippy::disallowed_methods,
    reason = "the one `kevent` of `wait::kevent`, its timeout recomputed from the deadline"
)]
unsafe fn kevent_once(
    kq: RawFd,
    changes: *const libc::kevent,
    nchanges: libc::c_int,
    events: *mut libc::kevent,
    room: libc::c_int,
    timeout: *const libc::timespec,
) -> libc::c_int {
    // SAFETY: the caller's pointers, valid for the counts given.
    unsafe { libc::kevent(kq, changes, nchanges, events, room, timeout) }
}

/// Sleeps until `deadline` on `clock`. Each call is a `mach_wait_until` to what is left on
/// `clock` from Mach's now, an absolute time the kernel holds, so an interrupted sleep sleeps
/// again for only what is left.
pub fn sleep_until(
    what: &'static str,
    deadline: Deadline,
    clock: &impl Clock,
) -> Result<(), WaitError> {
    let mut base = Timebase::default();
    // SAFETY: a struct for the kernel to write.
    let got = unsafe { mach_timebase_info(&mut base) };
    if got != 0 || base.numer == 0 || base.denom == 0 {
        return Err(WaitError::io(
            what,
            // alloc: an error.
            io::Error::other(format!(
                "mach_timebase_info returned {got} with {}/{}",
                base.numer, base.denom
            )),
        ));
    }
    let waited = until(what, clock, Some(deadline), |left| -> io::Result<Once<Infallible>> {
        let ns = left.unwrap_or_default().as_nanos();
        let ticks = ns.saturating_mul(u128::from(base.denom)).div_ceil(u128::from(base.numer));
        // SAFETY: a read of Mach's clock.
        let now = unsafe { mach_absolute_time() };
        let at = now.saturating_add(u64::try_from(ticks).unwrap_or(u64::MAX));
        // SAFETY: blocks this thread until `at`; no memory is passed.
        match unsafe { mach_wait_until(at) } {
            0 => Ok(Once::TimedOut),
            KERN_ABORTED => Ok(Once::Interrupted),
            // alloc: an error.
            error => Err(io::Error::other(format!("mach_wait_until returned {error}"))),
        }
    })?;
    match waited {
        Waited::Ready(never) => match never {},
        Waited::Deadline => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    /// The Mach calls the sleep is built on resolve at run time (rule macos-private-apis): an
    /// OS update that removes one fails here, naming it.
    #[test]
    fn the_mach_calls_of_a_sleep_resolve() {
        for name in [c"mach_wait_until", c"mach_absolute_time", c"mach_timebase_info"] {
            // SAFETY: a lookup of a NUL-terminated name in every image loaded.
            let found = unsafe { libc::dlsym(libc::RTLD_DEFAULT, name.as_ptr()) };
            assert!(
                !found.is_null(),
                "{name:?} is gone from this macOS: wait::sleep_until needs it"
            );
        }
    }
}
