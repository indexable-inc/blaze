//! Windows' waits: `GetQueuedCompletionStatusEx` for the IOCP driver, `WaitForSingleObject`
//! for one handle, and a sleep.

use std::{convert::Infallible, io, time::Duration};

use windows_sys::Win32::{
    Foundation::{GetLastError, HANDLE, WAIT_ABANDONED, WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT},
    System::{
        IO::{GetQueuedCompletionStatusEx, OVERLAPPED_ENTRY},
        Threading::{INFINITE, Sleep, WaitForSingleObject},
    },
};

use super::{Clock, Deadline, Once, WaitError, Waited, until};

/// A wait's time left in milliseconds, rounded up so a wait shorter than a millisecond is not
/// a spin, below `INFINITE`; `INFINITE` for no limit.
fn ms(left: Option<Duration>) -> u32 {
    match left {
        None => INFINITE,
        Some(left) => {
            let ms = u32::try_from(left.as_nanos().div_ceil(1_000_000));
            ms.map_or(INFINITE - 1, |ms| ms.min(INFINITE - 1))
        }
    }
}

/// Up to `entries.len()` packets of the completion port `port`, waiting for the first until
/// `deadline` on `clock` (forever for `None`): `Ready` with how many came.
pub(crate) fn completions(
    what: &'static str,
    port: HANDLE,
    entries: &mut [OVERLAPPED_ENTRY],
    deadline: Option<Deadline>,
    clock: &impl Clock,
) -> Result<Waited<usize>, WaitError> {
    let room = u32::try_from(entries.len()).unwrap_or(u32::MAX);
    until(what, clock, deadline, |left| {
        let mut got = 0;
        let ok = take(port, entries, room, &mut got, ms(left));
        if ok != 0 {
            return Ok(match got {
                0 => Once::Spurious,
                got => Once::Ready(got as usize),
            });
        }
        // SAFETY: reads this thread's last error, which the call just set.
        match unsafe { GetLastError() } {
            WAIT_TIMEOUT => Ok(Once::TimedOut),
            _ => Err(io::Error::last_os_error()),
        }
    })
}

/// One `GetQueuedCompletionStatusEx`, not alertable (so no APC ends it).
#[expect(
    clippy::disallowed_methods,
    reason = "the one take of `wait::completions`, its timeout recomputed from the deadline"
)]
fn take(port: HANDLE, entries: &mut [OVERLAPPED_ENTRY], room: u32, got: &mut u32, ms: u32) -> i32 {
    // SAFETY: room for `room` entries (at most `entries.len()`) and a count to write; a port
    // that is not open is an error the call returns.
    unsafe { GetQueuedCompletionStatusEx(port, entries.as_mut_ptr(), room, got, ms, 0) }
}

/// Waits for `handle` (a process, an event) to be signaled, until `deadline` on `clock`
/// (forever for `None`). An abandoned mutex counts as signaled.
pub fn signaled(
    what: &'static str,
    handle: HANDLE,
    deadline: Option<Deadline>,
    clock: &impl Clock,
) -> Result<Waited<()>, WaitError> {
    until(what, clock, deadline, |left| match wait_one(handle, ms(left)) {
        WAIT_OBJECT_0 | WAIT_ABANDONED => Ok(Once::Ready(())),
        WAIT_TIMEOUT => Ok(Once::TimedOut),
        WAIT_FAILED => Err(io::Error::last_os_error()),
        // alloc: an error.
        other => Err(io::Error::other(format!("WaitForSingleObject returned {other:#x}"))),
    })
}

/// One `WaitForSingleObject`.
#[expect(
    clippy::disallowed_methods,
    reason = "the one wait of `wait::signaled`, its timeout recomputed from the deadline"
)]
fn wait_one(handle: HANDLE, ms: u32) -> u32 {
    // SAFETY: a handle of the caller's; one that is not open is an error the call returns.
    unsafe { WaitForSingleObject(handle, ms) }
}

/// Sleeps until `deadline` on `clock`, each call a `Sleep` of what is left, rounded up to a
/// millisecond; one that ends before the deadline on `clock` sleeps again for what is left.
pub fn sleep_until(
    what: &'static str,
    deadline: Deadline,
    clock: &impl Clock,
) -> Result<(), WaitError> {
    let waited = until(what, clock, Some(deadline), |left| -> io::Result<Once<Infallible>> {
        sleep(ms(Some(left.unwrap_or_default())));
        Ok(Once::TimedOut)
    })?;
    match waited {
        Waited::Ready(never) => match never {},
        Waited::Deadline => Ok(()),
    }
}

/// One `Sleep`.
#[expect(
    clippy::disallowed_methods,
    reason = "the one sleep of `wait::sleep_until`, its length recomputed from the deadline"
)]
fn sleep(ms: u32) {
    // SAFETY: blocks this thread; no memory is passed.
    unsafe { Sleep(ms) }
}
